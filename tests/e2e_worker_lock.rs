use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

mod e2e_common;

use e2e_common::{seed_settings, worker_exe};

#[tokio::test]
#[ignore = "spawns a real worker subprocess; run with --ignored --nocapture"]
async fn open_db_rides_out_lock_held_by_a_live_worker_until_it_exits() {
    let home = TempDir::new().unwrap();
    let repo_dir = home.path().join("repo");
    std::fs::create_dir_all(&repo_dir).unwrap();
    std::fs::write(repo_dir.join("a.rs"), b"pub fn a() {}\n").unwrap();
    let repo = repo_dir.to_string_lossy().to_string();
    seed_settings(&home, &repo, 3600);

    let mut child = Command::new(worker_exe())
        .args(["--worker", &repo, "--port", "0", "--data-dir"])
        .arg(home.path())
        .args(["--home-dir"])
        .arg(home.path())
        .args(["--bind", "127.0.0.1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .spawn()
        .expect("spawn worker holding the lock");

    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let mut line = String::new();
        assert!(reader.read_line(&mut line).unwrap() > 0);
        if line.starts_with("CONTEXT_ENGINE_WORKER_READY") {
            break;
        }
        assert!(Instant::now() < deadline, "worker never became ready");
    }

    let data_dir = home.path().to_path_buf();
    let open_repo = repo.clone();
    let started = Instant::now();
    let open_task =
        tokio::spawn(
            async move { context_engine_rs::store::open_db(&data_dir, &open_repo, 0).await },
        );
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(
        !open_task.is_finished(),
        "lock contention was not established"
    );
    child.kill().unwrap();
    let _ = child.wait();

    let db = tokio::time::timeout(Duration::from_secs(35), open_task)
        .await
        .expect("open exceeded retry budget")
        .expect("join")
        .expect("open must succeed after worker exit");
    assert_eq!(
        context_engine_rs::store::ops::count_chunks(&db)
            .await
            .unwrap_or(0),
        0
    );
    assert!(started.elapsed() >= Duration::from_millis(800));
}
