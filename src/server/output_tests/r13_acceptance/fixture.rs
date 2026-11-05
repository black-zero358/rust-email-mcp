//! Lazy two-account TLS IMAP data and an independently metered HTTPS issuer.
use super::{ACCOUNTS, PRINCIPALS, Startup};
use anyhow::{Result, ensure};
use aws_lc_rs::{
    encoding::{AsDer, Pkcs8V1Der},
    rsa::{KeyPair, KeySize},
    signature::KeyPair as _,
};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
};
use tokio_rustls::{
    TlsAcceptor,
    rustls::{
        ServerConfig,
        pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer},
    },
};

mod imap;
mod mime;

pub(super) fn inbox_validity() -> u32 {
    imap::validity(0, 0)
}

pub(super) struct Fixture {
    pub startup: Startup,
    pub certificate: Vec<u8>,
    imap: [Arc<imap::Imap>; 2],
    holds: Arc<imap::Holds>,
    issuer: Arc<Issuer>,
    inserted: Mutex<Vec<Value>>,
}

impl Fixture {
    pub async fn start() -> Result<Self> {
        let certificate =
            rcgen::generate_simple_self_signed(vec!["localhost".into(), "127.0.0.1".into()])?;
        let cert = certificate.cert.der().clone();
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
            certificate.signing_key.serialize_der(),
        ));
        let tls = TlsAcceptor::from(Arc::new(
            ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(vec![cert.clone()], key)?,
        ));
        let issuer = Issuer::start(tls.clone()).await?;
        let first = TcpListener::bind("127.0.0.1:0").await?;
        let second = TcpListener::bind("127.0.0.1:0").await?;
        let ports = [first.local_addr()?.port(), second.local_addr()?.port()];
        let holds = Arc::new(imap::Holds::default());
        let imap = [
            imap::Imap::start(0, first, tls.clone(), holds.clone()),
            imap::Imap::start(1, second, tls, holds.clone()),
        ];
        Ok(Self {
            startup: Startup {
                imap_ports: ports,
                issuer: issuer.url.clone(),
            },
            certificate: cert.as_ref().to_vec(),
            imap,
            holds,
            issuer,
            inserted: Mutex::default(),
        })
    }

    pub fn tokens(&self, audience: &str) -> Result<Value> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let sign = |subject| self.issuer.token(audience, subject, now);
        Ok(json!({"primary":sign(PRINCIPALS[0])?, "secondary":sign(PRINCIPALS[1])?}))
    }

    pub fn accounts(&self) -> Value {
        let counts = [imap::INBOX_COUNT, imap::ARCHIVE_COUNT];
        json!(ACCOUNTS.into_iter().enumerate().map(|(account, id)| {
            let mailboxes = imap::MAILBOXES.into_iter().zip(counts).enumerate().map(|(index, (mailbox, count))| {
                json!({
                    "mailbox": mailbox, "uid_validity": imap::validity(account, index),
                    "first_uid": 1, "last_uid": count, "message_count": count,
                    "expected_attachments": count / mime::ATTACHMENT_UID_STRIDE,
                })
            }).collect::<Vec<_>>();
            json!({"id":id,"principal":PRINCIPALS[account],"mailboxes":mailboxes,"message_count":counts.iter().sum::<u32>(),"cold_uid":1250,"hot_uid":1})
        }).collect::<Vec<_>>())
    }

    pub fn attachments(&self) -> Value {
        let mut cases = ACCOUNTS.into_iter().enumerate().flat_map(|(account, id)| {
            (1..=imap::DOWNLOADS_COUNT).map(move |uid| {
                let payload = mime::large(account, uid);
                let role = match (account, uid) { (_, 1 | 2) => "slow", (0, _) => "extra", _ => "boundary" };
                json!({"account_id":id,"mailbox":"Downloads","uid":uid,"uid_validity":imap::validity(account, 2),
                    "attachment_index":1,"bytes":payload.bytes,"sha256":payload.sha256(),"role":role})
            })
        }).collect::<Vec<_>>();
        let payload = mime::small(10);
        cases.push(json!({"account_id":ACCOUNTS[0],"mailbox":"INBOX","uid":10,"uid_validity":inbox_validity(),
            "attachment_index":2,"bytes":payload.bytes,"sha256":payload.sha256(),"role":"multilayer"}));
        json!(cases)
    }

    pub fn sample(&self) -> Value {
        json!({
            "imap": {ACCOUNTS[0]:self.imap[0].sample(), ACCOUNTS[1]:self.imap[1].sample()},
            "oidc":{"discovery":self.issuer.discovery.load(Ordering::Relaxed),"jwks":self.issuer.jwks_hits.load(Ordering::Relaxed)},
            "holds": self.holds.sample(), "inserted": *self.inserted.lock().unwrap(),
        })
    }

    pub fn arm(&self) -> Result<Value> {
        ensure!(self.holds.arm(), "An IMAP hold is already armed");
        Ok(self.holds.sample())
    }

    pub fn release(&self) -> Value {
        self.holds.release();
        self.holds.sample()
    }

    pub fn append_tail(&self) -> Value {
        let inserted = self.imap.each_ref().map(|imap| imap.append());
        let mut retained = self.inserted.lock().unwrap();
        retained.extend(inserted.iter().cloned());
        // The sample needs only the most recent insertion for each account.
        if retained.len() > ACCOUNTS.len() {
            retained.drain(..ACCOUNTS.len());
        }
        json!(inserted)
    }
}

struct Issuer {
    url: String,
    header: Header,
    signing: EncodingKey,
    jwks: Vec<u8>,
    discovery: AtomicU64,
    jwks_hits: AtomicU64,
}

impl Issuer {
    async fn start(tls: TlsAcceptor) -> Result<Arc<Self>> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("https://{}", listener.local_addr()?);
        let key = KeyPair::generate(KeySize::Rsa2048).expect("Generate synthetic R13 signing key");
        let public = key.public_key();
        let jwks = serde_json::to_vec(&json!({"keys":[{
            "kty":"RSA", "kid":"r13-offline", "alg":"RS256", "use":"sig",
            "n":URL_SAFE_NO_PAD.encode(public.modulus().big_endian_without_leading_zero()),
            "e":URL_SAFE_NO_PAD.encode(public.exponent().big_endian_without_leading_zero()),
        }]}))?;
        let der = AsDer::<Pkcs8V1Der>::as_der(&key).expect("Encode synthetic signing key");
        let pem = format!(
            "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
            STANDARD.encode(der.as_ref())
        );
        let signing = EncodingKey::from_rsa_pem(pem.as_bytes())?;
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("r13-offline".into());
        let issuer = Arc::new(Self {
            url,
            header,
            signing,
            jwks,
            discovery: AtomicU64::new(0),
            jwks_hits: AtomicU64::new(0),
        });
        let server = issuer.clone();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let tls = tls.clone();
                let issuer = server.clone();
                tokio::spawn(async move {
                    if let Ok(stream) = tls.accept(socket).await {
                        let _ = issuer.respond(BufReader::new(stream)).await;
                    }
                });
            }
        });
        Ok(issuer)
    }

    fn token(&self, audience: &str, principal: &str, now: u64) -> Result<String> {
        Ok(encode(
            &self.header,
            &json!({"sub":principal,"iss":self.url,"aud":audience,
            "exp":now + 90 * 60,"nbf":now.saturating_sub(1),"scope":"openid email:mcp","typ":"Bearer"}),
            &self.signing,
        )?)
    }

    async fn respond(
        &self,
        mut io: BufReader<tokio_rustls::server::TlsStream<tokio::net::TcpStream>>,
    ) -> std::io::Result<()> {
        let mut first = String::new();
        if io.read_line(&mut first).await? == 0 {
            return Ok(());
        }
        let mut line = String::new();
        loop {
            line.clear();
            if io.read_line(&mut line).await? == 0 || line == "\r\n" {
                break;
            }
        }
        let discovery;
        let (status, body) = if first.starts_with("GET /.well-known/openid-configuration ") {
            self.discovery.fetch_add(1, Ordering::Relaxed);
            discovery =
                json!({"issuer":self.url,"jwks_uri":format!("{}/jwks",self.url)}).to_string();
            ("200 OK", discovery.as_bytes())
        } else if first.starts_with("GET /jwks ") {
            self.jwks_hits.fetch_add(1, Ordering::Relaxed);
            ("200 OK", self.jwks.as_slice())
        } else {
            ("404 Not Found", b"{}".as_slice())
        };
        io.write_all(format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).as_bytes()).await?;
        io.write_all(body).await?;
        io.flush().await?;
        io.shutdown().await
    }
}
