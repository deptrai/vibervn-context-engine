//! MCP formatting, merging, and file-key tests. Readiness policy tests live in
//! the readiness tests beside the policy itself.

use super::*;

#[test]
fn vector_only_fast_path_only_for_resolve_edges() {
    let resolving = crate::indexing::RepoStatus {
        state: crate::indexing::IndexState::Indexing,
        phase: crate::indexing::IndexPhase::ResolveEdges,
        ..Default::default()
    };
    assert!(readiness::is_resolve_edges_status(Some(&resolving)));

    let embedding = crate::indexing::RepoStatus {
        state: crate::indexing::IndexState::Indexing,
        phase: crate::indexing::IndexPhase::Embedding,
        ..Default::default()
    };
    assert!(!readiness::is_resolve_edges_status(Some(&embedding)));

    let idle_resolve = crate::indexing::RepoStatus {
        state: crate::indexing::IndexState::Idle,
        phase: crate::indexing::IndexPhase::ResolveEdges,
        ..Default::default()
    };
    assert!(!readiness::is_resolve_edges_status(Some(&idle_resolve)));
    assert!(!readiness::is_resolve_edges_status(None));
}

#[test]
fn file_retrieval_db_key_windows_backslash_input() {
    let repo = r"D:\projects\Python\local-context-engine";
    let file_path = r"context-engine-rs\Cargo.toml";
    let db_key = build_db_key(repo, file_path);
    assert_eq!(
        db_key,
        r"D:\projects\Python\local-context-engine\context-engine-rs\Cargo.toml"
    );
}

#[test]
fn file_retrieval_db_key_forward_slash_input() {
    let repo = r"D:\projects\Python\local-context-engine";
    let file_path = "context-engine-rs/Cargo.toml";
    let db_key = build_db_key(repo, file_path);
    assert_eq!(
        db_key,
        r"D:\projects\Python\local-context-engine\context-engine-rs\Cargo.toml"
    );
}

#[test]
fn file_retrieval_db_key_mixed_slashes() {
    let repo = r"D:\projects\Python\local-context-engine";
    let file_path = r"src/indexing\pipeline.rs";
    let db_key = build_db_key(repo, file_path);
    assert_eq!(
        db_key,
        r"D:\projects\Python\local-context-engine\src\indexing\pipeline.rs"
    );
}

#[test]
fn file_retrieval_db_key_leading_slash_in_file_path() {
    let repo = r"D:\projects\Python\local-context-engine";
    let file_path = "/context-engine-rs/Cargo.toml";
    let db_key = build_db_key(repo, file_path);
    assert_eq!(
        db_key,
        r"D:\projects\Python\local-context-engine\context-engine-rs\Cargo.toml"
    );
}

#[test]
fn file_retrieval_db_key_leading_backslash_in_file_path() {
    let repo = r"D:\projects\Python\local-context-engine";
    let file_path = r"\context-engine-rs\Cargo.toml";
    let db_key = build_db_key(repo, file_path);
    assert_eq!(
        db_key,
        r"D:\projects\Python\local-context-engine\context-engine-rs\Cargo.toml"
    );
}

#[test]
fn file_retrieval_db_key_trailing_slash_in_workspace() {
    let repo = r"D:\projects\Python\local-context-engine\";
    let file_path = "context-engine-rs/Cargo.toml";
    let db_key = build_db_key(repo, file_path);
    assert_eq!(
        db_key,
        r"D:\projects\Python\local-context-engine\context-engine-rs\Cargo.toml"
    );
}

#[test]
fn file_retrieval_db_key_both_edge_cases() {
    let repo = r"D:\projects\Python\local-context-engine/";
    let file_path = "/context-engine-rs/Cargo.toml";
    let db_key = build_db_key(repo, file_path);
    assert_eq!(
        db_key,
        r"D:\projects\Python\local-context-engine\context-engine-rs\Cargo.toml"
    );
}

#[test]
fn file_retrieval_db_key_unix_paths() {
    let repo = "/home/user/project";
    let file_path = "src/main.rs";
    let db_key = build_db_key(repo, file_path);
    assert!(db_key.contains("src"));
    assert!(db_key.contains("main.rs"));
    assert!(!db_key.contains("//"));
}

#[test]
fn cosine_identical_vectors() {
    let v = vec![1.0, 0.0, 0.0];
    assert!((cosine_similarity(&v, &v) - 1.0).abs() < 1e-6);
}

#[test]
fn cosine_orthogonal_vectors() {
    let a = vec![1.0, 0.0, 0.0];
    let b = vec![0.0, 1.0, 0.0];
    assert!(cosine_similarity(&a, &b).abs() < 1e-6);
}

#[test]
fn cosine_empty_returns_zero() {
    assert_eq!(cosine_similarity(&[], &[]), 0.0);
    assert_eq!(cosine_similarity(&[1.0], &[]), 0.0);
}

#[test]
fn budget_all_fit() {
    let blocks = vec![
        OutputBlock {
            header: "file.rs#L1-10".to_string(),
            content: "1: fn main() {\n2:   println!(\"hi\");\n3: }".to_string(),
            file: "file.rs".to_string(),
            line_start: 1,
            line_end: 10,
            callers: None,
            caller_files: None,
            ..Default::default()
        },
        OutputBlock {
            header: "file.rs#L20-30".to_string(),
            content: "20: fn foo() {\n21:   bar();\n22: }".to_string(),
            file: "file.rs".to_string(),
            line_start: 20,
            line_end: 30,
            callers: None,
            caller_files: None,
            ..Default::default()
        },
    ];
    let out = assemble_with_budget(&blocks);
    assert!(out.contains("1: fn main()"));
    assert!(out.contains("20: fn foo()"));
    assert!(!out.contains("truncated"));
}

#[test]
fn budget_exceeded_shows_header_and_first_line() {
    let big_content = (1..=500)
        .map(|i| format!("{}: // line {}", i, "x".repeat(80)))
        .collect::<Vec<_>>()
        .join("\n");
    let mut blocks = Vec::new();
    for i in 0..200 {
        blocks.push(OutputBlock {
            header: format!("big.rs#L{}-{}", i * 500 + 1, (i + 1) * 500),
            content: big_content.clone(),
            file: "big.rs".to_string(),
            line_start: i * 500 + 1,
            line_end: (i + 1) * 500,
            callers: None,
            caller_files: None,
            ..Default::default()
        });
    }
    let out = assemble_with_budget(&blocks);
    assert!(out.len() <= MAX_TOOL_OUTPUT_CHARS);
    assert!(out.contains("truncated to fit output size limit"));
    assert!(out.contains("elided, use Read"));
}

#[test]
fn budget_first_line_capped_at_120() {
    let long_line = format!("1: {}", "x".repeat(200));
    let blocks = vec![OutputBlock {
        header: "file.rs#L1-5".to_string(),
        content: "1: short line".to_string(),
        file: "file.rs".to_string(),
        line_start: 1,
        line_end: 5,
        callers: None,
        caller_files: None,
        ..Default::default()
    }];
    // This block fits fully, so test the truncation on a block that exceeds budget.
    let big = "y".repeat(MAX_TOOL_OUTPUT_CHARS);
    let blocks2 = vec![
        OutputBlock {
            header: "a.rs#L1-999".to_string(),
            content: big,
            file: "a.rs".to_string(),
            line_start: 1,
            line_end: 999,
            callers: None,
            caller_files: None,
            ..Default::default()
        },
        OutputBlock {
            header: "b.rs#L1-10".to_string(),
            content: long_line,
            file: "b.rs".to_string(),
            line_start: 1,
            line_end: 10,
            callers: None,
            caller_files: None,
            ..Default::default()
        },
    ];
    let out = assemble_with_budget(&blocks2);
    // The second block should be truncated. Its first line is >120 chars.
    // Verify the output contains the ellipsis marker for long line.
    assert!(out.contains("…"));
    // Verify within budget.
    assert!(out.len() <= MAX_TOOL_OUTPUT_CHARS);
    // First block's full output (all 'y's) also gets budget-applied:
    // since it alone exceeds budget, even it gets truncated form.
    assert!(out.contains("elided, use Read"));

    // Test blocks that fit fine.
    let out1 = assemble_with_budget(&blocks);
    assert!(out1.contains("1: short line"));
    assert!(!out1.contains("truncated"));
}

#[test]
fn budget_single_line_chunk_no_elision() {
    // Single-line chunk: line_end == line_start, so no elision marker needed.
    // Put it behind a budget-buster so it gets truncated form.
    let big = "z".repeat(MAX_TOOL_OUTPUT_CHARS);
    let blocks2 = vec![
        OutputBlock {
            header: "huge.rs#L1-999".to_string(),
            content: big,
            file: "huge.rs".to_string(),
            line_start: 1,
            line_end: 999,
            callers: None,
            caller_files: None,
            ..Default::default()
        },
        OutputBlock {
            header: "file.rs#L5-5".to_string(),
            content: "5: let x = 1;".to_string(),
            file: "file.rs".to_string(),
            line_start: 5,
            line_end: 5,
            callers: None,
            caller_files: None,
            ..Default::default()
        },
    ];
    let out = assemble_with_budget(&blocks2);
    // line_end == line_start → no "elided" line for this block
    assert!(out.contains("file.rs#L5-5"));
    assert!(out.contains("5: let x = 1;"));
    // But the "elided" marker should appear for the first (big) block
    assert!(out.contains("elided, use Read"));
}

#[test]
fn merge_blocks_no_overlap() {
    let blocks = vec![
        OutputBlock {
            header: "a.rs#L1-10".into(),
            content: "1: aaa".into(),
            file: "a.rs".into(),
            line_start: 1,
            line_end: 10,
            callers: None,
            caller_files: None,
            ..Default::default()
        },
        OutputBlock {
            header: "a.rs#L20-30".into(),
            content: "20: bbb".into(),
            file: "a.rs".into(),
            line_start: 20,
            line_end: 30,
            callers: None,
            caller_files: None,
            ..Default::default()
        },
    ];
    let merged = merge_overlapping_blocks(blocks);
    assert_eq!(merged.len(), 2);
    assert_eq!(merged[0].line_start, 1);
    assert_eq!(merged[1].line_start, 20);
}

#[test]
fn merge_blocks_overlap_same_file() {
    let blocks = vec![
        OutputBlock {
            header: "a.rs#L1-50".into(),
            content: "1: aaa\n2: bbb".into(),
            file: "a.rs".into(),
            line_start: 1,
            line_end: 50,
            callers: None,
            caller_files: None,
            ..Default::default()
        },
        OutputBlock {
            header: "a.rs#L26-75".into(),
            content: "26: ccc\n27: ddd".into(),
            file: "a.rs".into(),
            line_start: 26,
            line_end: 75,
            callers: None,
            caller_files: None,
            ..Default::default()
        },
    ];
    let merged = merge_overlapping_blocks(blocks);
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].line_start, 1);
    assert_eq!(merged[0].line_end, 75);
    // Header uses the same file path as inputs + merged range, no caller tag.
    assert_eq!(merged[0].header, "a.rs#L1-75");
}

#[test]
fn merge_blocks_combines_caller_tags() {
    let blocks = vec![
        OutputBlock {
            header: "a.rs#L1-50 [callers:3 files:2]".into(),
            content: "1: aaa".into(),
            file: "a.rs".into(),
            line_start: 1,
            line_end: 50,
            callers: Some(3),
            caller_files: Some(2),
            ..Default::default()
        },
        OutputBlock {
            header: "a.rs#L26-75 [callers:7 files:4]".into(),
            content: "26: bbb".into(),
            file: "a.rs".into(),
            line_start: 26,
            line_end: 75,
            callers: Some(7),
            caller_files: Some(4),
            ..Default::default()
        },
    ];
    let merged = merge_overlapping_blocks(blocks);
    assert_eq!(merged.len(), 1);
    // Caller stats: max(3,7)=7, max(2,4)=4
    assert_eq!(merged[0].callers, Some(7));
    assert_eq!(merged[0].caller_files, Some(4));
    // Header includes the combined caller tag (count-only format for merged blocks).
    assert_eq!(merged[0].header, "a.rs#L1-75 [callers:7]");
}

#[test]
fn merge_blocks_carries_caller_and_callee_names() {
    // Regression: the names MUST travel with the count through a merge.
    // Block A is name-less (the import region), Block B carries the real
    // symbol's counts AND names. Old merge logic bumped only the counts,
    // leaving A's empty names → bare "[callers:N]". Now the higher-count
    // block's names are adopted atomically with its count.
    let blocks = vec![
        OutputBlock {
            file: "a.rs".into(),
            content: "1: use foo;".into(),
            line_start: 1,
            line_end: 50,
            callers: None,
            caller_names: vec![],
            callees: None,
            callee_names: vec![],
            ..Default::default()
        },
        OutputBlock {
            file: "a.rs".into(),
            content: "26: fn x".into(),
            line_start: 26,
            line_end: 75,
            callers: Some(2),
            caller_files: Some(1),
            caller_names: vec!["foo".into(), "bar".into()],
            callees: Some(1),
            callee_names: vec!["baz".into()],
            ..Default::default()
        },
    ];
    let merged = merge_overlapping_blocks(blocks);
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].callers, Some(2));
    assert_eq!(
        merged[0].caller_names,
        vec!["foo".to_string(), "bar".to_string()]
    );
    assert_eq!(merged[0].callees, Some(1));
    assert_eq!(merged[0].callee_names, vec!["baz".to_string()]);
    // Header renders the NAMED form, not the bare count fallback.
    // (a.rs doesn't exist on disk, so the multi-block merge falls back to a
    // content union — only the header tags matter for this assertion.)
    assert!(
        merged[0].header.contains("[callers: foo, bar]"),
        "header missing named callers: {}",
        merged[0].header
    );
    assert!(
        merged[0].header.contains("[calls: baz]"),
        "header missing named callees: {}",
        merged[0].header
    );
    assert!(
        !merged[0].header.contains("[callers:2]"),
        "header fell back to bare count: {}",
        merged[0].header
    );
}

#[test]
fn merge_blocks_adjacent() {
    let blocks = vec![
        OutputBlock {
            header: "a.rs#L1-10".into(),
            content: "1: x".into(),
            file: "a.rs".into(),
            line_start: 1,
            line_end: 10,
            callers: None,
            caller_files: None,
            ..Default::default()
        },
        OutputBlock {
            header: "a.rs#L11-20".into(),
            content: "11: y".into(),
            file: "a.rs".into(),
            line_start: 11,
            line_end: 20,
            callers: None,
            caller_files: None,
            ..Default::default()
        },
    ];
    let merged = merge_overlapping_blocks(blocks);
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].line_start, 1);
    assert_eq!(merged[0].line_end, 20);
}

#[test]
fn merge_blocks_different_files_no_merge() {
    let blocks = vec![
        OutputBlock {
            header: "a.rs#L1-50".into(),
            content: "1: aaa".into(),
            file: "a.rs".into(),
            line_start: 1,
            line_end: 50,
            callers: None,
            caller_files: None,
            ..Default::default()
        },
        OutputBlock {
            header: "b.rs#L1-50".into(),
            content: "1: bbb".into(),
            file: "b.rs".into(),
            line_start: 1,
            line_end: 50,
            callers: None,
            caller_files: None,
            ..Default::default()
        },
    ];
    let merged = merge_overlapping_blocks(blocks);
    assert_eq!(merged.len(), 2);
}

#[test]
fn merge_blocks_preserves_priority_order() {
    let blocks = vec![
        OutputBlock {
            header: "b.rs#L1-10".into(),
            content: "1: first".into(),
            file: "b.rs".into(),
            line_start: 1,
            line_end: 10,
            callers: None,
            caller_files: None,
            ..Default::default()
        },
        OutputBlock {
            header: "a.rs#L1-50".into(),
            content: "1: second".into(),
            file: "a.rs".into(),
            line_start: 1,
            line_end: 50,
            callers: None,
            caller_files: None,
            ..Default::default()
        },
        OutputBlock {
            header: "a.rs#L26-75".into(),
            content: "26: third".into(),
            file: "a.rs".into(),
            line_start: 26,
            line_end: 75,
            callers: None,
            caller_files: None,
            ..Default::default()
        },
        OutputBlock {
            header: "b.rs#L20-30".into(),
            content: "20: fourth".into(),
            file: "b.rs".into(),
            line_start: 20,
            line_end: 30,
            callers: None,
            caller_files: None,
            ..Default::default()
        },
    ];
    let merged = merge_overlapping_blocks(blocks);
    // b.rs: L1-10 and L20-30 not overlapping → 2 blocks
    // a.rs: L1-50 and L26-75 overlap → 1 merged block
    assert_eq!(merged.len(), 3);
    // b.rs appeared first (index 0), so its blocks come first
    assert_eq!(merged[0].file, "b.rs");
    assert_eq!(merged[0].line_start, 1);
    // a.rs appeared at index 1
    assert_eq!(merged[1].file, "a.rs");
    assert_eq!(merged[1].line_start, 1);
    assert_eq!(merged[1].line_end, 75);
    // b.rs second block at index 3 → comes after a.rs
    assert_eq!(merged[2].file, "b.rs");
    assert_eq!(merged[2].line_start, 20);
}

#[test]
fn merge_blocks_fallback_preserves_content_on_fs_failure() {
    // Use a non-existent file path so read_lines_from_fs will fail,
    // exercising the fallback content-merge path.
    let blocks = vec![
        OutputBlock {
            header: "/nonexistent/z.rs#L1-50".into(),
            content: "1: aaa\n2: bbb\n3: ccc".into(),
            file: "/nonexistent/z.rs".into(),
            line_start: 1,
            line_end: 50,
            callers: None,
            caller_files: None,
            ..Default::default()
        },
        OutputBlock {
            header: "/nonexistent/z.rs#L26-75".into(),
            content: "2: bbb\n26: ddd\n27: eee".into(),
            file: "/nonexistent/z.rs".into(),
            line_start: 26,
            line_end: 75,
            callers: None,
            caller_files: None,
            ..Default::default()
        },
    ];
    let merged = merge_overlapping_blocks(blocks);
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].line_start, 1);
    assert_eq!(merged[0].line_end, 75);
    assert!(merged[0].header.contains("L1-75"));
    // Fallback should preserve original lines, deduped by line number.
    assert!(merged[0].content.contains("1: aaa"));
    assert!(merged[0].content.contains("2: bbb"));
    assert!(merged[0].content.contains("3: ccc"));
    assert!(merged[0].content.contains("26: ddd"));
    assert!(merged[0].content.contains("27: eee"));
    // Line "2: bbb" appeared in both blocks but should only appear once.
    assert_eq!(merged[0].content.matches("2: bbb").count(), 1);
}

// ─── select_empty_or_warming_message (warming-vs-empty signal) ───────────

#[test]
fn warming_message_takes_precedence_and_says_retry() {
    // warming=true → retry message, regardless of rerank_rejected.
    for rr in [false, true] {
        let msg = select_empty_or_warming_message(true, rr, "find the parser");
        assert!(
            msg.contains("warming"),
            "warming msg must mention warming: {msg}"
        );
        assert!(
            msg.to_lowercase().contains("retry"),
            "must tell caller to retry: {msg}"
        );
        // MUST NOT use the genuine-empty wording.
        assert!(
            !msg.contains("No results found"),
            "warming must not say 'No results found'"
        );
        assert!(
            !msg.contains("No relevant code found"),
            "warming must not say 'No relevant code found'"
        );
    }
}

#[test]
fn genuine_empty_resident_shard_keeps_existing_wording() {
    // warming=false, not rejected → the unchanged "No results found for: <q>".
    let msg = select_empty_or_warming_message(false, false, "find the parser");
    assert_eq!(msg, "No results found for: find the parser");

    // warming=false, rerank actively rejected → the unchanged rerank-rejected wording.
    let msg = select_empty_or_warming_message(false, true, "find the parser");
    assert!(
        msg.starts_with("No relevant code found."),
        "rerank-rejected wording preserved: {msg}"
    );
    assert!(
        !msg.contains("warming"),
        "genuine empty must not mention warming"
    );
}
