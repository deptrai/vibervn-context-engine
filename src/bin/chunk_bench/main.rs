//! Chunking-quality benchmark harness (Gate 0).
//!
//! WHY this exists: the cAST chunker change must be proven empirically, not
//! assumed. This binary measures the metrics that gate the change:
//!   1. duplication ratio   = chunk_chars / source_chars  (lower is better)
//!   2. symbols-per-chunk histogram (the decision input for the symbol_ref
//!      linkage heuristic: largest-overlap vs deepest-enclosing)
//!   3. window-over-symbol win-rate = % of top-k retrieval results whose line
//!      range straddles a symbol boundary (the cut-through defect; ~0 wanted)
//!   4. retrieval Recall@k + IoU against a fixed eval set of
//!      (query -> expected file:symbol) pairs derived from notepad-ade.
//!
//! Metrics (1)+(2) are computed IN-PROCESS from the *currently compiled*
//! chunker via `parse_file` — so the same binary, rebuilt, measures whichever
//! chunker is in the tree. Metrics (3)+(4) drive queries through the LIVE
//! server (`/api/query`) so they reflect the index actually on disk.
//!
//! The eval set is anchored on real SYMBOL NAMES and resolved to line ranges
//! through the FROZEN symbol extraction (`parse_file`), never through chunk
//! boundaries — so it does not favour either chunker.
//!
//! Usage:
//!   chunk_bench <repo_path> <server_url> <out.json> [--label NAME] [--no-retrieval] [--legacy]
//!   chunk_bench <repo_path> <legacy_url> <ab_out.json> --ab --new-server <new_url>
//!
//! Example (single):
//!   chunk_bench d:/projects/cpp/notepad-ade http://localhost:6699 baseline.json --label baseline
//!
//! Example (A/B — the reproducible same-code-path comparison):
//!   chunk_bench d:/projects/cpp/notepad-ade http://localhost:7801 ab_benchmark.json \
//!       --ab --new-server http://localhost:7802
//!
//! `--ab` mode fixes the original methodology hole: the archived baseline
//! retrieval row was measured on the OLD on-disk index which the cAST rebuild
//! then overwrote, so it was neither reproducible nor apples-to-apples. In A/B
//! mode BOTH rows are produced in ONE run by the IDENTICAL harness + query path,
//! each against a REAL fresh index built by its OWN chunker (legacy server =
//! git-HEAD build, new server = working-tree build), with identical frozen eval
//! set, identical chunker-independent ground-truth, and identical rerank=false.
//! See `scripts/ab_bench.sh` for the orchestration that boots both servers on
//! isolated temp ports + temp data dirs and cleans them up.

use std::collections::BTreeMap;

use context_engine_rs::indexing::walker::walk_repo;
use context_engine_rs::parsing::parse_file;
use context_engine_rs::parsing::symbols::Symbol;

mod eval;
use eval::eval_set;

// ─── Output report shape ─────────────────────────────────────────────────

#[derive(serde::Serialize, serde::Deserialize, Debug, Default)]
struct Report {
    label: String,
    repo: String,
    // (1) duplication
    chunk_chars: u64,
    source_chars: u64,
    duplication_ratio: f64,
    // chunk-shape sanity
    files_parsed: u64,
    total_chunks: u64,
    max_chunk_nonws: u64,
    overlapping_line_chunks: u64, // chunks that overlap another chunk in the same file
    // chunk-level window-over-symbol cut-through: a chunk whose first line and
    // last line sit in DISJOINT enclosing-symbol sets (the sliding-window defect
    // that straddles a function boundary). Measured directly on chunker output,
    // independent of the query-side merger (which is out of scope).
    cut_through_chunks: u64,
    cut_through_chunk_rate: f64,
    // (2) symbols-per-chunk histogram: buckets "0","1","2","3+"
    symbols_per_chunk: BTreeMap<String, u64>,
    // (3) retrieval — cut-through
    eval_pairs: u64,
    queries_run: u64,
    topk_results_total: u64,
    cut_through_results: u64,
    cut_through_rate: f64,
    // (4) retrieval — recall / IoU
    recall_at_1: f64,
    recall_at_5: f64,
    recall_at_10: f64,
    mean_iou: f64,
}

// ─── A/B comparison report (reproducible same-code-path side-by-side) ────────

/// Side-by-side legacy-vs-new comparison emitted by `--ab` mode.
///
/// WHY this exists: the archived `baseline.json` retrieval row was measured on
/// the OLD on-disk index, which the cAST rebuild then OVERWROTE — so it was not
/// reproducible and not apples-to-apples. This artifact fixes that: both rows
/// are produced in ONE run, by the IDENTICAL harness + query path, against a
/// REAL fresh index built by EACH chunker, with the SAME frozen eval set, SAME
/// ground-truth resolution (`resolve_expected_range`, chunker-independent), and
/// the SAME `rerank=false` setting. Anyone can re-run `reproduce_cmd` and get
/// the same numbers.
#[derive(serde::Serialize, Debug)]
struct AbReport {
    repo: String,
    /// How the two indexes were produced (see chosen mechanism in the report).
    mechanism: String,
    /// Frozen eval set size and query-path knobs shared by BOTH rows.
    eval_pairs: u64,
    rerank: bool,
    top_k: u64,
    /// Endpoints the retrieval eval ran against (for provenance / re-run).
    legacy_server: String,
    new_server: String,
    /// new − legacy on the metrics the gate cares about. Positive recall/IoU
    /// deltas mean the new chunker genuinely beats legacy under this rigorous
    /// setup; negative means it regressed (reported honestly either way).
    deltas: AbDeltas,
    /// The exact command that reproduces this artifact end to end.
    reproduce_cmd: String,
    legacy: Report,
    new: Report,
}

#[derive(serde::Serialize, Debug, Default)]
struct AbDeltas {
    recall_at_1: f64,
    recall_at_5: f64,
    recall_at_10: f64,
    mean_iou: f64,
    duplication_ratio: f64,
    cut_through_chunk_rate: f64,
    total_chunks: i64,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!(
            "usage: chunk_bench <repo_path> <server_url> <out.json> [--label NAME] [--no-retrieval] [--legacy]\n       chunk_bench <repo_path> <legacy_server_url> <ab_out.json> --ab --new-server <new_server_url>"
        );
        std::process::exit(2);
    }
    let repo = args[1].clone();
    let server = args[2].trim_end_matches('/').to_string();
    let out_path = args[3].clone();
    let mut label = "unlabeled".to_string();
    let mut do_retrieval = true;
    let mut legacy = false;
    let mut ab = false;
    let mut new_server: Option<String> = None;
    let mut i = 4;
    while i < args.len() {
        match args[i].as_str() {
            "--label" => {
                i += 1;
                if i < args.len() {
                    label = args[i].clone();
                }
            }
            "--no-retrieval" => do_retrieval = false,
            // Compute chunk-shape metrics from the LEGACY (pre-cAST) chunker
            // reproduction instead of the compiled chunker. Lets one binary
            // produce both baseline and post-change chunk metrics with identical
            // measurement code (apples-to-apples cut-through comparison).
            "--legacy" => legacy = true,
            // A/B mode: produce a single side-by-side comparison artifact. The
            // positional <server_url> is the LEGACY server; --new-server is the
            // cAST server. See `run_ab`.
            "--ab" => ab = true,
            "--new-server" => {
                i += 1;
                if i < args.len() {
                    new_server = Some(args[i].trim_end_matches('/').to_string());
                }
            }
            other => eprintln!("warning: ignoring unknown arg {other}"),
        }
        i += 1;
    }

    if ab {
        let new_server = new_server.unwrap_or_else(|| {
            eprintln!("[chunk_bench] --ab requires --new-server <url>");
            std::process::exit(2);
        });
        run_ab(&repo, &server, &new_server, &out_path);
        return;
    }

    let mut report = Report {
        label,
        repo: repo.clone(),
        ..Default::default()
    };

    eprintln!(
        "[chunk_bench] computing chunk-shape metrics on {repo} (chunker={}) ...",
        if legacy { "legacy" } else { "current" }
    );
    compute_chunk_metrics(&repo, legacy, &mut report);

    if do_retrieval {
        eprintln!("[chunk_bench] running retrieval eval against {server} ...");
        run_retrieval(&repo, &server, &mut report);
    }

    let json = serde_json::to_string_pretty(&report).expect("serialize report");
    std::fs::write(&out_path, &json).expect("write report");
    eprintln!("[chunk_bench] wrote {out_path}");
    println!("{json}");
}

/// A/B orchestration: build BOTH rows in one run with identical measurement code
/// and emit a single side-by-side artifact.
///
/// Mechanism (Design B — dual-server via git worktree): `legacy_server` is a
/// context-engine built from the submodule's git HEAD (the pre-cAST chunker) on
/// an isolated port + temp data dir; `new_server` is the working-tree (cAST)
/// build on its own port + temp data dir. Both indexes are REAL, freshly built
/// by the production `IndexPipeline`, never the currently-running :6699 index.
///
/// This binary is the NEW build, so it can compute BOTH chunk-shape rows itself.
/// The legacy row uses the `legacy_chunk_ranges` reproduction (byte-identical to
/// the HEAD chunker, verified against git HEAD), and the new row uses the
/// compiled cAST chunker via `parse_file` — both measured by the same metric
/// loop. Retrieval rows for each row hit their OWN server, so each Recall@k/IoU
/// is measured against the index that server actually built. Ground-truth
/// (`resolve_expected_range`) is chunker-independent — identical for both rows.
fn run_ab(repo: &str, legacy_server: &str, new_server: &str, out_path: &str) {
    eprintln!("[chunk_bench] === A/B run (Design B: dual-server) ===");

    // ── Legacy row ────────────────────────────────────────────────────────
    let mut legacy = Report {
        label: "legacy".to_string(),
        repo: repo.to_string(),
        ..Default::default()
    };
    eprintln!("[chunk_bench] legacy: chunk-shape metrics (legacy_chunk_ranges) ...");
    compute_chunk_metrics(repo, true, &mut legacy);
    eprintln!("[chunk_bench] legacy: retrieval eval against {legacy_server} ...");
    run_retrieval(repo, legacy_server, &mut legacy);

    // ── New row ─────────────────────────────────────────────────────────────
    let mut new = Report {
        label: "new".to_string(),
        repo: repo.to_string(),
        ..Default::default()
    };
    eprintln!("[chunk_bench] new: chunk-shape metrics (compiled cAST chunker) ...");
    compute_chunk_metrics(repo, false, &mut new);
    eprintln!("[chunk_bench] new: retrieval eval against {new_server} ...");
    run_retrieval(repo, new_server, &mut new);

    let deltas = AbDeltas {
        recall_at_1: new.recall_at_1 - legacy.recall_at_1,
        recall_at_5: new.recall_at_5 - legacy.recall_at_5,
        recall_at_10: new.recall_at_10 - legacy.recall_at_10,
        mean_iou: new.mean_iou - legacy.mean_iou,
        duplication_ratio: new.duplication_ratio - legacy.duplication_ratio,
        cut_through_chunk_rate: new.cut_through_chunk_rate - legacy.cut_through_chunk_rate,
        total_chunks: new.total_chunks as i64 - legacy.total_chunks as i64,
    };

    let ab = AbReport {
        repo: repo.to_string(),
        mechanism: "dual-server-git-worktree (Design B): legacy=git HEAD chunker, \
                    new=working-tree cAST chunker; each on an isolated port + temp \
                    data dir, fresh real IndexPipeline build of the repo; shared \
                    home-anchored embedding cache; identical harness + query path; \
                    ground-truth from frozen parse_file symbol extraction (unchanged \
                    vs HEAD)"
            .to_string(),
        eval_pairs: new.eval_pairs,
        rerank: false,
        top_k: 10,
        legacy_server: legacy_server.to_string(),
        new_server: new_server.to_string(),
        deltas,
        reproduce_cmd: "scripts/ab_bench.sh  (boots both servers on temp ports + temp \
                        data dirs, rebuilds notepad-ade on each, then: chunk_bench \
                        <repo> <legacy_url> ab_benchmark.json --ab --new-server <new_url>)"
            .to_string(),
        legacy,
        new,
    };

    let json = serde_json::to_string_pretty(&ab).expect("serialize ab report");
    std::fs::write(out_path, &json).expect("write ab report");
    eprintln!("[chunk_bench] wrote {out_path}");
    println!("{json}");
}

/// Count non-whitespace characters — the same size metric the chunker uses, so
/// duplication/budget numbers are comparable to the chunker's own accounting.
fn nonws_len(s: &str) -> u64 {
    s.chars().filter(|c| !c.is_whitespace()).count() as u64
}

/// Faithful reproduction of the LEGACY (pre-cAST) chunker, used to compute the
/// baseline chunk-shape metrics (duplication, overlap, cut-through) with the
/// EXACT SAME measurement code as the new chunker — the only honest way to
/// compare chunk-level cut-through. Mirrors the old `parsing/chunker.rs`:
///   1. one full-body chunk per symbol (line_start..=line_end),
///   2. a 50-line / 25-stride sliding window over lines not fully covered by a
///      symbol chunk,
///   3. blank/whitespace-only chunks dropped.
///
/// Returns (line_start, line_end) ranges (1-indexed).
fn legacy_chunk_ranges(source: &str, symbols: &[Symbol]) -> Vec<(u32, u32)> {
    const WINDOW: u32 = 50;
    const STRIDE: u32 = 25;
    let lines: Vec<&str> = source.lines().collect();
    let total = lines.len() as u32;
    if total == 0 {
        return vec![];
    }
    let mut out: Vec<(u32, u32)> = Vec::new();
    let mut covered = vec![false; total as usize];
    for s in symbols {
        let start = s.line_start.saturating_sub(1);
        let end = s.line_end.min(total).saturating_sub(1);
        if start > end || start >= total {
            continue;
        }
        let content = lines[start as usize..=end as usize].join("\n");
        if !content.trim().is_empty() {
            out.push((start + 1, end + 1));
        }
        for c in &mut covered[start as usize..=end as usize] {
            *c = true;
        }
    }
    let mut ws: u32 = 0;
    while ws < total {
        let we = (ws + WINDOW - 1).min(total - 1);
        let has_uncovered = (ws..=we).any(|i| !covered[i as usize]);
        if has_uncovered {
            let content = lines[ws as usize..=we as usize].join("\n");
            if !content.trim().is_empty() {
                out.push((ws + 1, we + 1));
            }
        }
        ws += STRIDE;
    }
    out
}

/// (1)+(2): walk the repo, re-parse every file with the CURRENTLY COMPILED
/// chunker, and accumulate duplication ratio + symbols-per-chunk histogram +
/// overlap sanity. No DB, no server — pure function of the chunker in this build.
fn compute_chunk_metrics(repo: &str, legacy: bool, report: &mut Report) {
    let files = walk_repo(repo);
    let mut hist: BTreeMap<String, u64> = BTreeMap::new();
    for b in ["0", "1", "2", "3+"] {
        hist.insert(b.to_string(), 0);
    }

    for path in &files {
        let source = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if source.contains('\0') {
            continue; // binary — pipeline skips these too
        }
        let parsed = parse_file(path, &source);
        let src_lines: Vec<&str> = source.lines().collect();

        // Build the chunk list under test: legacy reproduction or the compiled
        // chunker. Both yield (content, line_start, line_end) so the metric loop
        // below is byte-identical for baseline and post-change.
        let chunks: Vec<(String, u32, u32)> = if legacy {
            legacy_chunk_ranges(&source, &parsed.symbols)
                .into_iter()
                .map(|(ls, le)| {
                    let content = src_lines
                        [(ls as usize - 1)..=(le as usize - 1).min(src_lines.len() - 1)]
                        .join("\n");
                    (content, ls, le)
                })
                .collect()
        } else {
            parsed
                .chunks
                .iter()
                .map(|c| (c.content.clone(), c.line_start, c.line_end))
                .collect()
        };

        // Only count files that actually produced chunks (parsed source langs).
        if chunks.is_empty() {
            continue;
        }
        report.files_parsed += 1;
        report.source_chars += nonws_len(&source);
        report.total_chunks += chunks.len() as u64;

        // Sort chunks by line for the overlap check.
        let mut ranges: Vec<(u32, u32)> = chunks.iter().map(|(_, ls, le)| (*ls, *le)).collect();
        ranges.sort_unstable();
        for w in ranges.windows(2) {
            // Overlap: next chunk starts at or before the previous chunk's end.
            if w[1].0 <= w[0].1 {
                report.overlapping_line_chunks += 1;
            }
        }

        for (content, ls, le) in &chunks {
            report.chunk_chars += nonws_len(content);
            let nws = nonws_len(content);
            if nws > report.max_chunk_nonws {
                report.max_chunk_nonws = nws;
            }
            if is_cut_through(*ls, *le, &parsed.symbols) {
                report.cut_through_chunks += 1;
            }
            let n = distinct_symbols_covered(*ls, *le, &parsed.symbols);
            let bucket = match n {
                0 => "0",
                1 => "1",
                2 => "2",
                _ => "3+",
            };
            *hist.get_mut(bucket).unwrap() += 1;
        }
    }

    report.duplication_ratio = if report.source_chars > 0 {
        report.chunk_chars as f64 / report.source_chars as f64
    } else {
        0.0
    };
    report.cut_through_chunk_rate = if report.total_chunks > 0 {
        report.cut_through_chunks as f64 / report.total_chunks as f64
    } else {
        0.0
    };
    report.symbols_per_chunk = hist;
}

/// Count DISTINCT leaf symbols whose body line-range intersects [start,end].
/// "Leaf" = a symbol that does not strictly contain another counted symbol on
/// the same lines; we approximate by counting symbols whose range overlaps the
/// chunk, which is the quantity the histogram needs (how many symbol bodies a
/// chunk touches). Container symbols inflate this intentionally — that skew is
/// exactly what justifies deepest-enclosing over largest-overlap.
fn distinct_symbols_covered(start: u32, end: u32, symbols: &[Symbol]) -> usize {
    symbols
        .iter()
        .filter(|s| {
            // Overlap test on inclusive line ranges.
            s.line_start <= end && s.line_end >= start
        })
        .count()
}

// ─── (3)+(4): retrieval eval against the live server ─────────────────────

#[derive(serde::Deserialize)]
struct QueryResultRow {
    line_start: u32,
    line_end: u32,
    file: String,
}

#[derive(serde::Deserialize)]
struct QueryResp {
    #[serde(default)]
    results: Vec<QueryResultRow>,
}

/// Resolve an eval pair's expected line range by finding the named symbol in the
/// target file via the FROZEN extraction. Returns the innermost (smallest) match
/// so "parse" picks the function, not an enclosing class. Chunker-independent.
fn resolve_expected_range(repo: &str, rel_file: &str, symbol: &str) -> Option<(String, u32, u32)> {
    // Build the absolute path the index uses (same normalization the server stores).
    let abs = format!("{}/{}", repo.trim_end_matches(['/', '\\']), rel_file);
    let abs = abs.replace('\\', "/");
    let source = std::fs::read_to_string(&abs).ok()?;
    let parsed = parse_file(&abs, &source);
    parsed
        .symbols
        .iter()
        .filter(|s| s.qualified.name == symbol)
        .min_by_key(|s| s.line_end.saturating_sub(s.line_start))
        .map(|s| (abs.clone(), s.line_start, s.line_end))
}

/// A chunk is a window-over-symbol CUT-THROUGH when one of its boundaries falls
/// INSIDE a CALLABLE body (Function/Method) — i.e. some callable `S` overlaps
/// the chunk but neither fully contains the chunk nor is fully contained by it.
/// That is precisely the sliding-window defect (`pipeline.rs#L2026-2075`): a
/// fixed window slicing the tail of one function and the head of the next.
///
/// Restricted to Function/Method on purpose: container symbols (class, struct,
/// namespace, impl, …) legitimately span many chunks, so a chunk that sits
/// inside a class but not inside any one method is correct cAST output, NOT a
/// defect. Counting container partial-overlap would drown the real signal.
///
/// What this deliberately does NOT flag:
///  - a split fragment fully inside one big function (chunk ⊆ S) — clean.
///  - whole callables, or several whole sibling callables MERGED into one chunk
///    (each S ⊆ chunk) — boundaries sit on AST node edges, exactly what cAST's
///    mandatory merge produces.
fn is_cut_through(start: u32, end: u32, symbols: &[Symbol]) -> bool {
    use context_engine_rs::parsing::symbols::SymbolKind;
    if start > end {
        return false;
    }
    symbols
        .iter()
        .filter(|s| matches!(s.kind, SymbolKind::Function | SymbolKind::Method))
        .any(|s| {
            let overlaps = s.line_start <= end && s.line_end >= start;
            if !overlaps {
                return false;
            }
            let chunk_in_sym = s.line_start <= start && end <= s.line_end;
            let sym_in_chunk = start <= s.line_start && s.line_end <= end;
            if chunk_in_sym || sym_in_chunk {
                return false;
            }
            // Partial overlap. Exclude a trivial single-line boundary touch
            // (a function ending on the chunk's first line, or starting on its
            // last line): byte-precise AST splits legitimately abut a sibling on
            // a shared line, which is NOT the multi-line sliding-window defect.
            // Require >= 2 overlapping lines of the callable's body to count it
            // as a genuine cut-through.
            let ov_start = start.max(s.line_start);
            let ov_end = end.min(s.line_end);
            let overlap_lines = ov_end.saturating_sub(ov_start) + 1;
            overlap_lines >= 2
        })
}

/// IoU of two inclusive line ranges.
fn iou(a: (u32, u32), b: (u32, u32)) -> f64 {
    let inter_start = a.0.max(b.0);
    let inter_end = a.1.min(b.1);
    if inter_start > inter_end {
        return 0.0;
    }
    let inter = (inter_end - inter_start + 1) as f64;
    let union = ((a.1 - a.0 + 1) + (b.1 - b.0 + 1)) as f64 - inter;
    if union <= 0.0 { 0.0 } else { inter / union }
}

fn run_retrieval(repo: &str, server: &str, report: &mut Report) {
    let pairs = eval_set();
    report.eval_pairs = pairs.len() as u64;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let client = reqwest::Client::new();

    let mut recall1 = 0u64;
    let mut recall5 = 0u64;
    let mut recall10 = 0u64;
    let mut iou_sum = 0.0f64;
    let mut iou_count = 0u64;

    rt.block_on(async {
        for (query, rel_file, symbol) in &pairs {
            let expected = match resolve_expected_range(repo, rel_file, symbol) {
                Some(e) => e,
                None => {
                    eprintln!(
                        "[chunk_bench] WARN: eval symbol not found, skipping: {symbol} in {rel_file}"
                    );
                    continue;
                }
            };
            let (exp_file, exp_start, exp_end) = expected;
            let exp_file_norm = exp_file.replace('\\', "/").to_lowercase();

            // rerank=false: we measure the raw retrieval ranking, not the LLM
            // rerank (which is non-deterministic and would mask chunk quality).
            let body = serde_json::json!({
                "query": query,
                "repo": repo,
                "top_k": 10,
                "rerank": false,
            });
            let resp = match client
                .post(format!("{server}/api/query"))
                .json(&body)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("[chunk_bench] query error for {query:?}: {e}");
                    continue;
                }
            };
            let parsed: QueryResp = match resp.json().await {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("[chunk_bench] decode error for {query:?}: {e}");
                    continue;
                }
            };
            report.queries_run += 1;
            report.topk_results_total += parsed.results.len() as u64;

            // Cut-through: for each result, re-parse its file once and test the
            // straddle condition against frozen symbols.
            let mut best_iou = 0.0f64;
            let mut hit_rank: Option<usize> = None;
            for (rank, r) in parsed.results.iter().enumerate() {
                let r_file_norm = r.file.replace('\\', "/").to_lowercase();
                // cut-through is measured over ALL top-k results, any file.
                if let Ok(src) = std::fs::read_to_string(&r.file) {
                    let p = parse_file(&r.file, &src);
                    if is_cut_through(r.line_start, r.line_end, &p.symbols) {
                        report.cut_through_results += 1;
                    }
                }
                // recall/IoU only count results in the expected file.
                if r_file_norm == exp_file_norm {
                    let i = iou((exp_start, exp_end), (r.line_start, r.line_end));
                    if i > best_iou {
                        best_iou = i;
                    }
                    let overlaps = r.line_start <= exp_end && r.line_end >= exp_start;
                    if overlaps && hit_rank.is_none() {
                        hit_rank = Some(rank);
                    }
                }
            }
            if let Some(rank) = hit_rank {
                if rank < 1 {
                    recall1 += 1;
                }
                if rank < 5 {
                    recall5 += 1;
                }
                if rank < 10 {
                    recall10 += 1;
                }
            }
            iou_sum += best_iou;
            iou_count += 1;
        }
    });

    let denom = iou_count.max(1) as f64;
    report.recall_at_1 = recall1 as f64 / denom;
    report.recall_at_5 = recall5 as f64 / denom;
    report.recall_at_10 = recall10 as f64 / denom;
    report.mean_iou = iou_sum / denom;
    report.cut_through_rate = if report.topk_results_total > 0 {
        report.cut_through_results as f64 / report.topk_results_total as f64
    } else {
        0.0
    };
}
