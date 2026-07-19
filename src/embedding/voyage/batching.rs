use super::{MAX_BATCH_BYTES, MAX_BATCH_SIZE};

/// Split texts into sub-slices where each batch has at most `MAX_BATCH_SIZE`
/// texts AND the sum of `text.len()` stays under `MAX_BATCH_BYTES`. A single
/// text exceeding the byte cap is sent alone (VoyageAI will truncate or reject
/// at the token level, but it won't poison the whole batch).
pub(super) fn byte_aware_batches(texts: &[String]) -> Vec<&[String]> {
    let mut batches = Vec::new();
    let mut start = 0;
    while start < texts.len() {
        let mut end = start;
        let mut batch_bytes = 0usize;
        while end < texts.len()
            && end - start < MAX_BATCH_SIZE
            && (batch_bytes + texts[end].len() <= MAX_BATCH_BYTES || end == start)
        {
            batch_bytes += texts[end].len();
            end += 1;
        }
        batches.push(&texts[start..end]);
        start = end;
    }
    batches
}
