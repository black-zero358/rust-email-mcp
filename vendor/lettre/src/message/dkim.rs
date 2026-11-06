use std::{
    borrow::Cow,
    error::Error as StdError,
    fmt::{self, Display},
    time::SystemTime,
};

use ed25519_dalek::Signer;
use rsa::{RsaPrivateKey, pkcs1::DecodeRsaPrivateKey, pkcs1v15::Pkcs1v15Sign};
use sha2::{Digest, Sha256};

use crate::message::{
    Headers, Message,
    header::{HeaderName, HeaderValue},
};

/// Describe Dkim Canonicalization to apply to either body or headers
#[derive(Copy, Clone, Debug)]
pub enum DkimCanonicalizationType {
    Simple,
    Relaxed,
}

impl Display for DkimCanonicalizationType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            DkimCanonicalizationType::Simple => "simple",
            DkimCanonicalizationType::Relaxed => "relaxed",
        })
    }
}

/// Describe Canonicalization to be applied before signing
#[derive(Copy, Clone, Debug)]
pub struct DkimCanonicalization {
    pub header: DkimCanonicalizationType,
    pub body: DkimCanonicalizationType,
}

impl Default for DkimCanonicalization {
    fn default() -> Self {
        DkimCanonicalization {
            header: DkimCanonicalizationType::Simple,
            body: DkimCanonicalizationType::Relaxed,
        }
    }
}

/// Format canonicalization to be shown in Dkim header
impl Display for DkimCanonicalization {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.header, self.body)
    }
}

/// Describe the algorithm used for signing the message
#[derive(Copy, Clone, Debug)]
pub enum DkimSigningAlgorithm {
    Rsa,
    Ed25519,
}

impl Display for DkimSigningAlgorithm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            DkimSigningAlgorithm::Rsa => "rsa",
            DkimSigningAlgorithm::Ed25519 => "ed25519",
        })
    }
}

/// Describe [`DkimSigningKey`] key error
#[derive(Debug)]
pub struct DkimSigningKeyError(InnerDkimSigningKeyError);

#[derive(Debug)]
enum InnerDkimSigningKeyError {
    Base64(base64::DecodeError),
    Rsa(rsa::pkcs1::Error),
    Ed25519(ed25519_dalek::ed25519::Error),
}

impl Display for DkimSigningKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match &self.0 {
            InnerDkimSigningKeyError::Base64(_err) => "base64 decode error",
            InnerDkimSigningKeyError::Rsa(_err) => "rsa decode error",
            InnerDkimSigningKeyError::Ed25519(_err) => "ed25519 decode error",
        })
    }
}

impl StdError for DkimSigningKeyError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        Some(match &self.0 {
            InnerDkimSigningKeyError::Base64(err) => err,
            InnerDkimSigningKeyError::Rsa(err) => err,
            InnerDkimSigningKeyError::Ed25519(err) => err,
        })
    }
}

/// Describe a signing key to be carried by [`DkimConfig`] struct
#[derive(Debug)]
pub struct DkimSigningKey(InnerDkimSigningKey);

#[derive(Debug)]
enum InnerDkimSigningKey {
    Rsa(RsaPrivateKey),
    Ed25519(ed25519_dalek::SigningKey),
}

impl DkimSigningKey {
    pub fn new(
        private_key: &str,
        algorithm: DkimSigningAlgorithm,
    ) -> Result<DkimSigningKey, DkimSigningKeyError> {
        Ok(Self(match algorithm {
            DkimSigningAlgorithm::Rsa => InnerDkimSigningKey::Rsa(
                RsaPrivateKey::from_pkcs1_pem(private_key)
                    .map_err(|err| DkimSigningKeyError(InnerDkimSigningKeyError::Rsa(err)))?,
            ),
            DkimSigningAlgorithm::Ed25519 => {
                InnerDkimSigningKey::Ed25519(ed25519_dalek::SigningKey::from_bytes(
                    &crate::base64::decode(private_key)
                        .map_err(|err| DkimSigningKeyError(InnerDkimSigningKeyError::Base64(err)))?
                        .try_into()
                        .map_err(|_| {
                            DkimSigningKeyError(InnerDkimSigningKeyError::Ed25519(
                                ed25519_dalek::ed25519::Error::new(),
                            ))
                        })?,
                ))
            }
        }))
    }
    fn get_signing_algorithm(&self) -> DkimSigningAlgorithm {
        match self.0 {
            InnerDkimSigningKey::Rsa(_) => DkimSigningAlgorithm::Rsa,
            InnerDkimSigningKey::Ed25519(_) => DkimSigningAlgorithm::Ed25519,
        }
    }
}

/// A struct to describe Dkim configuration applied when signing a message
#[derive(Debug)]
pub struct DkimConfig {
    /// The name of the key published in DNS
    selector: String,
    /// The domain for which we sign the message
    domain: String,
    /// The private key in PKCS1 string format
    private_key: DkimSigningKey,
    /// A list of header names to be included in the signature. Signing of more than one
    /// header with the same name is not supported
    headers: Vec<HeaderName>,
    /// The signing algorithm to be used when signing
    canonicalization: DkimCanonicalization,
}

impl DkimConfig {
    /// Create a default signature configuration with a set of headers and "simple/relaxed"
    /// canonicalization
    pub fn default_config(
        selector: String,
        domain: String,
        private_key: DkimSigningKey,
    ) -> DkimConfig {
        DkimConfig {
            selector,
            domain,
            private_key,
            headers: vec![
                HeaderName::new_from_ascii_str("From"),
                HeaderName::new_from_ascii_str("Subject"),
                HeaderName::new_from_ascii_str("To"),
                HeaderName::new_from_ascii_str("Date"),
            ],
            canonicalization: DkimCanonicalization {
                header: DkimCanonicalizationType::Simple,
                body: DkimCanonicalizationType::Relaxed,
            },
        }
    }

    /// Create a [`DkimConfig`]
    pub fn new(
        selector: String,
        domain: String,
        private_key: DkimSigningKey,
        headers: Vec<HeaderName>,
        canonicalization: DkimCanonicalization,
    ) -> DkimConfig {
        DkimConfig {
            selector,
            domain,
            private_key,
            headers,
            canonicalization,
        }
    }
}

/// Create a Headers struct with a Dkim-Signature Header created from given parameters
fn dkim_header_format(
    config: &DkimConfig,
    timestamp: u64,
    headers_list: &str,
    body_hash: &str,
    signature: &str,
) -> Headers {
    let mut headers = Headers::new();
    let header_name =
        dkim_canonicalize_header_tag("DKIM-Signature", config.canonicalization.header);
    let header_name = HeaderName::new_from_ascii(header_name.into()).unwrap();
    headers.insert_raw(HeaderValue::new(header_name, format!("v=1; a={signing_algorithm}-sha256; d={domain}; s={selector}; c={canon}; q=dns/txt; t={timestamp}; h={headers_list}; bh={body_hash}; b={signature}",domain=config.domain, selector=config.selector,canon=config.canonicalization,timestamp=timestamp,headers_list=headers_list,body_hash=body_hash,signature=signature,signing_algorithm=config.private_key.get_signing_algorithm())));
    headers
}

/// Canonicalize the body of an email
fn dkim_canonicalize_body(
    mut body: &[u8],
    canonicalization: DkimCanonicalizationType,
) -> Cow<'_, [u8]> {
    match canonicalization {
        DkimCanonicalizationType::Simple => {
            // Remove empty lines at end
            while body.ends_with(b"\r\n\r\n") {
                body = &body[..body.len() - 2];
            }
            Cow::Borrowed(body)
        }
        DkimCanonicalizationType::Relaxed => {
            let mut out = Vec::with_capacity(body.len());
            loop {
                match body {
                    [b' ' | b'\t', b'\r', b'\n', ..] => {}
                    [b' ' | b'\t', b' ' | b'\t', ..] => {}
                    [b' ' | b'\t', ..] => out.push(b' '),
                    [c, ..] => out.push(*c),
                    [] => break,
                }
                body = &body[1..];
            }
            // Remove empty lines at end
            while out.ends_with(b"\r\n\r\n") {
                out.truncate(out.len() - 2);
            }
            Cow::Owned(out)
        }
    }
}

fn dkim_canonicalize_headers_relaxed(headers: &str) -> String {
    let mut r = String::with_capacity(headers.len());

    fn skip_whitespace(h: &str) -> &str {
        match h.as_bytes().first() {
            Some(b' ' | b'\t') => skip_whitespace(&h[1..]),
            _ => h,
        }
    }

    fn name(h: &str, out: &mut String) {
        if let Some(name_end) = h.bytes().position(|c| c == b':') {
            let (name, rest) = h.split_at(name_end + 1);
            *out += name;
            // Space after header colon is stripped.
            value(skip_whitespace(rest), out);
        } else {
            // This should never happen.
            *out += h;
        }
    }

    fn value(h: &str, out: &mut String) {
        match h.as_bytes() {
            // Continuation lines.
            [b'\r', b'\n', b' ' | b'\t', ..] => {
                out.push(' ');
                value(skip_whitespace(&h[2..]), out);
            }
            // End of header.
            [b'\r', b'\n', ..] => {
                *out += "\r\n";
                name(&h[2..], out);
            }
            // Sequential whitespace.
            [b' ' | b'\t', b' ' | b'\t' | b'\r', ..] => value(&h[1..], out),
            // All whitespace becomes spaces.
            [b'\t', ..] => {
                out.push(' ');
                value(&h[1..], out);
            }
            [_, ..] => {
                let mut chars = h.chars();
                out.push(chars.next().unwrap());
                value(chars.as_str(), out);
            }
            [] => {}
        }
    }

    name(headers, &mut r);

    r
}

/// Canonicalize header tag
fn dkim_canonicalize_header_tag(
    name: &str,
    canonicalization: DkimCanonicalizationType,
) -> Cow<'_, str> {
    match canonicalization {
        DkimCanonicalizationType::Simple => Cow::Borrowed(name),
        DkimCanonicalizationType::Relaxed => Cow::Owned(name.to_lowercase()),
    }
}

/// Canonicalize signed headers passed as `headers_list` among `mail_headers` using canonicalization
fn dkim_canonicalize_headers<'a>(
    headers_list: impl IntoIterator<Item = &'a str>,
    mail_headers: &Headers,
    canonicalization: DkimCanonicalizationType,
) -> String {
    let mut covered_headers = Headers::new();
    for name in headers_list {
        if let Some(h) = mail_headers.find_header(name) {
            let name = dkim_canonicalize_header_tag(name, canonicalization);
            covered_headers.insert_raw(HeaderValue::dangerous_new_pre_encoded(
                HeaderName::new_from_ascii(name.into()).unwrap(),
                h.get_raw().into(),
                h.get_encoded().into(),
            ));
        }
    }

    let serialized = covered_headers.to_string();

    match canonicalization {
        DkimCanonicalizationType::Simple => serialized,
        DkimCanonicalizationType::Relaxed => dkim_canonicalize_headers_relaxed(&serialized),
    }
}

/// Sign with Dkim a message by adding Dkim-Signature header created with configuration expressed by
/// `dkim_config`
pub fn dkim_sign(message: &mut Message, dkim_config: &DkimConfig) {
    dkim_sign_fixed_time(message, dkim_config, crate::time::now());
}

fn dkim_sign_fixed_time(message: &mut Message, dkim_config: &DkimConfig, timestamp: SystemTime) {
    let timestamp = timestamp
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let headers = message.headers();
    let body_hash = Sha256::digest(dkim_canonicalize_body(
        &message.body_raw(),
        dkim_config.canonicalization.body,
    ));
    let bh = crate::base64::encode(body_hash);
    let mut signed_headers_list =
        dkim_config
            .headers
            .iter()
            .fold(String::new(), |mut list, header| {
                if !list.is_empty() {
                    list.push(':');
                }

                list.push_str(header);
                list
            });
    if let DkimCanonicalizationType::Relaxed = dkim_config.canonicalization.header {
        signed_headers_list.make_ascii_lowercase();
    }
    let dkim_header = dkim_header_format(dkim_config, timestamp, &signed_headers_list, &bh, "");
    let signed_headers = dkim_canonicalize_headers(
        dkim_config.headers.iter().map(AsRef::as_ref),
        headers,
        dkim_config.canonicalization.header,
    );
    let canonicalized_dkim_header = dkim_canonicalize_headers(
        ["DKIM-Signature"],
        &dkim_header,
        dkim_config.canonicalization.header,
    );
    let mut hashed_headers = Sha256::new();
    hashed_headers.update(signed_headers.as_bytes());
    hashed_headers.update(canonicalized_dkim_header.trim_end().as_bytes());
    let hashed_headers = hashed_headers.finalize();
    let signature = match &dkim_config.private_key.0 {
        InnerDkimSigningKey::Rsa(private_key) => crate::base64::encode(
            private_key
                .sign(Pkcs1v15Sign::new::<Sha256>(), &hashed_headers)
                .unwrap(),
        ),
        InnerDkimSigningKey::Ed25519(private_key) => {
            crate::base64::encode(private_key.sign(&hashed_headers).to_bytes())
        }
    };
    let dkim_header = dkim_header_format(
        dkim_config,
        timestamp,
        &signed_headers_list,
        &bh,
        &signature,
    );
    message.headers.insert_raw(HeaderValue::new(
        HeaderName::new_from_ascii_str("DKIM-Signature"),
        dkim_header.get_raw("DKIM-Signature").unwrap().to_owned(),
    ));
}
