use crate::parsing::symbols::Symbol;

/// A text chunk ready for embedding.
#[derive(Debug, Clone)]
pub struct Chunk {
    /// Absolute path of the source file.
    pub file: String,
    pub line_start: u32,
    pub line_end: u32,
    pub content: String,
    /// FQN of the containing symbol, if this chunk came from a symbol body.
    pub symbol_ref: Option<String>,
}

const WINDOW: u32 = 50;
const STRIDE: u32 = 25;

/// Produce chunks for a source file.
///
/// Strategy:
/// 1. **Symbol chunks** — each symbol body becomes one chunk (linked via `symbol_ref`).
/// 2. **Coverage chunks** — sliding window (50 lines, 25-line stride) for lines NOT
///    already fully covered by a symbol chunk. `symbol_ref = None`.
///
/// Lines are 1-indexed.
pub fn chunk_file(file: &str, source: &str, symbols: &[Symbol]) -> Vec<Chunk> {
    let lines: Vec<&str> = source.lines().collect();
    let total_lines = lines.len() as u32;
    if total_lines == 0 {
        return vec![];
    }

    let mut chunks = Vec::new();

    // Build symbol chunks and collect covered line ranges.
    // A "covered" line is one whose range is fully within a symbol chunk.
    let mut symbol_covered: Vec<bool> = vec![false; total_lines as usize];

    for sym in symbols {
        let start = sym.line_start.saturating_sub(1); // 0-indexed
        let end = (sym.line_end).min(total_lines).saturating_sub(1); // 0-indexed inclusive
        if start > end || start >= total_lines {
            continue;
        }
        let content = lines[start as usize..=(end as usize)].join("\n");
        chunks.push(Chunk {
            file: file.to_string(),
            line_start: start + 1,
            line_end: end + 1,
            content,
            symbol_ref: Some(sym.qualified.fqn()),
        });
        for i in start..=end {
            if (i as usize) < symbol_covered.len() {
                symbol_covered[i as usize] = true;
            }
        }
    }

    // Sliding window over uncovered lines.
    let mut window_start: u32 = 0;
    while window_start < total_lines {
        let window_end = (window_start + WINDOW - 1).min(total_lines - 1);

        // Check if this window overlaps any uncovered line.
        let has_uncovered = (window_start..=window_end)
            .any(|i| !symbol_covered[i as usize]);

        if has_uncovered {
            let content = lines[window_start as usize..=window_end as usize].join("\n");
            chunks.push(Chunk {
                file: file.to_string(),
                line_start: window_start + 1,
                line_end: window_end + 1,
                content,
                symbol_ref: None,
            });
        }

        window_start += STRIDE;
    }

    chunks
}
