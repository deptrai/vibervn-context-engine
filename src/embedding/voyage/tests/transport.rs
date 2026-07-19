use std::sync::atomic::Ordering;

use axum::http::StatusCode;

use super::fixture::rejecting_client;

#[tokio::test]
async fn query_does_not_retry_auth_or_payment_rejections() {
    for status in [
        StatusCode::UNAUTHORIZED,
        StatusCode::PAYMENT_REQUIRED,
        StatusCode::FORBIDDEN,
    ] {
        let (client, requests) = rejecting_client(status).await;

        let error = client.embed_query("query").await.unwrap_err();
        assert!(error.to_string().contains(status.as_str()));
        assert_eq!(requests.load(Ordering::SeqCst), 1, "{status} was retried");
    }
}
