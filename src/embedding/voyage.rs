use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::Client;

mod batch_transport;
mod batching;
mod http_transport;
mod query_transport;
mod retry;

#[cfg(test)]
pub use retry::TRANSIENT_RETRY_LIMIT_FOR_TEST;
pub use retry::TransientEmbedExhausted;

const VOYAGE_ENDPOINT: &str = "https://api.voyageai.com/v1/embeddings";
const OPENAI_ENDPOINT: &str = "https://api.openai.com/v1/embeddings";
pub const MAX_BATCH_SIZE: usize = 128;
const MAX_BATCH_BYTES: usize = 1_500_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Voyage,
    OpenAI,
}

impl Provider {
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "openai" => Self::OpenAI,
            _ => Self::Voyage,
        }
    }

    fn default_endpoint(self) -> &'static str {
        match self {
            Self::Voyage => VOYAGE_ENDPOINT,
            Self::OpenAI => OPENAI_ENDPOINT,
        }
    }
}

pub fn embedding_url(provider: Provider, base: Option<&str>) -> String {
    let raw = base.unwrap_or_default().trim();
    if raw.is_empty() {
        return provider.default_endpoint().to_owned();
    }
    let trimmed = raw.trim_end_matches('/');
    if trimmed.ends_with("/embeddings") {
        trimmed.to_owned()
    } else {
        format!("{trimmed}/embeddings")
    }
}

/// Backward-compatible URL resolver using the Voyage default endpoint.
pub fn voyage_url(base: Option<&str>) -> String {
    embedding_url(Provider::Voyage, base)
}

#[derive(Clone)]
pub struct VoyageClient {
    pub(super) inner: Arc<VoyageInner>,
}

pub(super) struct VoyageInner {
    pub(super) http: Client,
    pub(super) query_http: Client,
    pub(super) provider: Provider,
    pub(super) model: String,
    pub(super) api_keys: Vec<String>,
    pub(super) endpoint: String,
    pub(super) dimensions: Option<u32>,
    pub(super) key_cursor: AtomicUsize,
}

impl VoyageClient {
    pub fn new(model: String, api_keys: Vec<String>, base_url: Option<&str>) -> Result<Self> {
        Self::new_for_provider(Provider::Voyage, model, api_keys, base_url, None)
    }

    pub fn new_for_provider(
        provider: Provider,
        model: String,
        api_keys: Vec<String>,
        base_url: Option<&str>,
        dimensions: Option<u32>,
    ) -> Result<Self> {
        if api_keys.is_empty() {
            bail!("embedding client requires at least one API key");
        }
        let http = Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .context("build reqwest client")?;
        let query_http = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("build query reqwest client")?;
        Ok(Self {
            inner: Arc::new(VoyageInner {
                http,
                query_http,
                provider,
                model,
                api_keys,
                endpoint: embedding_url(provider, base_url),
                dimensions,
                key_cursor: AtomicUsize::new(0),
            }),
        })
    }

    pub fn model(&self) -> &str {
        &self.inner.model
    }

    pub fn dimensions(&self) -> Option<u32> {
        self.inner.dimensions
    }
}

#[cfg(test)]
mod tests;
