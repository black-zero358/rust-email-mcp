use super::*;

fn config(directory: &Path) -> Config {
    let mut config = (*crate::config::fixture()).clone();
    config.database = directory.join("operations.sqlite").to_str().unwrap().into();
    config.imap_host = "imap.example.test".into();
    config
}

#[test]
fn concurrent_initializers_share_one_identity_and_leave_no_stage() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path());
    fs::write(&config.database, b"existing synthetic operations file").unwrap();
    let gate = std::sync::Arc::new(std::sync::Barrier::new(8));
    let handles = (0..8)
        .map(|_| {
            let gate = gate.clone();
            let config = config.clone();
            std::thread::spawn(move || {
                gate.wait();
                open_default(&config).unwrap()
            })
        })
        .collect::<Vec<_>>();
    let keys = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect::<Vec<_>>();
    assert!(keys.iter().all(|key| key == &keys[0]));
    assert_eq!(
        fs::read_dir(dir.path()).unwrap().count(),
        3,
        "only the operation file, accounts.sqlite and its persistent lock remain"
    );
}

#[test]
fn binding_failure_leaves_pending_send_state_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config(dir.path());
    open_default(&config).unwrap();
    let journal = crate::journal::Journal::open(Path::new(&config.database)).unwrap();
    journal
        .claim("synthetic-owner", "key", "fingerprint", "id")
        .unwrap();
    drop(journal);
    config.address = "another@example.test".into();
    assert_eq!(
        open_default(&config).unwrap_err().to_string(),
        BINDING_ERROR
    );
    let db = Connection::open(&config.database).unwrap();
    let status: String = db
        .query_row("SELECT status FROM operations", [], |row| row.get(0))
        .unwrap();
    assert_eq!(status, "sending");
}
