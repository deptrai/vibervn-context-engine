use std::time::Duration;

use reqwest::Client;
use tempfile::TempDir;

mod e2e_common;

use e2e_common::{poke_action, repo_id_b64, seed_settings, start_router, worker_active};

#[tokio::test]
#[ignore = "uses the real worker binary path; run with --ignored --nocapture"]
async fn opening_detail_spawns_no_worker() {
    let home = TempDir::new().unwrap();
    let repo_dir = home.path().join("repo");
    std::fs::create_dir_all(&repo_dir).unwrap();
    std::fs::write(repo_dir.join("a.rs"), b"pub fn a() {}\n").unwrap();
    let repo = repo_dir.to_string_lossy().to_string();
    seed_settings(&home, &repo, 3600);
    let addr = start_router(&home).await;
    let client = Client::new();
    let id = repo_id_b64(&repo);

    for path in ["status", "index-stats", "graph", "files"] {
        let response = client
            .get(format!("http://{addr}/api/repos/{id}/{path}"))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success(), "{path} must serve cold");
    }
    assert!(!worker_active(&client, addr, &repo).await);
    assert!(poke_action(&client, addr, &repo).await.is_success());
    assert!(worker_active(&client, addr, &repo).await);
}

#[tokio::test]
#[ignore = "spawns a real worker subprocess; run with --ignored --nocapture"]
async fn worker_spawn_idleexit_respawn_no_lock_collision() {
    let home = TempDir::new().unwrap();
    let repo_dir = home.path().join("repo");
    std::fs::create_dir_all(&repo_dir).unwrap();
    std::fs::write(repo_dir.join("main.rs"), b"fn main() {}\n").unwrap();
    let repo = repo_dir.to_string_lossy().to_string();
    seed_settings(&home, &repo, 1);
    let addr = start_router(&home).await;
    let client = Client::new();

    assert!(poke_action(&client, addr, &repo).await.is_success());
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(
        poke_action(&client, addr, &repo).await.is_success(),
        "respawn must ride out the old worker's RocksDB lock release"
    );
}

#[tokio::test]
#[ignore = "spawns a real worker subprocess; run with --ignored --nocapture"]
async fn worker_does_not_exit_while_requests_in_flight() {
    let home = TempDir::new().unwrap();
    let repo_dir = home.path().join("repo");
    std::fs::create_dir_all(&repo_dir).unwrap();
    std::fs::write(repo_dir.join("lib.rs"), b"pub fn f() {}\n").unwrap();
    let repo = repo_dir.to_string_lossy().to_string();
    seed_settings(&home, &repo, 1);
    let addr = start_router(&home).await;
    let client = Client::new();
    assert!(poke_action(&client, addr, &repo).await.is_success());

    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let mut count = 0;
    while std::time::Instant::now() < deadline {
        assert!(poke_action(&client, addr, &repo).await.is_success());
        count += 1;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(count > 5);
}
