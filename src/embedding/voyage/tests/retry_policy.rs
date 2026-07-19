use std::time::Duration;

use reqwest::Client;

use crate::embedding::InputType;

use super::super::VoyageClient;
use super::super::batching::byte_aware_batches;
use super::super::retry::{
    EmbedError, TRANSIENT_RETRY_LIMIT, backoff_with_jitter, is_send_error_transient,
};

#[test]
fn batching_respects_byte_and_count_limits() {
    let large: Vec<String> = (0..5).map(|_| "x".repeat(600_000)).collect();
    assert_eq!(
        byte_aware_batches(&large)
            .iter()
            .map(|batch| batch.len())
            .collect::<Vec<_>>(),
        [2, 2, 1]
    );
    let many: Vec<String> = (0..200).map(|_| "short".to_string()).collect();
    assert_eq!(
        byte_aware_batches(&many)
            .iter()
            .map(|batch| batch.len())
            .collect::<Vec<_>>(),
        [128, 72]
    );
    let oversized = vec!["x".repeat(3_000_000), "small".to_string()];
    assert_eq!(
        byte_aware_batches(&oversized)
            .iter()
            .map(|batch| batch.len())
            .collect::<Vec<_>>(),
        [1, 1]
    );
}

#[test]
fn backoff_jitter_is_bounded() {
    for cursor in 0..100 {
        let duration = backoff_with_jitter(2, cursor);
        assert!(duration >= Duration::from_secs(2));
        assert!(duration < Duration::from_millis(2500));
    }
    assert!(backoff_with_jitter(0, 42) < Duration::from_millis(100));
}

#[test]
#[allow(clippy::assertions_on_constants)]
fn transient_retry_limit_is_bounded() {
    assert!((2..=10).contains(&TRANSIENT_RETRY_LIMIT));
}

#[tokio::test]
async fn transport_errors_are_transient() {
    let client = Client::builder()
        .timeout(Duration::from_millis(1))
        .build()
        .unwrap();
    let voyage = VoyageClient::new(
        "test-model".to_string(),
        vec!["fake-key".to_string()],
        Some("http://127.0.0.1:1"),
    )
    .unwrap();
    let result = voyage
        .try_embed_with_key_using(
            &client,
            "fake-key",
            &["test".to_string()],
            InputType::Document,
        )
        .await;
    assert!(matches!(result, Err(EmbedError::Transient(_))));
}

#[tokio::test]
async fn send_error_predicate_covers_network_failures() {
    let client = Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let error = client
        .get("http://127.0.0.1:1/embeddings")
        .send()
        .await
        .unwrap_err();
    assert!(!error.is_builder());
    assert!(is_send_error_transient(&error));
    assert!(error.is_timeout() || error.is_connect() || error.is_request());
}
