//! Exercise production write dispatch with local protocols and real cache state.
use crate::{
    accounts::{AccountCatalog, CacheRun, open_catalog},
    cache::{Cache, MailboxEvidence},
    config::{Config, ValidatedAccountSet},
    journal::Journal,
    mail::{MailRuntime, MailService, PreparedCache},
    smtp::SmtpSender,
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
};
use tokio_rustls::{
    TlsAcceptor,
    rustls::{
        RootCertStore, ServerConfig,
        pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer},
    },
};

use super::AccountKey;

const FOLDERS: &[&str] = &[
    "INBOX",
    "Archive",
    "Drafts",
    "New",
    "Sent",
    "Trash",
    "Tree",
    "Tree/Child",
];
const UID_VALIDITY: u32 = 9;
const OPERATION_KEY: &str = "synthetic-send";

struct Fixture {
    config: Arc<Config>,
    catalog: Arc<AccountCatalog>,
    journal: Arc<Journal>,
    cache: PreparedCache,
    storage: Arc<Cache>,
    account: AccountKey,
    _run: CacheRun,
    _directory: tempfile::TempDir,
}

impl Fixture {
    fn new(imap_port: u16) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let mut config = (*crate::config::fixture()).clone();
        config.imap_host = "localhost".into();
        config.imap_port = imap_port;
        config.database = root.join("operations.sqlite").to_str().unwrap().into();
        config.account_registry = Some(root.join("accounts.sqlite"));
        config.attachment_staging_dir = Some(root.join("attachments"));
        config.timeout = Duration::from_secs(4);
        let config = Arc::new(config);
        let accounts = ValidatedAccountSet::legacy(config.clone());
        let path = root.join("cache.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&json!({
                "version":1,"mode":"full","directory":"cache","max_bytes":32 * 1024 * 1024,
                "accounts":[{"account_id":"default","mailboxes":FOLDERS,"max_bytes":32 * 1024 * 1024}]
            }))
            .unwrap(),
        )
        .unwrap();
        let settings = crate::config::load_cache_file(&path, &accounts).unwrap();
        let catalog = open_catalog(accounts).unwrap();
        let run = CacheRun::begin(catalog.config()).unwrap();
        let cache =
            MailRuntime::prepare_cache_for_run(&catalog, settings, Some(run.identity())).unwrap();
        let account = catalog.write_account("owner").unwrap().key;
        let storage = cache.full_storage(&account).unwrap().clone();
        for folder in FOLDERS {
            storage
                .observe_folder(&account, folder, UID_VALIDITY, Some(7), 1)
                .unwrap();
        }
        let journal = Arc::new(Journal::open(config.database.as_ref()).unwrap());
        Self {
            config,
            catalog,
            journal,
            cache,
            storage,
            account,
            _run: run,
            _directory: directory,
        }
    }

    fn smtp(&self, port: u16) -> MailService {
        let mut service = MailService::with_account(
            self.config.clone(),
            self.journal.clone(),
            self.account.clone(),
        )
        .unwrap();
        service.smtp = Some(SmtpSender::fixture(port, Duration::from_secs(2)));
        service.persistent = self.cache.clone();
        service
    }

    fn remember_sent_role(&self) {
        self.storage
            .remember_mailboxes(
                &self.account,
                &[MailboxEvidence {
                    name: "Sent".into(),
                    attributes: r"\Sent".into(),
                    attribute_flags: vec![r"\Sent".into()],
                    delimiter: Some("/".into()),
                    selectable: true,
                    special_use: vec!["sent".into()],
                    role_source: Some("server".into()),
                    role_candidates: Vec::new(),
                }],
                1,
            )
            .unwrap();
    }
}

fn assert_dirty(cache: &Cache, account: &AccountKey, expected: &[&str]) {
    let status = cache.status(account).unwrap();
    assert_eq!(status.folders.len(), FOLDERS.len());
    for folder in status.folders {
        assert_eq!(
            folder.dirty,
            expected.contains(&folder.mailbox.as_str()),
            "{}",
            folder.mailbox,
        );
    }
}

fn imap_tls() -> (TlsAcceptor, RootCertStore) {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert = certified.cert.der().clone();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        certified.signing_key.serialize_der(),
    ));
    let tls = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert.clone()], key)
        .unwrap();
    let mut roots = RootCertStore::empty();
    roots.add(cert).unwrap();
    (TlsAcceptor::from(Arc::new(tls)), roots)
}

async fn imap_backend(
    listener: TcpListener,
    tls: TlsAcceptor,
    cache: Arc<Cache>,
    account: AccountKey,
    mutation: &str,
    affected: &[&str],
    reject: bool,
) {
    let (socket, _) = listener.accept().await.unwrap();
    let mut stream = BufReader::new(tls.accept(socket).await.unwrap());
    stream
        .get_mut()
        .write_all(b"* OK synthetic IMAP\r\n")
        .await
        .unwrap();
    loop {
        let mut line = String::new();
        assert_ne!(stream.read_line(&mut line).await.unwrap(), 0);
        let (tag, command) = line.trim_end_matches("\r\n").split_once(' ').unwrap();
        let result = if command.starts_with("LOGIN ") {
            ""
        } else if command == "SELECT \"INBOX\"" {
            "* 1 EXISTS\r\n* OK [UIDVALIDITY 9] valid\r\n* OK [UIDNEXT 8] next\r\n"
        } else if command == "CAPABILITY" {
            "* CAPABILITY IMAP4rev1 MOVE\r\n"
        } else {
            let append = mutation.starts_with("APPEND ");
            if append {
                assert!(command.starts_with(mutation), "{command}");
            } else {
                assert_eq!(command, mutation);
            }
            assert_dirty(&cache, &account, affected);
            if append && !reject {
                stream.get_mut().write_all(b"+ ready\r\n").await.unwrap();
                let size: usize = command
                    .rsplit_once('{')
                    .unwrap()
                    .1
                    .strip_suffix('}')
                    .unwrap()
                    .parse()
                    .unwrap();
                let mut literal = vec![0; size + 2];
                stream.read_exact(&mut literal).await.unwrap();
                assert!(
                    literal
                        .windows(b"Subject: synthetic draft".len())
                        .any(|bytes| { bytes == b"Subject: synthetic draft" })
                );
                assert!(literal.ends_with(b"\r\n"));
            }
            let status = if reject { "NO" } else { "OK" };
            stream
                .get_mut()
                .write_all(format!("{tag} {status} completed\r\n").as_bytes())
                .await
                .unwrap();
            if reject {
                let mut tail = [0; 1];
                let end = stream.read(&mut tail).await;
                assert!(
                    matches!(end, Ok(0))
                        || end
                            .as_ref()
                            .is_err_and(|error| error.kind() == std::io::ErrorKind::UnexpectedEof),
                    "bytes after rejection: {end:?}",
                );
            }
            return;
        };
        assert_dirty(&cache, &account, &[]);
        stream
            .get_mut()
            .write_all(format!("{result}{tag} OK completed\r\n").as_bytes())
            .await
            .unwrap();
    }
}

#[derive(Clone, Copy)]
enum Submission {
    Accepted,
    LostReply,
    JournalUpdateFailed,
}

async fn smtp_backend(
    listener: TcpListener,
    cache: Arc<Cache>,
    account: AccountKey,
    journal: Arc<Journal>,
    outcome: Submission,
) -> TcpListener {
    let (socket, _) = listener.accept().await.unwrap();
    let sending = || {
        let operation = journal.get("owner", OPERATION_KEY).unwrap().unwrap();
        assert_eq!(operation.status, "sending");
        assert_dirty(&cache, &account, &["Sent"]);
    };
    sending();
    let mut stream = BufReader::new(socket);
    stream
        .get_mut()
        .write_all(b"220 synthetic ESMTP\r\n")
        .await
        .unwrap();
    let mut data = false;
    let mut payload = String::new();
    loop {
        let mut line = String::new();
        assert_ne!(stream.read_line(&mut line).await.unwrap(), 0);
        let response = if data {
            if line != ".\r\n" {
                payload.push_str(&line);
                continue;
            }
            assert!(payload.contains("Subject: synthetic message"));
            assert!(payload.contains("synthetic body"));
            sending();
            if matches!(outcome, Submission::LostReply) {
                break;
            }
            if matches!(outcome, Submission::JournalUpdateFailed) {
                journal.fail_writes();
            }
            data = false;
            "250 accepted\r\n"
        } else if line.starts_with("EHLO ") || line.starts_with("HELO ") {
            "250 synthetic\r\n"
        } else if line.starts_with("MAIL FROM:") || line.starts_with("RCPT TO:") {
            "250 ok\r\n"
        } else if line == "DATA\r\n" {
            data = true;
            "354 start data\r\n"
        } else if line == "QUIT\r\n" {
            if matches!(outcome, Submission::Accepted) {
                assert_eq!(
                    journal.get("owner", OPERATION_KEY).unwrap().unwrap().status,
                    "accepted"
                );
            }
            stream.get_mut().write_all(b"221 bye\r\n").await.unwrap();
            break;
        } else {
            panic!("unexpected SMTP command: {line}");
        };
        stream
            .get_mut()
            .write_all(response.as_bytes())
            .await
            .unwrap();
    }
    listener
}

fn send_args() -> Value {
    json!({
        "operation_key":OPERATION_KEY,"to":["receiver@example.test"],
        "subject":"synthetic message","body":"synthetic body"
    })
}

#[tokio::test]
async fn send_fences_only_after_claim_and_replay_preserves_known_or_unknown_outcomes() {
    for outcome in [
        Submission::Accepted,
        Submission::LostReply,
        Submission::JournalUpdateFailed,
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fixture = Fixture::new(993);
        fixture.remember_sent_role();
        let service = fixture.smtp(listener.local_addr().unwrap().port());
        let backend = tokio::spawn(smtp_backend(
            listener,
            fixture.storage.clone(),
            fixture.account.clone(),
            fixture.journal.clone(),
            outcome,
        ));
        let args = send_args();
        let result = service.execute("send_email", &args, "owner").await.unwrap();
        let expected = if matches!(outcome, Submission::LostReply) {
            "unknown"
        } else {
            "accepted"
        };
        assert_eq!(result["operation"]["status"], expected);
        assert_eq!(result["retry_allowed"], false);
        let listener = tokio::time::timeout(Duration::from_secs(4), backend)
            .await
            .unwrap()
            .unwrap();
        assert!(service.write_guards.drain(Duration::from_secs(2)).await);
        let stamp = || {
            fixture
                .storage
                .dirty_stamp(&fixture.account, "Sent")
                .unwrap()
        };
        let before = stamp();
        if matches!(outcome, Submission::JournalUpdateFailed) {
            assert!(
                result["operation"]["warnings"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("journal_update_failed"))
            );
        } else {
            let replay = service.execute("send_email", &args, "owner").await.unwrap();
            assert_eq!(replay["operation"], result["operation"]);
            assert!(service.write_guards.drain(Duration::from_secs(2)).await);
            assert!(stamp() == before, "replay changed the Sent dirty stamp");
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(30), listener.accept())
                .await
                .is_err()
        );
        fixture.cache.clone().finish().await;
    }

    // A journal failure before the initial claim must leave both the
    // remote submission and its local dirty side effect unstarted.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fixture = Fixture::new(993);
    fixture.remember_sent_role();
    let service = fixture.smtp(listener.local_addr().unwrap().port());
    fixture.journal.fail_writes();
    let result = service.execute("send_email", &send_args(), "owner").await;
    assert_eq!(result.unwrap_err().code, "STORAGE_UNAVAILABLE");
    assert_dirty(&fixture.storage, &fixture.account, &[]);
    assert!(
        tokio::time::timeout(Duration::from_millis(30), listener.accept())
            .await
            .is_err()
    );
    fixture.cache.clone().finish().await;
}

#[tokio::test]
async fn runtime_fences_the_expected_folders_before_the_first_imap_mutation() {
    let draft =
        json!({"to":["receiver@example.test"],"subject":"synthetic draft","body":"draft body"});
    let (tls, roots) = imap_tls();
    for (tool, args, mutation, affected, expected) in [
        (
            "move_email",
            json!({"uid":7,"uid_validity":9,"destination":"Archive"}),
            "UID MOVE 7 \"Archive\"",
            &["INBOX", "Archive"][..],
            Some("updated"),
        ),
        (
            "delete_email",
            json!({"uid":7,"uid_validity":9,"confirm":true}),
            "UID MOVE 7 \"Trash\"",
            &["INBOX", "Trash"],
            None,
        ),
        (
            "save_draft",
            draft.clone(),
            r#"APPEND "Drafts" (\Draft) {"#,
            &["Drafts"],
            Some("saved"),
        ),
        (
            "save_draft",
            draft,
            r#"APPEND "Drafts" (\Draft) {"#,
            &["Drafts"],
            None,
        ),
        (
            "create_mailbox",
            json!({"name":"New"}),
            "CREATE \"New\"",
            &["New"],
            Some("created"),
        ),
        (
            "rename_mailbox",
            json!({"name":"Tree","new_name":"New"}),
            "RENAME \"Tree\" \"New\"",
            FOLDERS,
            Some("renamed"),
        ),
        (
            "delete_mailbox",
            json!({"name":"Tree","confirm":true}),
            "DELETE \"Tree\"",
            FOLDERS,
            Some("deleted"),
        ),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fixture = Fixture::new(listener.local_addr().unwrap().port());
        let runtime = MailRuntime::with_cache(
            fixture.catalog.clone(),
            fixture.journal.clone(),
            fixture.cache.clone(),
        )
        .unwrap()
        .with_test_imap_roots(roots.clone());
        let server = tokio::spawn(imap_backend(
            listener,
            tls.clone(),
            fixture.storage.clone(),
            fixture.account.clone(),
            mutation,
            affected,
            expected.is_none(),
        ));
        let result = runtime.execute(tool, &args, "owner").await;
        if let Some(status) = expected {
            assert_eq!(result.unwrap()["status"], status, "{tool}");
        } else {
            assert_eq!(result.unwrap_err().code, "MAIL_BACKEND", "{tool}");
        }
        tokio::time::timeout(Duration::from_secs(4), server)
            .await
            .unwrap()
            .unwrap();
        runtime.finish_cache(fixture.cache.clone()).await;
        assert_dirty(&fixture.storage, &fixture.account, affected);
    }
}

#[tokio::test]
async fn batch_observes_only_confirmed_existing_generation_misses_and_preserves_item_statuses() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fixture = Fixture::new(listener.local_addr().unwrap().port());
    let (tls, roots) = imap_tls();
    fixture
        .storage
        .observe_folder(&fixture.account, "INBOX", UID_VALIDITY, Some(9), 2)
        .unwrap();
    let reference = |uid| crate::cache::Ref {
        account: &fixture.account,
        mailbox: "INBOX",
        uid_validity: UID_VALIDITY,
        uid,
    };
    for uid in [7, 8] {
        fixture
            .storage
            .remember_headers(reference(uid), true, b"Subject: retained\r\n\r\n", None)
            .unwrap();
        assert!(
            fixture
                .storage
                .cached_message(reference(uid))
                .unwrap()
                .is_some()
        );
    }
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut stream = BufReader::new(tls.accept(socket).await.unwrap());
        stream
            .get_mut()
            .write_all(b"* OK synthetic IMAP\r\n")
            .await
            .unwrap();
        for (expected, rows, status) in [
            ("LOGIN ", "", "OK"),
            (
                "SELECT \"INBOX\"",
                "* 2 EXISTS\r\n* OK [UIDVALIDITY 9] valid\r\n* OK [UIDNEXT 10] next\r\n",
                "OK",
            ),
            ("UID FETCH 99 (UID)", "", "OK"),
            (
                "SELECT \"INBOX\"",
                "* 2 EXISTS\r\n* OK [UIDVALIDITY 9] valid\r\n* OK [UIDNEXT 10] next\r\n",
                "OK",
            ),
            ("UID FETCH 7 (UID)", "", "OK"),
            (
                "SELECT \"INBOX\"",
                "* 2 EXISTS\r\n* OK [UIDVALIDITY 9] valid\r\n* OK [UIDNEXT 10] next\r\n",
                "OK",
            ),
            ("UID FETCH 8 (UID)", "* 1 FETCH (UID 8)\r\n", "NO"),
        ] {
            let mut line = String::new();
            assert_ne!(stream.read_line(&mut line).await.unwrap(), 0);
            let (tag, command) = line.trim_end_matches("\r\n").split_once(' ').unwrap();
            if expected == "LOGIN " {
                assert!(command.starts_with(expected));
            } else {
                assert_eq!(command, expected);
            }
            stream
                .get_mut()
                .write_all(format!("{rows}{tag} {status} completed\r\n").as_bytes())
                .await
                .unwrap();
        }
        let mut tail = [0; 1];
        let end = stream.read(&mut tail).await;
        assert!(
            matches!(end, Ok(0))
                || end
                    .as_ref()
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::UnexpectedEof),
            "bytes after the refused fetch: {end:?}",
        );
    });
    let runtime = MailRuntime::with_cache(
        fixture.catalog.clone(),
        fixture.journal.clone(),
        fixture.cache.clone(),
    )
    .unwrap()
    .with_test_imap_roots(roots);
    let result = runtime
        .execute(
            "mark_emails",
            &json!({
                "uid_validity":UID_VALIDITY, "uids":[99,7,8,9],"flag":"seen","value":true
            }),
            "owner",
        )
        .await
        .unwrap();
    assert_eq!(result["items"][0], json!({"uid":99,"status":"not_found"}));
    assert_eq!(result["items"][1], json!({"uid":7,"status":"not_found"}));
    assert_eq!(result["items"][2]["status"], "unknown");
    assert_eq!(result["items"][2]["error"]["code"], "MAIL_BACKEND");
    assert_eq!(
        result["items"][3],
        json!({"uid":9,"status":"unprocessed","retry_allowed":false})
    );
    assert_eq!(result["retry_allowed"], false);
    tokio::time::timeout(Duration::from_secs(4), server)
        .await
        .unwrap()
        .unwrap();
    runtime.finish_cache(fixture.cache.clone()).await;
    assert_dirty(&fixture.storage, &fixture.account, &[]);
    assert!(
        fixture
            .storage
            .cached_message(reference(7))
            .unwrap()
            .is_none()
    );
    assert!(
        fixture
            .storage
            .cached_message(reference(8))
            .unwrap()
            .is_some()
    );
}
