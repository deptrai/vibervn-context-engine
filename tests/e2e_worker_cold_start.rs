use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::{Json, Router, routing::post};
use reqwest::Client;
use tempfile::TempDir;
use tokio::net::TcpListener;

use context_engine_rs::config::{Settings, config_path, write_settings_atomic};

mod e2e_common;

use e2e_common::start_router;

/// The first request that elects a worker must survive readiness, boot catch-up
/// indexing, query embedding, and retrieval in one call while the gateway hides
/// an upstream pool-key 402 by failing over to a healthy key. The admin crate's
/// integration suite tests the real failover implementation; this mock pins the
/// external contract seen by a newly spawned engine worker.
#[tokio::test]
#[ignore = "uses the real worker binary path; run with --ignored --nocapture"]
async fn first_codebase_query_after_worker_startup_succeeds() {
    let gateway_requests = Arc::new(AtomicUsize::new(0));
    let upstream_payment_required = Arc::new(AtomicUsize::new(0));
    let upstream_healthy = Arc::new(AtomicUsize::new(0));
    let handler_requests = gateway_requests.clone();
    let handler_payment_required = upstream_payment_required.clone();
    let handler_healthy = upstream_healthy.clone();
    let gateway = Router::new().route(
        "/v1/embeddings",
        post(move || {
            let handler_requests = handler_requests.clone();
            let handler_payment_required = handler_payment_required.clone();
            let handler_healthy = handler_healthy.clone();
            async move {
                handler_requests.fetch_add(1, Ordering::SeqCst);
                // Simulated admin-internal failover: the exhausted pool key is
                // attempted once, then a healthy key supplies the response.
                handler_payment_required.fetch_add(1, Ordering::SeqCst);
                handler_healthy.fetch_add(1, Ordering::SeqCst);
                Json(serde_json::json!({
                    "data": [{"embedding": [1.0, 0.0, 0.0, 0.0]}]
                }))
            }
        }),
    );
    let gateway_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway_addr = gateway_listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(gateway_listener, gateway).await.unwrap();
    });

    let home = TempDir::new().unwrap();
    let repo_dir = home.path().join("first-query");
    std::fs::create_dir_all(&repo_dir).unwrap();
    std::fs::write(
        repo_dir.join("billing_probe.rs"),
        b"pub fn credit_is_available() -> bool { true }\n",
    )
    .unwrap();
    let repo = repo_dir.to_string_lossy().to_string();
    let mut settings = Settings {
        machine_id: Some("first-query-e2e-machine".to_string()),
        ..Settings::default()
    };
    settings.repos = vec![repo.clone()];
    settings.worker_idle_secs = 3600;
    settings.mcp_index_wait_secs = 30;
    settings.embedding.api_keys = vec!["proxy-test-key".to_string()];
    settings.embedding.voyage_base_url = Some(format!("http://{gateway_addr}/v1"));
    write_settings_atomic(&config_path(home.path()), &settings).unwrap();

    let addr = start_router(&home).await;
    let response = Client::new()
        .post(format!("http://{addr}/api/mcp-tool"))
        .json(&serde_json::json!({
            "information_request": "Where is credit availability checked?",
            "workspace_full_path": repo,
        }))
        .timeout(Duration::from_secs(45))
        .send()
        .await
        .expect("the first request must spawn a worker and complete");
    assert!(response.status().is_success());
    let body: serde_json::Value = response.json().await.unwrap();
    let result = body["result"].as_str().unwrap_or("");

    assert!(
        result.contains("billing_probe.rs") || result.contains("credit_is_available"),
        "the first cold-start query must return indexed code, got: {result}"
    );
    assert!(
        !result.contains("embedding failed") && !result.contains("Payment Required"),
        "the first query must not expose a gateway billing failure: {result}"
    );
    assert!(
        gateway_requests.load(Ordering::SeqCst) >= 2,
        "cold path must exercise both document and query embedding"
    );
    assert_eq!(
        upstream_payment_required.load(Ordering::SeqCst),
        gateway_requests.load(Ordering::SeqCst),
        "every cold-start embedding request must exercise the upstream 402 branch"
    );
    assert_eq!(
        upstream_healthy.load(Ordering::SeqCst),
        gateway_requests.load(Ordering::SeqCst),
        "every upstream 402 must fail over to a healthy pool key"
    );
}
