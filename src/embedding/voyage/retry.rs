use std::time::Duration;

/// Classify whether a `reqwest::Error` from the `.send()` phase is transient.
///
/// The rule is simple: if no valid HTTP response was received, the failure is
/// transport-level and inherently transient (timeout, connection refused,
/// mid-flight reset/os error 10054, DNS resolution, TLS handshake error, proxy
/// error, etc.). The ONLY non-transient send error is `is_builder()`, which
/// indicates a programming mistake constructing the request (invalid URL, bad
/// header value) — that will never self-heal on retry.
///
/// This function is factored out as a pure predicate so it can be unit-tested
/// without requiring a live socket or specific OS error. The downstream retry
/// loop (embed_batch) retries Transient up to TRANSIENT_RETRY_LIMIT times with
/// exponential backoff; if exhausted, the file is skipped (not the whole run).
#[inline]
pub(super) fn is_send_error_transient(e: &reqwest::Error) -> bool {
    // is_builder() = config/programming error building the Request struct itself.
    // Everything else from .send() is a transport failure: network unreachable,
    // connection refused, connection reset (10054), timeout, DNS failure,
    // TLS error, proxy errors, redirect loops, etc.
    !e.is_builder()
}

pub(super) enum EmbedError {
    RateLimited,
    /// Transient network error (timeout, connection refused/reset, mid-flight
    /// connection close, body-read failure) — retryable with bounded attempts.
    Transient(anyhow::Error),
    Other(anyhow::Error),
}

/// Marker error type wrapped around a transient embed failure that exhausted
/// all retry attempts. Carried inside the `anyhow::Error` chain so callers
/// can distinguish "transient/exhausted-retry" from "fatal/config" errors via
/// `err.downcast_ref::<TransientEmbedExhausted>()`.
///
/// A transient-exhausted failure for a single file is NON-FATAL to the pipeline:
/// crash-safe `file_meta` means the file is simply not committed and will be
/// retried on the next index trigger (self-healing). This distinction prevents
/// a single gateway timeout from aborting an entire 79K-file Linux kernel rebuild.
#[derive(Debug)]
pub struct TransientEmbedExhausted {
    pub attempts: usize,
}

impl std::fmt::Display for TransientEmbedExhausted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "transient embed error exhausted after {} attempts",
            self.attempts
        )
    }
}

impl std::error::Error for TransientEmbedExhausted {}

/// Maximum number of retry attempts for transient network errors (timeout,
/// connection failures). After this many attempts, the error propagates and
/// the file is left un-embedded (resumable via file_meta on next trigger).
///
/// Set to 6 to ride out multi-second gateway blips on large-repo runs (e.g.
/// Linux kernel 79K files through a shared gateway). With exponential backoff
/// capped at 16s, 6 attempts ≈ 2+4+8+16+16 = 46s worst-case per file — long
/// enough for most transient outages without stalling the pipeline indefinitely.
pub(super) const TRANSIENT_RETRY_LIMIT: usize = 6;

/// Test-visible alias for TRANSIENT_RETRY_LIMIT so pipeline tests can assert
/// the value without duplicating the constant.
#[cfg(test)]
pub const TRANSIENT_RETRY_LIMIT_FOR_TEST: usize = TRANSIENT_RETRY_LIMIT;

/// Compute a backoff duration with jitter for retry loops. Uses a lightweight
/// deterministic jitter derived from the atomic key cursor to de-correlate
/// concurrent embed tasks without pulling in the `rand` crate.
pub(super) fn backoff_with_jitter(base_secs: u64, cursor_val: usize) -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos())
        .unwrap_or(0) as u64;
    let entropy = (cursor_val as u64).wrapping_add(nanos);
    let max_jitter_ms = (base_secs * 250).max(100);
    let jitter_ms = entropy % max_jitter_ms;
    Duration::from_secs(base_secs) + Duration::from_millis(jitter_ms)
}
