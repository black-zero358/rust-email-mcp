use super::*;
use crate::cache::{
    AccountScope,
    sync_state::{MailboxObservation, ScanKind, ScanStamp, SyncFacts},
};
use async_imap::Session;
use sha2::Digest;
use std::{path::Path, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream},
    task::JoinHandle,
};

const UID: u32 = 7;
const INBOX: &str = "INBOX";
const MIB: u64 = 1024 * 1024;
const OBSERVATION: MailboxObservation = MailboxObservation {
    uid_validity: 9,
    uid_next: 8,
    checked_at_ms: 100,
};

struct Reply {
    command: String,
    bytes: Vec<u8>,
    status: &'static str,
}

impl Reply {
    fn ok(command: impl Into<String>, bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            command: command.into(),
            bytes: bytes.into(),
            status: "OK",
        }
    }
}

fn size(length: usize) -> Reply {
    Reply::ok(
        "UID FETCH 7 (UID RFC822.SIZE)",
        format!("* 1 FETCH (UID 7 RFC822.SIZE {length})\r\n"),
    )
}

fn section(section: &str, offset: usize, count: usize, bytes: &[u8]) -> Reply {
    let mut reply = format!(
        "* 1 FETCH (UID 7 BODY[{section}]<{offset}> {{{}}}\r\n",
        bytes.len()
    )
    .into_bytes();
    reply.extend_from_slice(bytes);
    reply.extend_from_slice(b")\r\n");
    Reply::ok(
        format!("UID FETCH 7 (UID BODY.PEEK[{section}]<{offset}.{count}>)"),
        reply,
    )
}

async fn open_session(
    account: &AccountKey,
    replies: Vec<Reply>,
    cancel: CancellationToken,
) -> (Session<DuplexStream>, Selected, JoinHandle<()>) {
    async fn tag(server: &mut BufReader<DuplexStream>, expected: &str) -> String {
        let mut line = String::new();
        assert_ne!(server.read_line(&mut line).await.unwrap(), 0, "{expected}");
        let (tag, actual) = line.trim_end_matches("\r\n").split_once(' ').unwrap();
        assert_eq!(actual, expected);
        tag.into()
    }
    let (client, server) = tokio::io::duplex(4096);
    let task = tokio::spawn(async move {
        let mut server = BufReader::new(server);
        let login = tag(&mut server, "LOGIN \"synthetic\" \"local\"").await;
        server
            .write_all(format!("{login} OK login\r\n").as_bytes())
            .await
            .unwrap();
        let selected = tag(&mut server, "EXAMINE \"INBOX\"").await;
        server.write_all(format!("* 1 EXISTS\r\n* OK [UIDVALIDITY 9] valid\r\n* OK [UIDNEXT 8] next\r\n{selected} OK [READ-ONLY] selected\r\n").as_bytes()).await.unwrap();
        for reply in replies {
            let response = tag(&mut server, &reply.command).await;
            server.write_all(&reply.bytes).await.unwrap();
            match reply.status {
                "CANCEL" => {
                    cancel.cancel();
                    return;
                }
                "EOF" => return,
                status => server
                    .write_all(format!("{response} {status} done\r\n").as_bytes())
                    .await
                    .unwrap(),
            }
        }
        let mut line = String::new();
        assert_eq!(server.read_line(&mut line).await.unwrap(), 0, "{line}");
    });
    let mut session = async_imap::Client::with_buffer_limit(client, 2 * 1024 * 1024)
        .login("synthetic", "local")
        .await
        .unwrap();
    let (selected, _) = Selected::examine(&mut session, account, INBOX)
        .await
        .unwrap();
    (session, selected, task)
}

fn open(path: &Path, account: &AccountKey, quota: u64) -> Arc<Cache> {
    Cache::open_full(
        path,
        quota,
        vec![AccountScope {
            account: account.clone(),
            mailboxes: vec![INBOX.into()],
            max_bytes: quota,
        }],
        &[],
    )
    .unwrap()
}

fn reference(account: &AccountKey) -> CacheRef<'_> {
    CacheRef {
        account,
        mailbox: INBOX,
        uid_validity: 9,
        uid: UID,
    }
}

fn seed(cache: &Cache, account: &AccountKey, header: &[u8], length: usize) -> ScanStamp {
    let state = cache
        .start_sync(account, INBOX, ScanKind::History, OBSERVATION)
        .unwrap();
    let state = cache
        .record_sync_search(&state.stamp, 1, UID, &[UID], &|| false)
        .unwrap();
    let facts = next(cache, &state.stamp, ContentLayer::Facts);
    cache
        .commit_sync_facts(
            &facts,
            SyncFacts {
                header,
                complete: true,
                index_text: None,
                thread_ids: None,
                flags: &[],
                internal_date: "1-Jan-2025",
                size: length as u32,
            },
            100,
            &|| false,
        )
        .unwrap();
    state.stamp
}

fn next(cache: &Cache, stamp: &ScanStamp, expected: ContentLayer) -> ContentWork {
    let work = cache
        .sync_content_page(stamp, 100, 1)
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(work.layer, expected);
    work
}

async fn materialize_reply(
    cache: &Arc<Cache>,
    account: &AccountKey,
    work: &ContentWork,
    replies: Vec<Reply>,
) -> ContentResult<ContentOutcome> {
    let cancellation = CancellationToken::new();
    let (mut session, selected, peer) = open_session(account, replies, cancellation.clone()).await;
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        materialize(
            cache,
            &mut session,
            Request {
                account,
                selected: &selected,
                work,
                max_message: crate::config::DEFAULT_MAX_MESSAGE,
            },
            &cancellation,
        ),
    )
    .await
    .expect("content fixture timed out");
    drop(session);
    peer.await.unwrap();
    result
}

async fn complete(
    cache: &Arc<Cache>,
    account: &AccountKey,
    stamp: &ScanStamp,
    layer: ContentLayer,
    replies: Vec<Reply>,
) -> ContentOutcome {
    let work = next(cache, stamp, layer);
    let outcome = materialize_reply(cache, account, &work, replies)
        .await
        .unwrap();
    cache
        .commit_sync_content(&work, outcome, &|| false)
        .unwrap();
    outcome
}

fn blob(outcome: ContentOutcome) -> BlobId {
    match outcome {
        ContentOutcome::PresentBlob(id) => id,
        outcome => panic!("{outcome:?}"),
    }
}

#[tokio::test]
async fn raw_and_full_utf8_index_and_all_attachment_indices_survive_restarted_checkpoint() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("cache");
    let account = AccountKey::synthetic();
    let header = b"Subject: synthetic\r\nContent-Type: multipart/mixed; boundary=m\r\n\r\n";
    let mut raw = header.to_vec();
    raw.extend_from_slice(b"--m\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n");
    raw.extend(std::iter::repeat_n(b'x', OUTPUT_CHUNK + 9));
    raw.extend_from_slice("远端全文索引尾部\r\n".as_bytes());
    for (encoding, payload) in [
        ("base64", "AP9h"),
        ("base64", "AP5i"),
        ("base64", "$AAA"),
        ("unknown", "opaque"),
    ] {
        write!(raw, "--m\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=same.bin\r\nContent-Transfer-Encoding: {encoding}\r\n\r\n{payload}\r\n").unwrap();
    }
    raw.extend_from_slice(b"--m--\r\n");
    let length = raw.len();
    let cache = open(&path, &account, 40 * MIB);
    let stamp = seed(&cache, &account, header, length);
    let mut unsupported = Reply::ok("UID FETCH 7 (UID BODYSTRUCTURE)", []);
    unsupported.status = "NO";
    assert!(matches!(
        complete(
            &cache,
            &account,
            &stamp,
            ContentLayer::Manifest,
            vec![size(length), unsupported]
        )
        .await,
        ContentOutcome::NeedsRaw
    ));
    let raw_work = next(&cache, &stamp, ContentLayer::Raw);
    let mut replies = vec![size(length)];
    for (i, bytes) in raw.chunks(OUTPUT_CHUNK).enumerate() {
        replies.push(section("", i * OUTPUT_CHUNK, bytes.len(), bytes));
    }
    replies.push(section("", length, 1, &[]));
    let stored = blob(
        materialize_reply(&cache, &account, &raw_work, replies)
            .await
            .unwrap(),
    );
    // Restart after blob registration and before the work checkpoint.
    assert_eq!(next(&cache, &stamp, ContentLayer::Raw).uid, UID);
    cache.mark_clean().unwrap();
    drop(cache);
    let cache = open(&path, &account, 40 * MIB);
    let scan = cache
        .start_sync(&account, INBOX, ScanKind::History, OBSERVATION)
        .unwrap();
    let stamp = scan.stamp;
    let raw_outcome = complete(
        &cache,
        &account,
        &stamp,
        ContentLayer::Raw,
        vec![size(length)],
    )
    .await;
    assert_eq!(blob(raw_outcome), stored);
    let mut lease = cache
        .open_blob(
            reference(&account),
            ArtifactKey::Raw,
            Access::Offline,
            Some(stored),
        )
        .unwrap();
    let mut verified = Vec::new();
    lease.read_to_end(&mut verified).unwrap();
    assert_eq!(verified, raw);
    drop(lease);
    assert!(matches!(
        complete(
            &cache,
            &account,
            &stamp,
            ContentLayer::Manifest,
            vec![size(length)]
        )
        .await,
        ContentOutcome::Present
    ));
    let body_id = blob(
        complete(
            &cache,
            &account,
            &stamp,
            ContentLayer::FullBody,
            vec![size(length)],
        )
        .await,
    );
    let indexed = blob(
        complete(
            &cache,
            &account,
            &stamp,
            ContentLayer::BodyIndex,
            vec![size(length)],
        )
        .await,
    );
    assert_eq!(body_id, indexed);
    let found = cache
        .search_cached(&account, INBOX, "全文索引尾部", None, 10, Some((1, 1)))
        .unwrap();
    assert!(
        found
            .items
            .iter()
            .any(|item| item.message.uid == UID && item.matched_body)
    );
    for (index, expected) in [(1, [0, 255, b'a']), (2, [0, 254, b'b'])] {
        let id = blob(
            complete(
                &cache,
                &account,
                &stamp,
                ContentLayer::Attachment(index),
                vec![size(length)],
            )
            .await,
        );
        let key = artifact(ContentLayer::Attachment(index)).unwrap();
        let mut lease = cache
            .open_blob(reference(&account), key, Access::Offline, Some(id))
            .unwrap();
        assert_eq!(
            lease.info().sha256,
            <[u8; 32]>::from(sha2::Sha256::digest(expected))
        );
        let mut bytes = Vec::new();
        lease.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, expected);
    }
    for (index, reason) in [
        (3, Omission::UnsupportedContent),
        (4, Omission::UnsupportedEncoding),
    ] {
        let layer = ContentLayer::Attachment(index);
        assert!(
            matches!(complete(&cache, &account, &stamp, layer, vec![size(length)]).await, ContentOutcome::Terminal(actual) if actual == reason)
        );
        assert_eq!(
            cache
                .artifact_state(reference(&account), artifact(layer).unwrap())
                .unwrap(),
            ArtifactState::Unsupported(reason)
        );
    }
    let progress = cache.sync_progress(&stamp).unwrap();
    assert_eq!(progress.pending, 0);
    assert_eq!(progress.terminal, 2);
}

#[tokio::test]
async fn fixed_html_budget_marks_only_the_index_as_omitted() {
    let root = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let cache = open(&root.path().join("cache"), &account, 40 * MIB);
    let header = b"Subject: HTML input limit\r\nContent-Type: text/html; charset=utf-8\r\n\r\n";
    let mut raw = header.to_vec();
    let body_len = crate::html::MAX_INPUT + 1;
    raw.extend(std::iter::repeat_n(b'x', body_len));
    let length = raw.len();
    let stamp = seed(&cache, &account, header, length);
    let mut unsupported = Reply::ok("UID FETCH 7 (UID BODYSTRUCTURE)", []);
    unsupported.status = "NO";
    assert!(matches!(
        complete(
            &cache,
            &account,
            &stamp,
            ContentLayer::Manifest,
            vec![size(length), unsupported]
        )
        .await,
        ContentOutcome::NeedsRaw
    ));
    let mut replies = vec![size(length)];
    for (i, bytes) in raw.chunks(OUTPUT_CHUNK).enumerate() {
        replies.push(section("", i * OUTPUT_CHUNK, bytes.len(), bytes));
    }
    replies.push(section("", length, 1, &[]));
    complete(&cache, &account, &stamp, ContentLayer::Raw, replies).await;
    for layer in [ContentLayer::Manifest, ContentLayer::FullBody] {
        complete(&cache, &account, &stamp, layer, vec![size(length)]).await;
    }
    assert!(matches!(
        complete(
            &cache,
            &account,
            &stamp,
            ContentLayer::BodyIndex,
            vec![size(length)]
        )
        .await,
        ContentOutcome::Terminal(Omission::DecodedLimit)
    ));
    let ArtifactState::Materialized(body) = cache
        .artifact_state(reference(&account), cached_body::KEY)
        .unwrap()
    else {
        panic!("the complete body must remain materialized");
    };
    assert_eq!(body.bytes, body_len as u64);
    let progress = cache.sync_progress(&stamp).unwrap();
    assert_eq!(progress.pending, 0);
    assert_eq!(progress.terminal, 1);
}

#[test]
fn body_write_checks_cancellation_between_blocks_and_releases_stage() {
    let root = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let cache = open(&root.path().join("cache"), &account, 40 * MIB);
    let size = OUTPUT_CHUNK + 1;
    seed(&cache, &account, b"Subject: cancel\r\n\r\n", size);
    let start_stage = || {
        cache
            .begin_blob(
                reference(&account),
                cached_body::KEY,
                Some(size as u64),
                size as u64,
            )
            .unwrap()
    };
    drop(start_stage());
    let baseline = cache.status(&account).unwrap().used_bytes;
    let mut stage = start_stage();
    let cancellation = CancellationToken::new();
    let mut writer = BodyWriter {
        stage: &mut stage,
        cancellation: &cancellation,
    };
    let bytes = vec![b'x'; size];
    let written = writer.write(&bytes).unwrap();
    assert_eq!(written, OUTPUT_CHUNK);
    cancellation.cancel();
    assert!(writer.write_all(&bytes[written..]).is_err());
    drop(stage);
    assert_eq!(cache.status(&account).unwrap().used_bytes, baseline);
    assert_eq!(
        cache
            .artifact_state(reference(&account), cached_body::KEY)
            .unwrap(),
        ArtifactState::Missing
    );
}

#[tokio::test]
async fn raw_command_failure_and_cancellation_do_not_advance_or_publish() {
    let root = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let cache = open(&root.path().join("cache"), &account, 40 * MIB);
    let header = b"Subject: synthetic\r\n\r\n";
    let mut raw = header.to_vec();
    raw.extend(std::iter::repeat_n(b'x', OUTPUT_CHUNK));
    let stamp = seed(&cache, &account, header, raw.len());
    let manifest = next(&cache, &stamp, ContentLayer::Manifest);
    cache
        .commit_sync_content(&manifest, ContentOutcome::NeedsRaw, &|| false)
        .unwrap();
    let work = next(&cache, &stamp, ContentLayer::Raw);
    // Establish the fixed directory cost before measuring stage cleanup.
    drop(
        cache
            .begin_blob(reference(&account), ArtifactKey::Raw, Some(0), 0)
            .unwrap(),
    );
    let baseline = cache.status(&account).unwrap().used_bytes;
    for status in ["NO", "EOF", "CANCEL"] {
        let mut tail = section(
            "",
            OUTPUT_CHUNK,
            raw.len() - OUTPUT_CHUNK,
            &raw[OUTPUT_CHUNK..],
        );
        tail.status = status;
        let outcome = materialize_reply(
            &cache,
            &account,
            &work,
            vec![
                size(raw.len()),
                section("", 0, OUTPUT_CHUNK, &raw[..OUTPUT_CHUNK]),
                tail,
            ],
        )
        .await;
        match status {
            "CANCEL" => assert!(matches!(outcome, Err(ContentError::Cancelled))),
            _ => assert!(matches!(outcome, Err(ContentError::Read(_)))),
        }
        assert_eq!(
            cache
                .artifact_state(reference(&account), ArtifactKey::Raw)
                .unwrap(),
            ArtifactState::Missing
        );
        assert_eq!(next(&cache, &stamp, ContentLayer::Raw).uid, UID);
        tokio::time::timeout(Duration::from_secs(2), async {
            while cache.status(&account).unwrap().used_bytes != baseline {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("failed content step retained a temporary file");
    }
    let mut missing = size(0);
    missing.bytes.clear();
    let outcome = materialize_reply(&cache, &account, &work, vec![missing])
        .await
        .unwrap();
    assert!(matches!(outcome, ContentOutcome::MissingRemote));
    cache
        .commit_sync_content(&work, outcome, &|| false)
        .unwrap();
    assert!(cache.sync_content_page(&stamp, 100, 1).unwrap().is_empty());
}

fn structured(header: &[u8], plain: &[u8], attachment: &[u8], size_on_server: usize) -> Vec<Reply> {
    let mut leaves = section(
        "1.MIME",
        0,
        131073,
        b"Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n",
    );
    leaves.command =
        "UID FETCH 7 (UID BODY.PEEK[1.MIME]<0.131073> BODY.PEEK[2.MIME]<0.131073>)".into();
    leaves.bytes.extend(section("2.MIME", 0, 131073,
        b"Content-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=small.bin\r\nContent-Transfer-Encoding: base64\r\n\r\n").bytes);
    let structure = format!(
        r#"* 1 FETCH (UID 7 BODYSTRUCTURE (("TEXT" "PLAIN" ("CHARSET" "UTF-8") NIL NIL "8BIT" {} 1 NIL NIL NIL NIL)("APPLICATION" "OCTET-STREAM" NIL NIL NIL "BASE64" {} NIL ("ATTACHMENT" ("FILENAME" "small.bin")) NIL NIL) "MIXED" ("BOUNDARY" "m") NIL NIL NIL))"#,
        plain.len(),
        attachment.len()
    );
    vec![
        size(size_on_server),
        Reply::ok(
            "UID FETCH 7 (UID BODYSTRUCTURE)",
            format!("{structure}\r\n"),
        ),
        section("HEADER", 0, 131073, header),
        leaves,
    ]
}

#[tokio::test]
async fn oversized_raw_omission_does_not_prevent_small_structured_body_and_attachment() {
    let root = tempfile::tempdir().unwrap();
    let account = AccountKey::synthetic();
    let cache = open(&root.path().join("cache"), &account, 40 * MIB);
    let header = b"Subject: large raw\r\nContent-Type: multipart/mixed; boundary=m\r\n\r\n";
    let body = "小正文完整".as_bytes();
    let part = b"AP9h";
    let size_on_server = 33 * MIB as usize;
    let stamp = seed(&cache, &account, header, size_on_server);
    let metadata = || structured(header, body, part, size_on_server);
    complete(&cache, &account, &stamp, ContentLayer::Manifest, metadata()).await;
    assert!(matches!(
        complete(
            &cache,
            &account,
            &stamp,
            ContentLayer::Raw,
            vec![size(size_on_server)]
        )
        .await,
        ContentOutcome::Terminal(Omission::RawLimit)
    ));
    assert_eq!(
        cache
            .artifact_state(reference(&account), ArtifactKey::Raw)
            .unwrap(),
        ArtifactState::Omitted(Omission::RawLimit)
    );
    let mut replies = metadata();
    replies.extend([
        section("1", 0, OUTPUT_CHUNK, body),
        section("1", body.len(), 1, &[]),
    ]);
    let body_id = blob(complete(&cache, &account, &stamp, ContentLayer::FullBody, replies).await);
    assert_eq!(
        blob(
            complete(
                &cache,
                &account,
                &stamp,
                ContentLayer::BodyIndex,
                vec![size(size_on_server)]
            )
            .await
        ),
        body_id
    );
    let mut replies = metadata();
    replies.extend([
        section("2", 0, part.len(), part),
        section("2", part.len(), 1, &[]),
    ]);
    let id = blob(
        complete(
            &cache,
            &account,
            &stamp,
            ContentLayer::Attachment(1),
            replies,
        )
        .await,
    );
    let mut lease = cache
        .open_blob(
            reference(&account),
            artifact(ContentLayer::Attachment(1)).unwrap(),
            Access::Offline,
            Some(id),
        )
        .unwrap();
    let mut bytes = Vec::new();
    lease.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, [0, 255, b'a']);
    assert_eq!(cache.sync_progress(&stamp).unwrap().pending, 0);
}

#[tokio::test]
async fn raw_quota_pause_retains_work_and_resumes_after_reopening_with_more_space() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("cache");
    let account = AccountKey::synthetic();
    let cache = open(&path, &account, 3 * MIB);
    let header = b"Subject: quota\r\n\r\n";
    let length = 4 * MIB as usize;
    let stamp = seed(&cache, &account, header, length);
    let manifest = next(&cache, &stamp, ContentLayer::Manifest);
    cache
        .commit_sync_content(&manifest, ContentOutcome::NeedsRaw, &|| false)
        .unwrap();
    let raw = next(&cache, &stamp, ContentLayer::Raw);
    assert!(matches!(
        materialize_reply(&cache, &account, &raw, vec![size(length)]).await,
        Err(ContentError::Storage(CacheError::Quota))
    ));
    cache
        .commit_sync_content(&raw, ContentOutcome::QuotaBlocked, &|| false)
        .unwrap();
    assert_eq!(cache.sync_progress(&stamp).unwrap().quota_blocked, 1);
    cache.mark_clean().unwrap();
    drop(cache);
    let cache = open(&path, &account, 16 * MIB);
    let stamp = cache
        .start_sync(&account, INBOX, ScanKind::History, OBSERVATION)
        .unwrap()
        .stamp;
    assert_eq!(cache.resume_sync_quota(&stamp, 1).unwrap(), 1);
    let mut bytes = header.to_vec();
    bytes.resize(length, b'x');
    let mut replies = vec![size(length)];
    for (i, part) in bytes.chunks(OUTPUT_CHUNK).enumerate() {
        replies.push(section("", i * OUTPUT_CHUNK, part.len(), part));
    }
    replies.push(section("", length, 1, &[]));
    let id = blob(complete(&cache, &account, &stamp, ContentLayer::Raw, replies).await);
    let lease = cache
        .open_blob(
            reference(&account),
            ArtifactKey::Raw,
            Access::Offline,
            Some(id),
        )
        .unwrap();
    assert_eq!(lease.info().bytes, length as u64);
    assert_eq!(cache.sync_progress(&stamp).unwrap().quota_blocked, 0);
    assert_eq!(next(&cache, &stamp, ContentLayer::Manifest).uid, UID);
}
