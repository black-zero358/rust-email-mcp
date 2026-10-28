use rust_email_mcp::journal::Journal;
use std::{path::Path, sync::Arc, time::Instant};
fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("pass a new scratch database path");
    assert!(!Path::new(&path).exists(), "Use a new scratch file");
    let journal = Arc::new(Journal::open(Path::new(&path)).unwrap());
    let start = Instant::now();
    let workers: Vec<_> = (0..4)
        .map(|worker| {
            let journal = journal.clone();
            std::thread::spawn(move || {
                let mut durations = Vec::new();
                for i in 0..250 {
                    let start = Instant::now();
                    let key = format!("{worker}-{i}");
                    journal.claim("benchmark", &key, "hash", "test-id").unwrap();
                    journal.finish("benchmark", &key, "accepted", &[]).unwrap();
                    assert_eq!(
                        journal.get("benchmark", &key).unwrap().unwrap().status,
                        "accepted"
                    );
                    durations.push(start.elapsed().as_micros() as u64);
                }
                durations
            })
        })
        .collect();
    let mut durations = Vec::new();
    for worker in workers {
        durations.extend(worker.join().unwrap());
    }
    durations.sort_unstable();
    println!(
        "{}",
        serde_json::json!({"sqlite_version":rusqlite::version(),"workers":4,"operations":durations.len(),"elapsed_ms":start.elapsed().as_millis(),"median_us":durations[durations.len()/2],"p95_us":durations[durations.len()*95/100],"max_us":durations.last(),"synchronous":"FULL","journal_mode":"WAL"})
    );
}
