use std::time::Duration;

use context_engine_rs::config::{Settings, config_path, write_settings_atomic};
use reqwest::Client;
use tempfile::TempDir;

mod e2e_common;

use e2e_common::{repo_id_b64, seed_settings, start_router, worker_active};

#[tokio::test]
#[ignore = "uses the real worker binary path; run with --ignored --nocapture"]
async fn adding_a_repo_auto_triggers_first_index() {
    let home = TempDir::new().unwrap();
    let settings = Settings {
        machine_id: Some("e2e-machine".to_string()),
        worker_idle_secs: 3600,
        ..Settings::default()
    };
    write_settings_atomic(&config_path(home.path()), &settings).unwrap();
    let repo_dir = home.path().join("freshrepo");
    std::fs::create_dir_all(&repo_dir).unwrap();
    std::fs::write(repo_dir.join("a.rs"), b"pub fn a() {}\n").unwrap();
    let repo = repo_dir.to_string_lossy().to_string();
    let addr = start_router(&home).await;
    let client = Client::new();
    assert!(!worker_active(&client, addr, &repo).await);

    let mut config = context_engine_rs::config::ensure_dir_and_load(home.path()).unwrap();
    config.repos.push(repo.clone());
    config.worker_idle_secs = 3600;
    let response = client
        .put(format!("http://{addr}/api/config"))
        .json(&config)
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());

    for _ in 0..40 {
        if worker_active(&client, addr, &repo).await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(worker_active(&client, addr, &repo).await);
    let statuses: serde_json::Value = client
        .get(format!("http://{addr}/api/index-status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let entry = statuses.as_array().unwrap().iter().find(|entry| {
        entry["repo"].as_str() == Some(&context_engine_rs::store::normalize_repo_path(&repo))
    });
    let entry = entry.expect("repo present in index status");
    assert_eq!(entry["worker_active"], true);
    assert!(entry.get("phase").is_some());
    assert!(matches!(entry["state"].as_str(), Some("indexing" | "idle")));
}

#[tokio::test]
#[ignore = "uses the real worker binary path; run with --ignored --nocapture"]
async fn worker_boot_triggers_incremental_index() {
    let home = TempDir::new().unwrap();
    let repo_dir = home.path().join("bootidx");
    std::fs::create_dir_all(&repo_dir).unwrap();
    std::fs::write(repo_dir.join("a.rs"), b"pub fn a() {}\n").unwrap();
    let repo = repo_dir.to_string_lossy().to_string();
    seed_settings(&home, &repo, 3600);
    let addr = start_router(&home).await;
    let client = Client::new();
    let response = client
        .get(format!(
            "http://{addr}/api/repos/{}/chunks",
            repo_id_b64(&repo)
        ))
        .query(&[("file", repo_dir.join("a.rs").to_string_lossy().as_ref())])
        .timeout(Duration::from_secs(40))
        .send()
        .await
        .unwrap();
    assert_ne!(response.status().as_u16(), 503);

    let mut observed = false;
    for _ in 0..60 {
        let statuses: serde_json::Value = client
            .get(format!("http://{addr}/api/index-status"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if let Some(entry) = statuses.as_array().unwrap().iter().find(|entry| {
            entry["repo"].as_str() == Some(&context_engine_rs::store::normalize_repo_path(&repo))
        }) {
            let state = entry["state"].as_str().unwrap_or("");
            observed = state == "indexing"
                || (state == "idle" && entry["indexed_files"].as_u64().unwrap_or(0) > 0);
        }
        if observed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(observed, "worker boot must trigger catch-up indexing");
}
