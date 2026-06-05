pub mod google;
pub mod openai;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use anyhow::{Result, bail};
use reqwest::Client;
use crate::config::LlmConfig;

#[derive(Clone)]
pub struct LlmClient {
    provider: String,
    model: String,
    api_keys: Vec<String>,
    http: Client,
    key_cursor: std::sync::Arc<AtomicUsize>,
}

impl LlmClient {
    /// Create a new client. Returns None if api_keys is empty.
    pub fn new(config: &LlmConfig) -> Option<Self> {
        if config.api_keys.is_empty() {
            return None;
        }
        let http = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .ok()?;
        Some(Self {
            provider: config.provider.clone(),
            model: config.rerank_model.clone(),
            api_keys: config.api_keys.clone(),
            http,
            key_cursor: std::sync::Arc::new(AtomicUsize::new(0)),
        })
    }

    fn next_key(&self) -> &str {
        let idx = self.key_cursor.fetch_add(1, Ordering::Relaxed) % self.api_keys.len();
        &self.api_keys[idx]
    }

    /// Send a completion request to the configured LLM provider.
    pub async fn complete(&self, system: &str, user: &str, temperature: f32) -> Result<String> {
        match self.provider.as_str() {
            "google" => google::complete(&self.http, &self.model, self.next_key(), system, user, temperature).await,
            "openai" => openai::complete(&self.http, &self.model, self.next_key(), system, user, temperature).await,
            other => bail!("unsupported LLM provider: {other}"),
        }
    }
}
