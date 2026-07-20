# AGENTS.md — vibervn-context-engine

## Build & Test

- **Toolchain**: Rust stable (install via `rustup`). `cargo` must be on PATH
  (source `~/.cargo/env` if needed).
- **Run all tests**: `cargo test --workspace`
- **Run lib tests only** (fast, ~70s): `cargo test --lib`
- **Run a single test**: `cargo test --lib -- <test_name_substring>`
- **Build**: `cargo build` (debug=0 in dev profile for speed)
- **Binaries**: `context-engine-rs` (server, default-run), `bench-query`,
  `bench-incremental`, `chunk_bench` — all share the same `engine_boot` path.

## Mutation Testing (cargo-mutants)

This repo uses [cargo-mutants](https://mutants.rs/) for mutation testing.
Config lives in `.cargo/mutants.toml`.

### Setup

```bash
cargo install cargo-mutants --locked
```

### Commands

```bash
# List all generated mutants (no execution)
cargo mutants --list

# RECOMMENDED: --in-place for fast builds (shares target/, ~75s/mutant).
# CAVEAT: sequential (no -j), uses git checkout to rollback — commit/stash
# uncommitted src/ changes first!
git stash  # if you have uncommitted src/ changes
cargo mutants --in-place -f 'src/config.rs'
git stash pop  # restore your changes

# Mutate a single file (targeted run)
cargo mutants --in-place -f 'src/mcp.rs'

# Full run — WARNING: ~84h sequential (2876 mutants × ~75s)
# Use --shard to split across machines, or run overnight per file.
cargo mutants --in-place

# Parallel sharding (run on multiple machines / CI)
cargo mutants --in-place --shard 1/4
cargo mutants --in-place --shard 2/4
cargo mutants --in-place --shard 3/4
cargo mutants --in-place --shard 4/4

# Skip mutants already caught in a previous run (incremental)
cargo mutants --in-place --iterate

# Only mutate code touched by a diff/PR
cargo mutants --in-place --in-diff <(git diff main...HEAD)
```

### Performance notes

This repo has 600+ dependencies including RocksDB (200+ C++ files). The
default scratch-dir mode rebuilds ALL deps per mutant (~20 min/mutant).
`--in-place` shares `target/` 100%, so only the mutated crate is recompiled
(~5s build + ~70s test = ~75s/mutant). Always prefer `--in-place`.

`copy_target = true` is set in `.cargo/mutants.toml` as a fallback for
non-in-place runs, but it does NOT fully prevent RocksDB rebuilds (build
scripts rerun on fingerprint mismatch).

### Config highlights (`.cargo/mutants.toml`)

- **`additional_cargo_test_args = ["--lib"]`** — only run lib tests per mutant,
  not the bench binaries (which are measurement harnesses, not unit tests).
- **`exclude_globs = ["src/bin/**", "vendor/**"]`** — skip bench CLIs and
  vendored tree-sitter grammar.
- **`skip_calls`** — skips `debug!`/`info!`/`warn!`/`trace!` logging macros and
  progress-bar setters (mutating them is noise, never caught by tests).
- **`cap_lints = true`** — denied warnings in mutated code don't block viable
  mutants.
- **`minimum_test_timeout = 300`** — some tests (e.g.
  `live_handle_blocks_reset`) run 60s+; this prevents false timeout flags.

### Output

Results go to `mutants.out/` (gitignored). Key files:
- `mutants.out/outcomes.json` — machine-readable results
- `mutants.out/missed.txt` — list of survived (uncaught) mutants

### Interpreting results

- **caught** = mutant killed by tests (good — test covers this code path)
- **missed** = mutant survived (bad — test gap, needs more coverage)
- **unviable** = mutant didn't compile (neutral — code structure issue)
- **timeout** = mutant caused test to hang (investigate — may indicate missing
  test timeout or infinite loop in production code)

Exit codes: `0` = all caught, `2` = some missed, `3` = some timeout.
