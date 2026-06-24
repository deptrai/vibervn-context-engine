//! Auto-setup of MCP config + prompt files for external coding agents.
//!
//! The web UI's MCP panel has one "Auto Setup" button per tool tab (Claude
//! Code / Codex / Opencode). Clicking it writes that tool's config + prompt
//! files **directly into the repo on disk** instead of making the user
//! copy-paste snippets — the engine runs locally and a "repo" in settings IS
//! its real filesystem path (see `store::sanitize_repo_name`).
//!
//! Hard requirements baked into this module:
//!   * Idempotent: re-running only updates the `codebase-retrieval` server URL
//!     (so it tracks the live port) and appends prompt guidance that is
//!     missing — it never duplicates or clobbers unrelated keys/servers.
//!   * Crash-safe: every write goes through a tempfile + atomic rename
//!     (the same pattern as `config.rs`), never a partial in-place write.
//!   * Never destroy user data: a config file that already exists but is
//!     malformed (bad JSON / bad TOML) is reported as an error and left
//!     **untouched** — we never overwrite a file we couldn't parse.
//!   * Path-escape safe: the caller validates the repo against settings; every
//!     target path is a fixed relative constant joined under the repo root, so
//!     nothing can be coaxed to write outside the repo.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use tempfile::NamedTempFile;

// ─── Guidance text (kept byte-identical to the copy-snippets in index.html) ──

/// Appended when the prompt file does not already mention `codebase-retrieval`.
const GUIDANCE_CODEBASE: &str = "When asked about the codebase, project structure, or to find code, always use the context-engine MCP tool (codebase-retrieval) in the root workspace first before reading individual files. Use `codebase-retrieval` instead of the Explore subagent for codebase exploration and search tasks.";

/// Appended when the prompt file does not already mention `file-retrieval`.
const GUIDANCE_FILE: &str = "When you need to read a specific file but don't know the exact line range, use the file-retrieval MCP tool instead of reading the entire file. Describe what information you need and it returns only the relevant snippets with line numbers. Use the Read tool with the returned line ranges (expanded as needed) to get current content before making edits.";

/// MCP server name written into every tool's config.
const SERVER_NAME: &str = "codebase-retrieval";

// ─── Target tool ─────────────────────────────────────────────────────────

/// Which external coding agent we are configuring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Claude,
    Codex,
    Opencode,
}

impl Target {
    pub fn parse(s: &str) -> Option<Target> {
        match s {
            "claude" => Some(Target::Claude),
            "codex" => Some(Target::Codex),
            "opencode" => Some(Target::Opencode),
            _ => None,
        }
    }
}

// ─── Per-file action result (surfaced to the UI) ───────────────────────────

/// Outcome status for a single file the setup touched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Created,
    Updated,
    Unchanged,
    Error,
}

impl FileStatus {
    fn as_str(self) -> &'static str {
        match self {
            FileStatus::Created => "created",
            FileStatus::Updated => "updated",
            FileStatus::Unchanged => "unchanged",
            FileStatus::Error => "error",
        }
    }
}

/// What happened to one file. Serialized into the JSON response as
/// `{ "file": "...", "status": "...", "detail": "..." }`.
#[derive(Debug, Clone)]
pub struct FileAction {
    /// Repo-relative path, forward-slashed for display.
    pub file: String,
    pub status: FileStatus,
    /// Present only for `Error` — a short human-readable reason.
    pub detail: Option<String>,
}

impl FileAction {
    pub fn to_json(&self) -> Value {
        let mut obj = Map::new();
        obj.insert("file".into(), Value::String(self.file.clone()));
        obj.insert("status".into(), Value::String(self.status.as_str().into()));
        if let Some(d) = &self.detail {
            obj.insert("detail".into(), Value::String(d.clone()));
        }
        Value::Object(obj)
    }
}

// ─── Entry point ───────────────────────────────────────────────────────────

/// Run auto-setup for one tool against one repo.
///
/// `repo_root` MUST already be a validated, on-disk repo path (the caller
/// confirms it is present in `settings.repos`). `endpoint_url` is the
/// browser-built MCP URL (`<origin>/mcp-repo/<sanitized>`) — using the origin
/// is how the live port reaches us, since the server's port lives only in
/// `main.rs` and is not in `AppState`.
///
/// Returns one `FileAction` per file touched. Per-file errors (e.g. a
/// malformed existing config) are captured as `FileStatus::Error` actions and
/// do NOT abort the other files — best-effort, never destructive.
pub fn run_setup(repo_root: &Path, target: Target, endpoint_url: &str) -> Vec<FileAction> {
    match target {
        Target::Claude => vec![
            write_claude_mcp_json(repo_root, endpoint_url),
            write_claude_settings_local(repo_root),
            ensure_gitignored(repo_root, ".claude/settings.local.json"),
            write_prompt_file(repo_root, "CLAUDE.md"),
        ],
        Target::Codex => vec![
            write_codex_config_toml(repo_root, endpoint_url),
            write_prompt_file(repo_root, "AGENTS.md"),
        ],
        Target::Opencode => vec![
            write_opencode_json(repo_root, endpoint_url),
            write_prompt_file(repo_root, "AGENTS.md"),
        ],
    }
}

// ─── Path-safe file IO ─────────────────────────────────────────────────────

/// Join a fixed relative path under the repo root and confirm it cannot escape.
///
/// `rel` is always a hardcoded constant in this module (never user input), so
/// this is belt-and-suspenders: it rejects absolute components and any `..`
/// segment so a future edit can't accidentally introduce an escape. The repo
/// root itself is trusted (validated against settings by the caller).
fn safe_join(repo_root: &Path, rel: &str) -> Result<PathBuf, String> {
    let rel_path = Path::new(rel);
    for comp in rel_path.components() {
        use std::path::Component;
        match comp {
            Component::Normal(_) => {}
            _ => return Err(format!("unsafe relative path: {rel}")),
        }
    }
    Ok(repo_root.join(rel_path))
}

/// Atomic write: tempfile in the target's parent dir → rename. Mirrors the
/// crash-safe pattern in `config.rs` so a crash mid-write never leaves a
/// truncated config behind. Creates parent dirs (e.g. `.claude/`, `.codex/`).
fn atomic_write(path: &Path, contents: &str) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "target has no parent dir".to_string())?;
    fs::create_dir_all(parent).map_err(|e| format!("create dir {}: {e}", parent.display()))?;
    let temp =
        NamedTempFile::new_in(parent).map_err(|e| format!("create tempfile in parent: {e}"))?;
    fs::write(temp.path(), contents.as_bytes()).map_err(|e| format!("write tempfile: {e}"))?;
    temp.persist(path)
        .map_err(|e| format!("persist {}: {e}", path.display()))?;
    Ok(())
}

/// Forward-slashed repo-relative label for the UI.
fn rel_label(rel: &str) -> String {
    rel.replace('\\', "/")
}

// ─── JSON helpers ──────────────────────────────────────────────────────────

/// Read an existing JSON file into an object, or return None if absent.
///
/// On a present-but-malformed file we return `Err` so the caller can report it
/// and leave the file untouched — never overwrite data we couldn't parse.
/// A file that parses to a non-object JSON value is also treated as malformed
/// (we will not coerce e.g. a top-level array into an object).
fn read_json_object(path: &Path) -> Result<Option<Map<String, Value>>, String> {
    match fs::read_to_string(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("read: {e}")),
        Ok(text) => {
            if text.trim().is_empty() {
                // Empty file → treat as a fresh object (common when a tool
                // pre-creates an empty config). Not malformed.
                return Ok(Some(Map::new()));
            }
            match serde_json::from_str::<Value>(&text) {
                Ok(Value::Object(obj)) => Ok(Some(obj)),
                Ok(_) => Err("file is valid JSON but not an object".to_string()),
                Err(e) => Err(format!("malformed JSON: {e}")),
            }
        }
    }
}

/// Pretty-print a JSON object with a trailing newline (matches editor norms).
fn json_to_string(obj: &Map<String, Value>) -> Result<String, String> {
    let mut s = serde_json::to_string_pretty(&Value::Object(obj.clone()))
        .map_err(|e| format!("serialize JSON: {e}"))?;
    s.push('\n');
    Ok(s)
}

// ─── Claude: .mcp.json ─────────────────────────────────────────────────────

/// Merge `mcpServers.codebase-retrieval = { type:"http", url }` into `.mcp.json`,
/// preserving every other server and top-level key.
fn write_claude_mcp_json(repo_root: &Path, endpoint_url: &str) -> FileAction {
    const REL: &str = ".mcp.json";
    let path = match safe_join(repo_root, REL) {
        Ok(p) => p,
        Err(e) => return error_action(REL, e),
    };
    let existed = path.exists();
    let mut root = match read_json_object(&path) {
        Ok(Some(obj)) => obj,
        Ok(None) => Map::new(),
        Err(e) => return error_action(REL, e),
    };

    let servers = root
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(servers) = servers.as_object_mut() else {
        return error_action(REL, "`mcpServers` exists but is not an object".to_string());
    };

    let mut desired = Map::new();
    desired.insert("type".into(), Value::String("http".into()));
    desired.insert("url".into(), Value::String(endpoint_url.to_string()));
    let changed = servers.get(SERVER_NAME) != Some(&Value::Object(desired.clone()));
    servers.insert(SERVER_NAME.into(), Value::Object(desired));

    commit_json(&path, REL, &root, existed, changed)
}

// ─── Claude: .claude/settings.local.json ───────────────────────────────────

/// Ensure `enabledMcpjsonServers` contains `codebase-retrieval` and
/// `enableAllProjectMcpServers` is `true`, merging into any existing file
/// without clobbering unrelated keys.
fn write_claude_settings_local(repo_root: &Path) -> FileAction {
    const REL: &str = ".claude/settings.local.json";
    let path = match safe_join(repo_root, REL) {
        Ok(p) => p,
        Err(e) => return error_action(REL, e),
    };
    let existed = path.exists();
    let mut root = match read_json_object(&path) {
        Ok(Some(obj)) => obj,
        Ok(None) => Map::new(),
        Err(e) => return error_action(REL, e),
    };
    let mut changed = false;

    // enabledMcpjsonServers: array; add SERVER_NAME if missing.
    let list = root
        .entry("enabledMcpjsonServers")
        .or_insert_with(|| Value::Array(Vec::new()));
    let Some(arr) = list.as_array_mut() else {
        return error_action(
            REL,
            "`enabledMcpjsonServers` exists but is not an array".to_string(),
        );
    };
    let present = arr.iter().any(|v| v.as_str() == Some(SERVER_NAME));
    if !present {
        arr.push(Value::String(SERVER_NAME.into()));
        changed = true;
    }

    // enableAllProjectMcpServers: force true.
    if root.get("enableAllProjectMcpServers") != Some(&Value::Bool(true)) {
        root.insert("enableAllProjectMcpServers".into(), Value::Bool(true));
        changed = true;
    }

    commit_json(&path, REL, &root, existed, changed)
}

// ─── .gitignore ────────────────────────────────────────────────────────────

/// Ensure `entry` is listed in the repo's `.gitignore`, appending it if absent
/// and creating the file if it doesn't exist.
///
/// `settings.local.json` holds machine-local MCP-enablement state, so it should
/// not be committed. Matching is line-exact against the trimmed entry (so a
/// pre-existing `.claude/settings.local.json` line — with or without a leading
/// `/` — is respected and not duplicated); we never rewrite existing lines.
fn ensure_gitignored(repo_root: &Path, entry: &str) -> FileAction {
    const REL: &str = ".gitignore";
    let path = match safe_join(repo_root, REL) {
        Ok(p) => p,
        Err(e) => return error_action(REL, e),
    };
    let existed = path.exists();
    let current = match fs::read_to_string(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return error_action(REL, format!("read: {e}")),
        Ok(t) => t,
    };

    // Already ignored? Accept the bare path or a leading-slash anchored form.
    let anchored = format!("/{entry}");
    let already = current.lines().any(|l| {
        let l = l.trim();
        l == entry || l == anchored
    });
    if already {
        return FileAction {
            file: rel_label(REL),
            status: if existed {
                FileStatus::Unchanged
            } else {
                FileStatus::Created
            },
            detail: None,
        };
    }

    let mut out = current.clone();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(entry);
    out.push('\n');

    match atomic_write(&path, &out) {
        Ok(()) => FileAction {
            file: rel_label(REL),
            status: if existed {
                FileStatus::Updated
            } else {
                FileStatus::Created
            },
            detail: None,
        },
        Err(e) => error_action(REL, e),
    }
}

// ─── Opencode: opencode.json ───────────────────────────────────────────────

/// Merge `mcp.codebase-retrieval = { type:"remote", url, enabled:true }` into
/// `opencode.json`, adding the `$schema` only when the file is new/absent.
fn write_opencode_json(repo_root: &Path, endpoint_url: &str) -> FileAction {
    const REL: &str = "opencode.json";
    let path = match safe_join(repo_root, REL) {
        Ok(p) => p,
        Err(e) => return error_action(REL, e),
    };
    let existed = path.exists();
    let mut root = match read_json_object(&path) {
        Ok(Some(obj)) => obj,
        Ok(None) => Map::new(),
        Err(e) => return error_action(REL, e),
    };
    let mut changed = false;

    // Add `$schema` only when absent — never override a user's existing one.
    if !root.contains_key("$schema") {
        root.insert(
            "$schema".into(),
            Value::String("https://opencode.ai/config.json".into()),
        );
        changed = true;
    }

    let mcp = root
        .entry("mcp")
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(mcp) = mcp.as_object_mut() else {
        return error_action(REL, "`mcp` exists but is not an object".to_string());
    };

    let mut desired = Map::new();
    desired.insert("type".into(), Value::String("remote".into()));
    desired.insert("url".into(), Value::String(endpoint_url.to_string()));
    desired.insert("enabled".into(), Value::Bool(true));
    if mcp.get(SERVER_NAME) != Some(&Value::Object(desired.clone())) {
        changed = true;
    }
    mcp.insert(SERVER_NAME.into(), Value::Object(desired));

    commit_json(&path, REL, &root, existed, changed)
}

/// Shared JSON commit: write if changed (or newly created), map result to a
/// `FileAction`.
fn commit_json(
    path: &Path,
    rel: &str,
    root: &Map<String, Value>,
    existed: bool,
    changed: bool,
) -> FileAction {
    if existed && !changed {
        return FileAction {
            file: rel_label(rel),
            status: FileStatus::Unchanged,
            detail: None,
        };
    }
    let text = match json_to_string(root) {
        Ok(t) => t,
        Err(e) => return error_action(rel, e),
    };
    match atomic_write(path, &text) {
        Ok(()) => FileAction {
            file: rel_label(rel),
            status: if existed {
                FileStatus::Updated
            } else {
                FileStatus::Created
            },
            detail: None,
        },
        Err(e) => error_action(rel, e),
    }
}

fn error_action(rel: &str, detail: String) -> FileAction {
    FileAction {
        file: rel_label(rel),
        status: FileStatus::Error,
        detail: Some(detail),
    }
}

// ─── Codex: .codex/config.toml ─────────────────────────────────────────────

/// Set `mcp_servers.codebase-retrieval.url` in `.codex/config.toml` via
/// `toml_edit`, preserving the rest of the file (comments, formatting, other
/// servers, model/sandbox/profile settings). HTTP transport is signalled by
/// the presence of `url` (no extra flag needed).
fn write_codex_config_toml(repo_root: &Path, endpoint_url: &str) -> FileAction {
    use toml_edit::{DocumentMut, value};
    const REL: &str = ".codex/config.toml";
    let path = match safe_join(repo_root, REL) {
        Ok(p) => p,
        Err(e) => return error_action(REL, e),
    };
    let existed = path.exists();

    let mut doc = match fs::read_to_string(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => DocumentMut::new(),
        Err(e) => return error_action(REL, format!("read: {e}")),
        // Present-but-malformed TOML → report and leave the file untouched.
        Ok(text) => match text.parse::<DocumentMut>() {
            Ok(d) => d,
            Err(e) => return error_action(REL, format!("malformed TOML: {e}")),
        },
    };

    // Read the current value with `.get()` chains — read-indexing a missing
    // key with `[]` panics; only write-indexing auto-creates tables.
    let prev = doc
        .get("mcp_servers")
        .and_then(|s| s.get(SERVER_NAME))
        .and_then(|s| s.get("url"))
        .and_then(|u| u.as_str())
        .map(str::to_string);
    let changed = prev.as_deref() != Some(endpoint_url);
    // Write-indexing auto-creates the intermediate tables if missing.
    doc["mcp_servers"][SERVER_NAME]["url"] = value(endpoint_url);

    if existed && !changed {
        return FileAction {
            file: rel_label(REL),
            status: FileStatus::Unchanged,
            detail: None,
        };
    }
    match atomic_write(&path, &doc.to_string()) {
        Ok(()) => FileAction {
            file: rel_label(REL),
            status: if existed {
                FileStatus::Updated
            } else {
                FileStatus::Created
            },
            detail: None,
        },
        Err(e) => error_action(REL, e),
    }
}

// ─── Prompt files: CLAUDE.md / AGENTS.md ───────────────────────────────────

/// Append guidance paragraphs the file is missing. Each block is gated
/// independently: append GUIDANCE_CODEBASE iff `codebase-retrieval` is absent,
/// GUIDANCE_FILE iff `file-retrieval` is absent. A brand-new file gets only the
/// missing blocks (no `# CLAUDE.md` header). Existing content is never
/// rewritten — blocks are appended at the end.
fn write_prompt_file(repo_root: &Path, rel: &str) -> FileAction {
    let path = match safe_join(repo_root, rel) {
        Ok(p) => p,
        Err(e) => return error_action(rel, e),
    };
    let existed = path.exists();
    let current = match fs::read_to_string(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return error_action(rel, format!("read: {e}")),
        Ok(t) => t,
    };

    let need_codebase = !current.contains("codebase-retrieval");
    let need_file = !current.contains("file-retrieval");
    if !need_codebase && !need_file {
        return FileAction {
            file: rel_label(rel),
            status: if existed {
                FileStatus::Unchanged
            } else {
                FileStatus::Created
            },
            detail: None,
        };
    }

    let mut out = current.clone();
    let mut append_block = |block: &str| {
        if out.is_empty() {
            out.push_str(block);
        } else {
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push('\n');
            out.push_str(block);
        }
    };
    if need_codebase {
        append_block(GUIDANCE_CODEBASE);
    }
    if need_file {
        append_block(GUIDANCE_FILE);
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }

    match atomic_write(&path, &out) {
        Ok(()) => FileAction {
            file: rel_label(rel),
            status: if existed {
                FileStatus::Updated
            } else {
                FileStatus::Created
            },
            detail: None,
        },
        Err(e) => error_action(rel, e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const URL: &str = "http://localhost:6699/mcp-repo/d__repo";

    fn read(dir: &TempDir, rel: &str) -> String {
        fs::read_to_string(dir.path().join(rel)).unwrap()
    }

    fn json_at(dir: &TempDir, rel: &str) -> Value {
        serde_json::from_str(&read(dir, rel)).unwrap()
    }

    fn status_of(actions: &[FileAction], rel: &str) -> FileStatus {
        actions
            .iter()
            .find(|a| a.file == rel)
            .unwrap_or_else(|| panic!("no action for {rel}"))
            .status
    }

    // ── Claude ──────────────────────────────────────────────────────────

    #[test]
    fn claude_fresh_repo_creates_all_three_files() {
        let dir = TempDir::new().unwrap();
        let actions = run_setup(dir.path(), Target::Claude, URL);
        assert_eq!(status_of(&actions, ".mcp.json"), FileStatus::Created);
        assert_eq!(
            status_of(&actions, ".claude/settings.local.json"),
            FileStatus::Created
        );
        assert_eq!(status_of(&actions, "CLAUDE.md"), FileStatus::Created);

        let mcp = json_at(&dir, ".mcp.json");
        assert_eq!(mcp["mcpServers"][SERVER_NAME]["type"], "http");
        assert_eq!(mcp["mcpServers"][SERVER_NAME]["url"], URL);

        let settings = json_at(&dir, ".claude/settings.local.json");
        assert_eq!(settings["enableAllProjectMcpServers"], true);
        assert_eq!(settings["enabledMcpjsonServers"][0], SERVER_NAME);

        let md = read(&dir, "CLAUDE.md");
        assert!(md.contains("codebase-retrieval"));
        assert!(md.contains("file-retrieval"));
        assert!(!md.contains("# CLAUDE.md")); // no header on fresh file
    }

    #[test]
    fn claude_preserves_other_servers_and_updates_url() {
        let dir = TempDir::new().unwrap();
        let pre = r#"{"mcpServers":{"other":{"type":"stdio","command":"foo"},"codebase-retrieval":{"type":"http","url":"http://localhost:1111/mcp-repo/d__repo"}}}"#;
        fs::write(dir.path().join(".mcp.json"), pre).unwrap();

        let actions = run_setup(dir.path(), Target::Claude, URL);
        assert_eq!(status_of(&actions, ".mcp.json"), FileStatus::Updated);

        let mcp = json_at(&dir, ".mcp.json");
        // Other server untouched.
        assert_eq!(mcp["mcpServers"]["other"]["command"], "foo");
        // URL updated to the new port.
        assert_eq!(mcp["mcpServers"][SERVER_NAME]["url"], URL);
    }

    #[test]
    fn claude_idempotent_second_run_is_unchanged() {
        let dir = TempDir::new().unwrap();
        run_setup(dir.path(), Target::Claude, URL);
        let actions = run_setup(dir.path(), Target::Claude, URL);
        assert_eq!(status_of(&actions, ".mcp.json"), FileStatus::Unchanged);
        assert_eq!(
            status_of(&actions, ".claude/settings.local.json"),
            FileStatus::Unchanged
        );
        assert_eq!(status_of(&actions, "CLAUDE.md"), FileStatus::Unchanged);
    }

    #[test]
    fn claude_settings_local_merges_without_clobbering() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join(".claude")).unwrap();
        let pre = r#"{"permissions":{"allow":["Read"]},"enabledMcpjsonServers":["existing"]}"#;
        fs::write(dir.path().join(".claude/settings.local.json"), pre).unwrap();

        run_setup(dir.path(), Target::Claude, URL);
        let s = json_at(&dir, ".claude/settings.local.json");
        // Unrelated key preserved.
        assert_eq!(s["permissions"]["allow"][0], "Read");
        // Existing entry kept, ours appended.
        let servers = s["enabledMcpjsonServers"].as_array().unwrap();
        assert!(servers.iter().any(|v| v == "existing"));
        assert!(servers.iter().any(|v| v == SERVER_NAME));
        assert_eq!(s["enableAllProjectMcpServers"], true);
    }

    #[test]
    fn malformed_mcp_json_is_reported_and_not_overwritten() {
        let dir = TempDir::new().unwrap();
        let bad = "{ this is not json ]";
        fs::write(dir.path().join(".mcp.json"), bad).unwrap();

        let actions = run_setup(dir.path(), Target::Claude, URL);
        assert_eq!(status_of(&actions, ".mcp.json"), FileStatus::Error);
        // File left byte-for-byte intact.
        assert_eq!(read(&dir, ".mcp.json"), bad);
    }

    #[test]
    fn claude_creates_gitignore_with_settings_local() {
        let dir = TempDir::new().unwrap();
        let actions = run_setup(dir.path(), Target::Claude, URL);
        assert_eq!(status_of(&actions, ".gitignore"), FileStatus::Created);
        let gi = read(&dir, ".gitignore");
        assert!(gi.contains(".claude/settings.local.json"));
    }

    #[test]
    fn claude_appends_gitignore_preserving_existing() {
        let dir = TempDir::new().unwrap();
        let pre = "node_modules/\ntarget/\n";
        fs::write(dir.path().join(".gitignore"), pre).unwrap();

        let actions = run_setup(dir.path(), Target::Claude, URL);
        assert_eq!(status_of(&actions, ".gitignore"), FileStatus::Updated);
        let gi = read(&dir, ".gitignore");
        assert!(gi.contains("node_modules/")); // existing kept
        assert!(gi.contains("target/"));
        assert!(gi.contains(".claude/settings.local.json"));
    }

    #[test]
    fn claude_gitignore_idempotent_and_respects_anchored_form() {
        let dir = TempDir::new().unwrap();
        // Pre-existing leading-slash anchored entry must not be duplicated.
        fs::write(
            dir.path().join(".gitignore"),
            "/.claude/settings.local.json\n",
        )
        .unwrap();
        let actions = run_setup(dir.path(), Target::Claude, URL);
        assert_eq!(status_of(&actions, ".gitignore"), FileStatus::Unchanged);
        let gi = read(&dir, ".gitignore");
        assert_eq!(gi.matches("settings.local.json").count(), 1);
    }

    #[test]
    fn non_claude_targets_do_not_touch_gitignore() {
        let dir = TempDir::new().unwrap();
        run_setup(dir.path(), Target::Codex, URL);
        assert!(!dir.path().join(".gitignore").exists());
    }

    // ── Codex ───────────────────────────────────────────────────────────

    #[test]
    fn codex_fresh_creates_toml_and_agents_md() {
        let dir = TempDir::new().unwrap();
        let actions = run_setup(dir.path(), Target::Codex, URL);
        assert_eq!(
            status_of(&actions, ".codex/config.toml"),
            FileStatus::Created
        );
        assert_eq!(status_of(&actions, "AGENTS.md"), FileStatus::Created);

        let toml = read(&dir, ".codex/config.toml");
        assert!(toml.contains("[mcp_servers.codebase-retrieval]") || toml.contains(SERVER_NAME));
        assert!(toml.contains(URL));
    }

    #[test]
    fn codex_preserves_comments_and_other_settings() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join(".codex")).unwrap();
        let pre =
            "# my codex config\nmodel = \"gpt-5\"\n\n[mcp_servers.other]\nurl = \"http://x\"\n";
        fs::write(dir.path().join(".codex/config.toml"), pre).unwrap();

        let actions = run_setup(dir.path(), Target::Codex, URL);
        assert_eq!(
            status_of(&actions, ".codex/config.toml"),
            FileStatus::Updated
        );

        let toml = read(&dir, ".codex/config.toml");
        assert!(toml.contains("# my codex config")); // comment preserved
        assert!(toml.contains("model = \"gpt-5\"")); // setting preserved
        assert!(toml.contains("[mcp_servers.other]")); // other server preserved
        assert!(toml.contains(URL));
    }

    #[test]
    fn codex_malformed_toml_reported_not_overwritten() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join(".codex")).unwrap();
        let bad = "this = = not valid toml [[[";
        fs::write(dir.path().join(".codex/config.toml"), bad).unwrap();

        let actions = run_setup(dir.path(), Target::Codex, URL);
        assert_eq!(status_of(&actions, ".codex/config.toml"), FileStatus::Error);
        assert_eq!(read(&dir, ".codex/config.toml"), bad);
    }

    // ── Opencode ────────────────────────────────────────────────────────

    #[test]
    fn opencode_fresh_sets_schema_and_remote_server() {
        let dir = TempDir::new().unwrap();
        let actions = run_setup(dir.path(), Target::Opencode, URL);
        assert_eq!(status_of(&actions, "opencode.json"), FileStatus::Created);

        let j = json_at(&dir, "opencode.json");
        assert_eq!(j["$schema"], "https://opencode.ai/config.json");
        assert_eq!(j["mcp"][SERVER_NAME]["type"], "remote");
        assert_eq!(j["mcp"][SERVER_NAME]["url"], URL);
        assert_eq!(j["mcp"][SERVER_NAME]["enabled"], true);
    }

    #[test]
    fn opencode_keeps_user_schema_and_other_keys() {
        let dir = TempDir::new().unwrap();
        let pre = r#"{"$schema":"https://custom","theme":"dark","mcp":{"other":{"type":"local"}}}"#;
        fs::write(dir.path().join("opencode.json"), pre).unwrap();

        run_setup(dir.path(), Target::Opencode, URL);
        let j = json_at(&dir, "opencode.json");
        assert_eq!(j["$schema"], "https://custom"); // not overridden
        assert_eq!(j["theme"], "dark"); // unrelated key kept
        assert_eq!(j["mcp"]["other"]["type"], "local"); // other server kept
        assert_eq!(j["mcp"][SERVER_NAME]["url"], URL);
    }

    // ── Prompt gating ───────────────────────────────────────────────────

    #[test]
    fn prompt_appends_only_missing_block() {
        let dir = TempDir::new().unwrap();
        // File already mentions codebase-retrieval but not file-retrieval.
        let pre = "# Notes\n\nWe use codebase-retrieval already.\n";
        fs::write(dir.path().join("AGENTS.md"), pre).unwrap();

        let actions = run_setup(dir.path(), Target::Codex, URL);
        assert_eq!(status_of(&actions, "AGENTS.md"), FileStatus::Updated);

        let md = read(&dir, "AGENTS.md");
        assert!(md.starts_with("# Notes")); // original kept
        // GUIDANCE_CODEBASE not appended again (only one occurrence of its text).
        assert_eq!(md.matches("instead of the Explore subagent").count(), 0);
        // GUIDANCE_FILE appended.
        assert!(md.contains("file-retrieval"));
    }

    #[test]
    fn prompt_unchanged_when_both_blocks_present() {
        let dir = TempDir::new().unwrap();
        run_setup(dir.path(), Target::Codex, URL); // creates AGENTS.md
        let actions = run_setup(dir.path(), Target::Codex, URL);
        assert_eq!(status_of(&actions, "AGENTS.md"), FileStatus::Unchanged);
    }

    // ── Path safety ─────────────────────────────────────────────────────

    #[test]
    fn safe_join_rejects_parent_traversal() {
        let dir = TempDir::new().unwrap();
        assert!(safe_join(dir.path(), "../escape.json").is_err());
        assert!(safe_join(dir.path(), "a/../../escape").is_err());
        assert!(safe_join(dir.path(), ".claude/settings.local.json").is_ok());
    }
}
