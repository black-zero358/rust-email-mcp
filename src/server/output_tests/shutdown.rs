use super::*;
use std::{
    path::PathBuf,
    process::{Child, Command},
    time::Instant,
};
use tokio::sync::mpsc;

const CHILD_DIR: &str = "RUST_EMAIL_MCP_TEST_SIGTERM_DIR";
const OUTCOME: &str = "RUST_EMAIL_MCP_TEST_SIGTERM_OUTCOME";
const WAIT: Duration = Duration::from_secs(5);

struct Process(Child);

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn observe<T>(mut poll: impl FnMut() -> Option<T>) -> T {
    tokio::time::timeout(WAIT, async {
        loop {
            if let Some(value) = poll() {
                return value;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("shutdown test milestone")
}

#[tokio::test]
async fn sigterm_drains_admitted_drafts() {
    if let Some(dir) = std::env::var_os(CHILD_DIR) {
        serve_fixture(PathBuf::from(dir), &std::env::var(OUTCOME).unwrap()).await;
        return;
    }
    for outcome in ["idle", "accepted", "unknown"] {
        let dir = tempfile::tempdir().unwrap();
        let mut child = Process(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "server::output_tests::shutdown::sigterm_drains_admitted_drafts",
                    "--nocapture",
                ])
                .env(CHILD_DIR, dir.path())
                .env(OUTCOME, outcome)
                .spawn()
                .unwrap(),
        );
        let addr: std::net::SocketAddr = observe(|| {
            std::fs::read_to_string(dir.path().join("imap-waiting"))
                .ok()?
                .parse()
                .ok()
        })
        .await;
        let signalled = Instant::now();
        assert!(
            Command::new("kill")
                .args(["-TERM", &child.0.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        if outcome == "idle" {
            assert!(observe(|| child.0.try_wait().unwrap()).await.success());
            assert!(signalled.elapsed() < Duration::from_secs(2));
            continue;
        }
        // Keep the write in IMAP until the listener has stopped accepting.
        observe(|| std::net::TcpStream::connect(addr).is_err().then_some(())).await;
        assert!(child.0.try_wait().unwrap().is_none());
        std::fs::write(dir.path().join("release"), []).unwrap();
        assert!(observe(|| child.0.try_wait().unwrap()).await.success());
        assert!(signalled.elapsed() < WAIT);
        // Read the durable state without Journal::open's sending recovery.
        let database = rusqlite::Connection::open_with_flags(
            dir.path().join("operations.sqlite"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let status: String = database.query_row(
            "SELECT status FROM operations WHERE principal='owner' AND operation_key='during-shutdown'",
            [], |row| row.get(0)
        ).unwrap();
        assert_eq!(status, outcome);
    }
}

async fn serve_fixture(dir: PathBuf, outcome: &str) {
    let accepted = outcome == "accepted";
    let signed = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert = signed.cert.der().clone();
    let mut roots = RootCertStore::empty();
    roots.add(cert.clone()).unwrap();
    let tls = tokio_rustls::rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert],
            PrivateKeyDer::Pkcs8(signed.signing_key.serialize_der().into()),
        )
        .unwrap();
    let (gate, mut imap_started) = mpsc::channel(1);
    let imap =
        Backend::start_with_draft_gate(TlsAcceptor::from(Arc::new(tls)), "draft body", Some(gate))
            .await;
    let smtp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let smtp_port = smtp.local_addr().unwrap().port();
    let smtp_task = tokio::spawn(async move {
        for _ in 0..if accepted { 2 } else { 1 } {
            let (socket, _) = smtp.accept().await.unwrap();
            let mut socket = BufReader::new(socket);
            socket.write_all(b"220 fixture\r\n").await.unwrap();
            for (prefix, reply) in [
                ("EHLO ", "250 fixture\r\n"),
                ("MAIL ", "250 OK\r\n"),
                ("RCPT ", "250 OK\r\n"),
                ("DATA", "354 data\r\n"),
            ] {
                let mut line = String::new();
                socket.read_line(&mut line).await.unwrap();
                assert!(line.starts_with(prefix));
                socket.write_all(reply.as_bytes()).await.unwrap();
            }
            loop {
                let mut line = String::new();
                assert_ne!(socket.read_line(&mut line).await.unwrap(), 0);
                if line == ".\r\n" {
                    break;
                }
            }
            if accepted {
                // Together with the gated IMAP phase, this send needs more
                // than one configured operation timeout.
                tokio::time::sleep(Duration::from_millis(600)).await;
                socket.write_all(b"250 accepted\r\n").await.unwrap();
                let mut line = String::new();
                socket.read_line(&mut line).await.unwrap();
                assert_eq!(line, "QUIT\r\n");
                socket.write_all(b"221 bye\r\n").await.unwrap();
            } else {
                assert_eq!(socket.read_line(&mut String::new()).await.unwrap(), 0);
            }
        }
    });

    let mut config = (*config::fixture()).clone();
    config.imap_host = "localhost".into();
    config.imap_port = imap.port;
    config.timeout = Duration::from_secs(1);
    let config = Arc::new(config);
    let journal = Arc::new(Journal::open(&dir.join("operations.sqlite")).unwrap());
    let catalog =
        AccountCatalog::from_test_accounts(ValidatedAccountSet::legacy(config.clone())).unwrap();
    let mail = Arc::new(
        MailRuntime::new(catalog, journal.clone())
            .unwrap()
            .with_test_imap_roots(roots)
            .with_test_smtp(smtp_port),
    );
    let credentials = Credentials::new(config.clone());
    let shutdown = CancellationToken::new();
    let router = http_router(
        &config,
        mail.clone(),
        credentials.auth.clone(),
        shutdown.clone(),
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(serve_until_shutdown(
        listener,
        router,
        mail,
        Default::default(),
        shutdown,
    ));
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    if outcome == "idle" {
        assert!(
            http.get(format!("http://{addr}/healthz"))
                .send()
                .await
                .unwrap()
                .status()
                .is_success()
        );
        std::fs::write(dir.join("imap-waiting"), addr.to_string()).unwrap();
        assert!(server.await.unwrap().unwrap());
        smtp_task.abort();
        return;
    }
    let rpc = |key| {
        let request = http.post(format!("http://{addr}/mcp"))
            .bearer_auth(credentials.token("owner"))
            .header("MCP-Protocol-Version", "2025-06-18")
            .header("Accept", "application/json, text/event-stream")
            .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":"send_draft","arguments":{"uid":7,"uid_validity":9,"operation_key":key}}}));
        tokio::spawn(async move { request.send().await })
    };

    if accepted {
        let cancelled = rpc("client-cancelled");
        let release = imap_started.recv().await.unwrap();
        cancelled.abort();
        assert!(cancelled.await.unwrap_err().is_cancelled());
        release.send(()).unwrap();
        observe(|| {
            journal
                .get("owner", "client-cancelled")
                .unwrap()
                .filter(|operation| operation.status == "accepted")
        })
        .await;
    }
    let request = rpc("during-shutdown");
    let release = imap_started.recv().await.unwrap();
    std::fs::write(dir.join("imap-waiting"), addr.to_string()).unwrap();
    observe(|| dir.join("release").exists().then_some(())).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    release.send(()).unwrap();
    assert!(server.await.unwrap().unwrap());
    smtp_task.await.unwrap();
    request.abort();
}
