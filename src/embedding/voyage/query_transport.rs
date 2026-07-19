use std::sync::atomic::Ordering;

use anyhow::Result;
use tracing::warn;

use super::retry::{EmbedError, backoff_with_jitter};
use crate::embedding::InputType;

use super::VoyageClient;

impl VoyageClient {
    pub async fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        let texts = vec![text.to_string()];
        let key_count = self.inner.api_keys.len();
        let start = self.inner.key_cursor.fetch_add(1, Ordering::Relaxed) % key_count;

        for offset in 0..key_count {
            let key_index = (start + offset) % key_count;
            match self
                .try_embed_query_with_key(&self.inner.api_keys[key_index], &texts, InputType::Query)
                .await
            {
                Ok(mut embeddings) => return pop_query_embedding(&mut embeddings),
                Err(EmbedError::RateLimited) => {
                    warn!(key_index, "VoyageAI 429 on query embed; trying next key");
                }
                Err(EmbedError::Transient(error)) => {
                    return Err(error.context("VoyageAI transient error on query embed"));
                }
                Err(EmbedError::Other(error)) => return Err(error),
            }
        }

        let cursor = self.inner.key_cursor.load(Ordering::Relaxed);
        let delay = backoff_with_jitter(2, cursor);
        warn!(
            delay_ms = delay.as_millis() as u64,
            "all VoyageAI keys rate-limited on query embed; backing off"
        );
        tokio::time::sleep(delay).await;

        for offset in 0..key_count {
            let key_index = (start + offset) % key_count;
            match self
                .try_embed_query_with_key(&self.inner.api_keys[key_index], &texts, InputType::Query)
                .await
            {
                Ok(mut embeddings) => return pop_query_embedding(&mut embeddings),
                Err(EmbedError::RateLimited) => continue,
                Err(EmbedError::Transient(error)) => {
                    return Err(error.context("VoyageAI transient error on query embed"));
                }
                Err(EmbedError::Other(error)) => return Err(error),
            }
        }
        anyhow::bail!("VoyageAI query embed still rate-limited after backoff")
    }
}

fn pop_query_embedding(embeddings: &mut Vec<Vec<f32>>) -> Result<Vec<f32>> {
    embeddings
        .pop()
        .ok_or_else(|| anyhow::anyhow!("VoyageAI returned empty embeddings"))
}
