use crate::parsing::symbols::Symbol;
use tree_sitter::Node;

/// A text chunk ready for embedding.
#[derive(Debug, Clone)]
pub struct Chunk {
    /// Absolute path of the source file.
    pub file: String,
    pub line_start: u32,
    pub line_end: u32,
    pub content: String,
    /// FQN of the deepest-enclosing symbol, if this chunk is cleanly contained
    /// within exactly one symbol (see [`symbol_ref_for`]). `None` when the chunk
    /// straddles a symbol boundary (merged multi-symbol chunk) so caller/callee
    /// stats are never misattributed.
    pub symbol_ref: Option<String>,
}

/// Maximum chunk size in NON-WHITESPACE characters.
///
/// cAST (arXiv 2506.15655) tunes the sweet spot at ~2000 chars; line counts
/// mis-size dense vs sparse code, so we budget by non-whitespace chars. This is
/// the single tunable knob validated by the benchmark harness — if Recall/IoU
/// regress, this is the first dial to turn (within ~1500–2000).
const MAX_CHUNK_NONWS: usize = 1500;

/// Build-level chunker algorithm version. BUMP THIS whenever the chunk shape
/// changes (algorithm, budget, linkage) so the per-file freshness check
/// (`indexing/tracker::detect_changes`) treats stale-version files as modified
/// and lazily re-chunks them on the next trigger — WITHOUT a DB schema bump
/// (schema migrations are in-place transforms and cannot regenerate chunks).
///
/// History:
///   1 = flat per-symbol + 50/25 sliding window (pre-cAST).
///   2 = cAST recursive split-then-merge, non-overlapping, deepest-enclosing
///       symbol_ref, 1500 non-whitespace-char budget.
pub const CHUNKER_VERSION: i64 = 2;

// ─── Non-whitespace size accounting (O(1) range queries) ────────────────────

/// Prefix sum of non-whitespace character counts, indexed by BYTE offset.
/// `prefix[b]` = number of non-whitespace chars strictly before byte `b`.
///
/// Tree-sitter byte offsets are always UTF-8 char boundaries, and every range
/// we query is a char boundary, so interior bytes of multi-byte chars are never
/// read (they stay 0). This makes `nonws(a, b) = prefix[b] - prefix[a]` an O(1)
/// lookup, keeping the recursive split + merge passes linear per file (no
/// repeated O(span) recounting — preserves the no-O(n²) invariant).
fn build_nonws_prefix(source: &str) -> Vec<u32> {
    let mut prefix = vec![0u32; source.len() + 1];
    let mut count = 0u32;
    for (i, ch) in source.char_indices() {
        prefix[i] = count;
        if !ch.is_whitespace() {
            count += 1;
        }
    }
    prefix[source.len()] = count;
    prefix
}

#[inline]
fn nonws(prefix: &[u32], start: usize, end: usize) -> usize {
    (prefix[end] - prefix[start]) as usize
}

// ─── Byte-offset → line mapping (O(log n) via precomputed line starts) ──────

/// Byte offsets at which each (1-indexed) line begins. `line_starts[0] == 0`.
fn build_line_starts(source: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (i, b) in source.bytes().enumerate() {
        if b == b'\n' {
            starts.push(i + 1);
        }
    }
    starts
}

/// 1-indexed line number containing byte offset `off`.
#[inline]
fn line_of(line_starts: &[usize], off: usize) -> u32 {
    // Number of line-starts <= off == line number (1-indexed).
    line_starts.partition_point(|&ls| ls <= off) as u32
}

// ─── A byte span produced by the recursive split, pre-merge ─────────────────

#[derive(Clone, Copy)]
struct Span {
    start: usize,
    end: usize,
}

/// Recursive split (cAST step 1): walk the named children of `node`, greedily
/// accumulating sibling byte-ranges into spans up to the size budget. When a
/// single child alone exceeds the budget, recurse into ITS children to split it
/// further; a leaf that still exceeds the budget (e.g. a giant string literal or
/// a token-less node) is emitted whole rather than truncated mid-token.
///
/// `lo`/`hi` bound the region of `node` not yet consumed by an emitted child, so
/// the text BETWEEN large children (punctuation, braces, blank lines) is still
/// covered exactly once — guaranteeing full coverage with no gaps and no overlap.
fn split_node(node: Node, source_len: usize, prefix: &[u32], out: &mut Vec<Span>) {
    let node_start = node.start_byte();
    let node_end = node.end_byte().min(source_len);
    if node_start >= node_end {
        return;
    }

    // Leaf or small-enough node: emit whole.
    if nonws(prefix, node_start, node_end) <= MAX_CHUNK_NONWS || node.named_child_count() == 0 {
        out.push(Span { start: node_start, end: node_end });
        return;
    }

    // Oversized internal node: walk named children, packing siblings greedily and
    // recursing into any child that alone exceeds the budget. `cursor_pos` tracks
    // the byte just past the last emitted content so inter-child gaps are kept.
    let mut acc: Option<Span> = None;
    let mut cursor_pos = node_start;
    let mut tc = node.walk();
    for child in node.named_children(&mut tc) {
        let c_start = child.start_byte();
        let c_end = child.end_byte().min(source_len);
        if c_start >= c_end {
            continue;
        }
        // Region from cursor_pos..c_start is gap text (delimiters/comments between
        // children). Fold it into the accumulator so it is never dropped.
        let seg_start = cursor_pos.min(c_start);

        if nonws(prefix, c_start, c_end) > MAX_CHUNK_NONWS {
            // This child alone is too big: flush the accumulator (including any
            // gap text up to the child), then recurse into the child.
            if let Some(a) = acc.take() {
                out.push(Span { start: a.start, end: seg_start.max(a.end).min(c_start) });
            } else if seg_start < c_start {
                // Gap text with no accumulator — emit it so coverage is complete.
                out.push(Span { start: seg_start, end: c_start });
            }
            split_node(child, source_len, prefix, out);
            cursor_pos = c_end;
            continue;
        }

        // Small child: try to extend the accumulator; if adding it would bust the
        // budget, flush and start a new accumulator at the gap boundary.
        match acc {
            None => acc = Some(Span { start: seg_start, end: c_end }),
            Some(a) => {
                if nonws(prefix, a.start, c_end) <= MAX_CHUNK_NONWS {
                    acc = Some(Span { start: a.start, end: c_end });
                } else {
                    out.push(a);
                    acc = Some(Span { start: a.end, end: c_end });
                }
            }
        }
        cursor_pos = c_end;
    }
    // Flush the final accumulator plus any trailing gap up to node_end.
    if let Some(a) = acc {
        out.push(Span { start: a.start, end: node_end.max(a.end) });
    } else if cursor_pos < node_end {
        out.push(Span { start: cursor_pos, end: node_end });
    }
}

/// Merge pass (cAST step 2 — MANDATORY): coalesce adjacent spans whose combined
/// non-whitespace size stays within budget. Split-only over-fragments the index
/// and degrades ranking (cAST: nDCG 85→66), so this step is not optional. Spans
/// arrive in source order from `split_node`; merging adjacent ones preserves
/// non-overlapping, gap-free coverage.
fn merge_spans(mut spans: Vec<Span>, prefix: &[u32]) -> Vec<Span> {
    if spans.is_empty() {
        return spans;
    }
    // Defensive: ensure source order (split_node already emits in order, but a
    // recursion boundary could interleave — sort keeps merge correct & cheap).
    spans.sort_unstable_by_key(|s| s.start);
    let mut merged: Vec<Span> = Vec::with_capacity(spans.len());
    let mut cur = spans[0];
    for s in spans.into_iter().skip(1) {
        // Adjacent (or touching) and within budget → merge.
        if s.start >= cur.end && nonws(prefix, cur.start, s.end) <= MAX_CHUNK_NONWS {
            cur.end = s.end;
        } else {
            merged.push(cur);
            // Start the next run at the larger of cur.end / s.start so we never
            // overlap the chunk just pushed (a forced split boundary may leave
            // s.start < cur.end after recursion).
            cur = Span { start: cur.end.max(s.start), end: s.end.max(cur.end) };
        }
    }
    merged.push(cur);
    merged
}

// ─── symbol_ref linkage: deepest-enclosing (c-contained) ────────────────────

/// Choose the `symbol_ref` for a chunk spanning lines `[ls, le]` using the
/// DEEPEST-ENCLOSING ("c-contained") rule. Decision (see design D5) is driven by
/// the measured symbols-per-chunk histogram on notepad-ade — 45% of baseline
/// chunks covered 2+ symbols, so "largest-overlap" would mislabel nearly half
/// the index; deepest-enclosing labels only chunks that lie cleanly within one
/// symbol's body and returns `None` for true multi-symbol straddles.
///
/// Rules:
///  - The chunk must be fully contained (`sym.start <= ls && le <= sym.end`) in a
///    candidate symbol. Among all containing symbols (e.g. a class and a method
///    inside it), the DEEPEST (smallest line span) wins — that is the most
///    specific scope. A class-declaration chunk that contains member symbols is
///    itself contained only by the class, so it gets the CONTAINER's FQN
///    (container/skeleton rule, D5).
///  - Split fragments of one big function are each fully inside that function →
///    every fragment inherits the function's FQN → caller-stats stay exact.
///  - A merged chunk that is NOT fully inside any single symbol (it straddles
///    two siblings, or spills past a symbol's end) → `None` (never mislabeled).
fn symbol_ref_for(ls: u32, le: u32, symbols: &[Symbol]) -> Option<String> {
    symbols
        .iter()
        .filter(|s| s.line_start <= ls && le <= s.line_end)
        // Deepest = smallest enclosing line span. Ties (identical ranges) are
        // broken toward the LAST in extraction order, which is the inner symbol
        // (children are pushed after parents in the recursive extractors).
        .min_by_key(|s| (s.line_end - s.line_start, u32::MAX - s.line_start))
        .map(|s| s.qualified.fqn())
}

// ─── Public entry points ────────────────────────────────────────────────────

/// AST-aware recursive split-then-merge chunker (cAST). Called from INSIDE the
/// rayon parse closure in `parsing/mod.rs` because `tree_sitter::Tree`/`Node`
/// are not `Send` — the tree is consumed here and only the owned `Vec<Chunk>`
/// leaves the closure (never the tree).
///
/// Produces NON-OVERLAPPING, gap-free chunks: every source line lands in exactly
/// one chunk except at a forced split boundary inside an oversized leaf. The old
/// flat strategy (one full-body chunk per symbol PLUS a 50/25 sliding window)
/// is gone — it duplicated container bodies and emitted windows that straddled
/// function boundaries (the `pipeline.rs#L2026-2075` cut-through defect).
pub fn chunk_file_ast(
    file: &str,
    source: &str,
    root: Node,
    symbols: &[Symbol],
) -> Vec<Chunk> {
    if source.is_empty() {
        return vec![];
    }
    let prefix = build_nonws_prefix(source);
    let line_starts = build_line_starts(source);

    let mut spans = Vec::new();
    split_node(root, source.len(), &prefix, &mut spans);
    let spans = merge_spans(spans, &prefix);

    spans_to_chunks(file, source, &line_starts, &spans, symbols)
}

/// Source-only fallback used when there is no usable tree-sitter tree (parse
/// failure, or `Lang::Other`). Splits the file into non-overlapping line windows
/// budgeted by non-whitespace chars — no overlap, full coverage, AST-free.
pub fn chunk_file(file: &str, source: &str, symbols: &[Symbol]) -> Vec<Chunk> {
    if source.is_empty() {
        return vec![];
    }
    let prefix = build_nonws_prefix(source);
    let line_starts = build_line_starts(source);

    // Greedily pack whole lines into spans up to the budget. Whole-line spans
    // keep us off mid-token boundaries without a tree.
    let mut spans: Vec<Span> = Vec::new();
    let n_lines = line_starts.len();
    let mut i = 0usize;
    while i < n_lines {
        let span_start = line_starts[i];
        let mut j = i;
        // Extend while the next line keeps us within budget.
        while j + 1 < n_lines {
            let cand_end = line_starts[j + 1];
            if nonws(&prefix, span_start, cand_end) > MAX_CHUNK_NONWS && j > i {
                break;
            }
            j += 1;
        }
        let span_end = if j + 1 < n_lines { line_starts[j + 1] } else { source.len() };
        spans.push(Span { start: span_start, end: span_end });
        i = j + 1;
    }

    spans_to_chunks(file, source, &line_starts, &spans, symbols)
}

/// Convert byte spans into `Chunk`s: trim to char boundaries already guaranteed
/// by tree-sitter/line offsets, compute 1-indexed line ranges, assign
/// `symbol_ref` via deepest-enclosing, and drop blank/whitespace-only chunks.
fn spans_to_chunks(
    file: &str,
    source: &str,
    line_starts: &[usize],
    spans: &[Span],
    symbols: &[Symbol],
) -> Vec<Chunk> {
    let mut chunks = Vec::with_capacity(spans.len());
    for sp in spans {
        let start = sp.start.min(source.len());
        let end = sp.end.min(source.len());
        if start >= end {
            continue;
        }
        let content = &source[start..end];
        // Guard: never emit a blank/whitespace-only chunk. The pipeline relies on
        // a strict 1:1 positional alignment between this chunk list and the
        // embedding vectors, so the filter MUST live here (not at the embed
        // layer, which would desync the zip and corrupt stored data).
        if content.trim().is_empty() {
            continue;
        }
        let line_start = line_of(line_starts, start);
        // The last byte of the span determines the end line. For an end that sits
        // exactly on a line boundary (points at a '\n' start), step back one byte
        // so a span ending at the newline doesn't bleed into the next line.
        let last_byte = end - 1;
        let line_end = line_of(line_starts, last_byte);
        let symbol_ref = symbol_ref_for(line_start, line_end, symbols);
        chunks.push(Chunk {
            file: file.to_string(),
            line_start,
            line_end,
            content: content.to_string(),
            symbol_ref,
        });
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parsing::symbols::{QualifiedSymbol, Symbol, SymbolKind};
    use tree_sitter::Parser;

    fn make_symbol(file: &str, name: &str, line_start: u32, line_end: u32) -> Symbol {
        Symbol {
            qualified: QualifiedSymbol {
                file: file.to_string(),
                scope_path: vec![],
                name: name.to_string(),
            },
            kind: SymbolKind::Function,
            line_start,
            line_end,
            signature: None,
            parent_fqn: None,
        }
    }

    fn parse_cpp(source: &str) -> tree_sitter::Tree {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_cpp::LANGUAGE.into())
            .unwrap();
        parser.parse(source, None).unwrap()
    }

    fn chunk_cpp(file: &str, source: &str, symbols: &[Symbol]) -> Vec<Chunk> {
        let tree = parse_cpp(source);
        chunk_file_ast(file, source, tree.root_node(), symbols)
    }

    // ─── size accounting ────────────────────────────────────────────────

    #[test]
    fn nonws_prefix_counts_only_nonwhitespace() {
        let src = "a b\tc\nd";
        let prefix = build_nonws_prefix(src);
        // 4 non-whitespace chars total: a, b, c, d.
        assert_eq!(nonws(&prefix, 0, src.len()), 4);
    }

    // ─── blank / whitespace handling (preserved from old behavior) ───────

    #[test]
    fn blank_file_yields_no_chunks() {
        let source = "\n   \n\t\n\n";
        let chunks = chunk_cpp("empty.cpp", source, &[]);
        assert!(chunks.is_empty(), "expected no chunks, got {}", chunks.len());
    }

    #[test]
    fn no_blank_content_chunks() {
        let mut source = String::from("void foo() {\n    return;\n}\n");
        for _ in 0..60 {
            source.push('\n');
        }
        source.push_str("int x = 1;\n");
        let chunks = chunk_cpp("test.cpp", &source, &[]);
        for c in &chunks {
            assert!(
                !c.content.trim().is_empty(),
                "chunk at {}-{} is blank",
                c.line_start,
                c.line_end
            );
        }
    }

    // ─── cAST: non-overlap + full coverage ───────────────────────────────

    #[test]
    fn chunks_are_non_overlapping_and_cover_all_code_lines() {
        // Several small functions: should be packed, never overlapping.
        let mut source = String::new();
        for i in 0..8 {
            source.push_str(&format!("int f{i}(int x) {{\n    return x + {i};\n}}\n"));
        }
        let chunks = chunk_cpp("multi.cpp", &source, &[]);
        assert!(!chunks.is_empty());
        // Sort by line_start and assert no overlap (next.start > prev.end).
        let mut ranges: Vec<(u32, u32)> =
            chunks.iter().map(|c| (c.line_start, c.line_end)).collect();
        ranges.sort_unstable();
        for w in ranges.windows(2) {
            assert!(
                w[1].0 > w[0].1,
                "overlap: chunk ending {} followed by chunk starting {}",
                w[0].1,
                w[1].0
            );
        }
    }

    #[test]
    fn small_siblings_are_packed_not_one_chunk_each() {
        // 8 tiny functions fit well under one 1500-nonws budget → should merge to
        // far fewer chunks than 8 (ideally 1).
        let mut source = String::new();
        for i in 0..8 {
            source.push_str(&format!("int f{i}() {{ return {i}; }}\n"));
        }
        let chunks = chunk_cpp("packed.cpp", &source, &[]);
        assert!(
            chunks.len() < 8,
            "expected siblings packed, got {} chunks",
            chunks.len()
        );
    }

    #[test]
    fn oversized_function_is_split_by_recursion() {
        // One function with a body far exceeding the budget → must produce >1
        // chunk, each within budget, none truncated mid-token (we just check the
        // budget + count here; mid-token is guaranteed by byte-range alignment).
        let mut body = String::from("void big() {\n");
        // ~3000 non-whitespace chars of statements.
        for i in 0..400 {
            body.push_str(&format!("    int v{i}=compute_value_number_{i}();\n"));
        }
        body.push_str("}\n");
        let chunks = chunk_cpp("big.cpp", &body, &[]);
        assert!(chunks.len() > 1, "oversized fn must split, got {}", chunks.len());
        for c in &chunks {
            let nws = c.content.chars().filter(|ch| !ch.is_whitespace()).count();
            // Allow a single oversized leaf to exceed, but statement-level split
            // should keep all chunks within ~budget. Assert a generous ceiling.
            assert!(
                nws <= MAX_CHUNK_NONWS * 2,
                "chunk {}-{} has {} nonws chars (budget {})",
                c.line_start,
                c.line_end,
                nws,
                MAX_CHUNK_NONWS
            );
        }
    }

    // ─── symbol_ref deepest-enclosing linkage ────────────────────────────

    #[test]
    fn single_symbol_chunk_gets_its_fqn() {
        let source = "int only() {\n    return 1;\n}\n";
        let sym = make_symbol("one.cpp", "only", 1, 3);
        let chunks = chunk_cpp("one.cpp", source, &[sym]);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].symbol_ref.as_deref(), Some("one.cpp::only"));
    }

    #[test]
    fn split_fragments_inherit_function_fqn() {
        // One big function spanning many lines, forced to split; both fragments
        // must carry the function FQN (caller-stats exact).
        let mut source = String::from("void big() {\n");
        for i in 0..400 {
            source.push_str(&format!("    int v{i}=compute_value_number_{i}();\n"));
        }
        source.push_str("}\n");
        let total_lines = source.lines().count() as u32;
        let sym = make_symbol("big.cpp", "big", 1, total_lines);
        let chunks = chunk_cpp("big.cpp", &source, &[sym]);
        assert!(chunks.len() > 1, "expected split, got {}", chunks.len());
        for c in &chunks {
            assert_eq!(
                c.symbol_ref.as_deref(),
                Some("big.cpp::big"),
                "fragment {}-{} lost its FQN",
                c.line_start,
                c.line_end
            );
        }
    }

    #[test]
    fn merged_multi_symbol_chunk_is_none() {
        // Two tiny sibling functions merged into one chunk → straddles both →
        // symbol_ref must be None (not mislabeled with either).
        let source = "int a() { return 1; }\nint b() { return 2; }\n";
        let sa = make_symbol("two.cpp", "a", 1, 1);
        let sb = make_symbol("two.cpp", "b", 2, 2);
        let chunks = chunk_cpp("two.cpp", source, &[sa, sb]);
        // They should merge into a single chunk covering both.
        let merged = chunks.iter().find(|c| c.line_start <= 1 && c.line_end >= 2);
        assert!(merged.is_some(), "expected a merged chunk covering both fns");
        assert_eq!(
            merged.unwrap().symbol_ref,
            None,
            "merged multi-symbol chunk must be None, got {:?}",
            merged.unwrap().symbol_ref
        );
    }

    #[test]
    fn container_chunk_gets_container_fqn_via_deepest_enclosing() {
        // symbol_ref_for unit test: a chunk fully inside a method (deepest) beats
        // the enclosing class.
        let class_sym = make_symbol("c.cpp", "Widget", 1, 20);
        let mut method = make_symbol("c.cpp", "draw", 5, 10);
        method.kind = SymbolKind::Method;
        let syms = vec![class_sym, method];
        // Chunk fully inside the method → deepest = draw.
        assert_eq!(
            symbol_ref_for(6, 9, &syms).as_deref(),
            Some("c.cpp::draw")
        );
        // Chunk that is the class declaration line (outside the method) → only the
        // class contains it → container FQN.
        assert_eq!(
            symbol_ref_for(2, 3, &syms).as_deref(),
            Some("c.cpp::Widget")
        );
        // Chunk straddling the method end (spills to line 15, still inside class
        // but NOT inside method) → class FQN (deepest fully-containing symbol).
        assert_eq!(
            symbol_ref_for(8, 15, &syms).as_deref(),
            Some("c.cpp::Widget")
        );
        // Chunk extending past the class → contained by nothing → None.
        assert_eq!(symbol_ref_for(18, 25, &syms), None);
    }
}
