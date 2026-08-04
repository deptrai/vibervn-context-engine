use std::sync::atomic::Ordering;

use anyhow::Result;
use tracing::{info, warn};

use crate::embedding::InputType;

use super::VoyageClient;
use super::batching::byte_aware_batches;
use super::retry::{
    EmbedError, TRANSIENT_RETRY_LIMIT, TransientEmbedExhausted, backoff_with_jitter,
};

impl VoyageClient {
    pub async fn embed(&self, texts: &[String], input_type: InputType) -> Result<Vec<Vec<f32>>> {
        let mut all = Vec::with_capacity(texts.len());
        for batch in byte_aware_batches(texts) {
            all.extend(self.embed_batch(batch, input_type).await?);
        }
        Ok(all)
    }

    pub async fn embed_batch(
        &self,
        texts: &[String],
        input_type: InputType,
    ) -> Result<Vec<Vec<f32>>> {
        let key_count = self.inner.api_keys.len();
        let mut transient_attempts = 0;
        loop {
            let start = self.inner.key_cursor.fetch_add(1, Ordering::Relaxed) % key_count;
            for offset in 0..key_count {
                let key_index = (start + offset) % key_count;
                match self
                    .try_embed_with_key(&self.inner.api_keys[key_index], texts, input_type)
                    .await
                {
                    Ok(embeddings) => return Ok(embeddings),
                    Err(EmbedError::RateLimited) => {
                        warn!(key_index, "VoyageAI 429; trying next key");
                    }
                    Err(EmbedError::Transient(error)) => {
                        transient_attempts += 1;
                        if transient_attempts >= TRANSIENT_RETRY_LIMIT {
                            return Err(error.context(TransientEmbedExhausted {
                                attempts: transient_attempts,
                            }));
                        }
                        sleep_for_transient(self, transient_attempts, &error).await;
                        break;
                    }
                    Err(EmbedError::Other(error)) => return Err(error),
                }
            }

            if transient_attempts == 0 {
                let mut delay_secs = 2;
                loop {
                    sleep_for_rate_limit(self, delay_secs).await;
                    let start = self.inner.key_cursor.fetch_add(1, Ordering::Relaxed) % key_count;
                    let mut saw_transient = false;
                    for offset in 0..key_count {
                        let key_index = (start + offset) % key_count;
                        match self
                            .try_embed_with_key(&self.inner.api_keys[key_index], texts, input_type)
                            .await
                        {
                            Ok(embeddings) => {
                                info!("VoyageAI embed succeeded after backoff");
                                return Ok(embeddings);
                            }
                            Err(EmbedError::RateLimited) => continue,
                            Err(EmbedError::Transient(error)) => {
                                transient_attempts += 1;
                                if transient_attempts >= TRANSIENT_RETRY_LIMIT {
                                    return Err(error.context(TransientEmbedExhausted {
                                        attempts: transient_attempts,
                                    }));
                                }
                                saw_transient = true;
                                break;
                            }
                            Err(EmbedError::Other(error)) => return Err(error),
                        }
                    }
                    if saw_transient {
                        break;
                    }
                    delay_secs = (delay_secs * 2).min(60);
                }
            }
        }
    }
}

async fn sleep_for_transient(client: &VoyageClient, attempt: usize, error: &anyhow::Error) {
    let cursor = client.inner.key_cursor.load(Ordering::Relaxed);
    let delay = backoff_with_jitter(2u64.pow(attempt as u32).min(16), cursor);
    warn!(attempt, max_attempts = TRANSIENT_RETRY_LIMIT, delay_ms = delay.as_millis() as u64, error = %error, "VoyageAI transient error; retrying after backoff");
    tokio::time::sleep(delay).await;
}

async fn sleep_for_rate_limit(client: &VoyageClient, delay_secs: u64) {
    let cursor = client.inner.key_cursor.load(Ordering::Relaxed);
    let delay = backoff_with_jitter(delay_secs, cursor);
    warn!(
        delay_ms = delay.as_millis() as u64,
        "all VoyageAI keys rate-limited; backing off with jitter"
    );
    tokio::time::sleep(delay).await;
}
