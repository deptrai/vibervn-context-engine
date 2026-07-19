use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::{Json, Router, http::StatusCode, routing::post};

use super::super::VoyageClient;

pub(super) async fn rejecting_client(status: StatusCode) -> (VoyageClient, Arc<AtomicUsize>) {
    let requests = Arc::new(AtomicUsize::new(0));
    let handler_requests = requests.clone();
    let app = Router::new().route(
        "/v1/embeddings",
        post(move || {
            let requests = handler_requests.clone();
            async move {
                requests.fetch_add(1, Ordering::SeqCst);
                (
                    status,
                    Json(serde_json::json!({"error": status.to_string()})),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = VoyageClient::new(
        "test-model".to_string(),
        vec!["test-key".to_string()],
        Some(&format!("http://{addr}/v1")),
    )
    .unwrap();
    (client, requests)
}
