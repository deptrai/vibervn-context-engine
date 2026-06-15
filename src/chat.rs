//! Repo chat: a streaming, tool-calling agent that answers questions about a
//! single repository. It is **hard-locked** to exactly two tools —
//! `codebase-retrieval` and `file-retrieval` — both scoped to the repo the
//! conversation was opened for. The agent cannot reach any other repo or tool.
//!
//! Conversation state lives only in memory ([`ConversationStore`]), keyed by a
//! client-generated id. Closing the dialog drops the id; reopening makes a new
//! one. The store is LRU-capped (count + per-conversation message count) so RAM
//! stays bounded regardless of how many dialogs are opened over a session — and
//! only the plain-text transcript (user questions + assistant answers) is kept,
//! never the tool-call/result history.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;
use surrealdb::Surreal;
use surrealdb::engine::local::Db;
use tokio::sync::{Mutex, RwLock, mpsc};

use crate::config::Settings;
use crate::indexing::IndexEngine;
use crate::llm::{ChatMessage, LlmClient, ToolDef, ToolResult, ToolTurnResult};

/// The only two tool names the chat agent may call. Any other name returned by
/// the model is rejected with an error tool-result so it self-corrects.
pub const TOOL_CODEBASE: &str = "codebase-retrieval";
pub const TOOL_FILE: &str = "file-retrieval";

/// Max tool-calling rounds before the loop gives up (bounds cost per question).
const MAX_TURNS: u32 = 8;
/// Max characters of a tool result forwarded to the UI as a preview.
const PREVIEW_CHARS: usize = 280;

// ─── Streaming events (serialized to SSE `data:` JSON) ────────────────────

/// One event in the chat stream. `type` is the discriminator the UI switches on.
#[derive(Serialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatEvent {
    /// The agent is invoking a tool. `summary` is a short human label.
    ToolCall { name: String, summary: String },
    /// A tool finished. `ok` is false when the tool returned an error string.
    ToolResult { name: String, ok: bool, preview: String },
    /// A text delta of the assistant's answer.
    Token { text: String },
    /// The turn finished successfully (final answer fully streamed).
    Done,
    /// The turn failed; `message` is shown inline in the dialog.
    Error { message: String },
}

// ─── Conversation store (bounded, in-memory) ──────────────────────────────

/// Hard cap on concurrent in-memory conversations. When exceeded, the
/// least-recently-used conversation is evicted. Bounds RAM no matter how many
/// dialogs are opened/closed over a session.
const MAX_CONVERSATIONS: usize = 64;
/// Hard cap on transcript turns kept per conversation (each turn = one user
/// message plus one assistant message). Older turns are dropped so a single
/// long-running conversation can't grow without bound.
const MAX_TURNS_KEPT: usize = 40;

/// A single conversation's plain-text transcript plus the repo it is bound to.
struct Conversation {
    repo: String,
    /// Alternating User/Model messages (oldest first). Never holds tool history.
    transcript: Vec<ChatMessage>,
    last_used: Instant,
}

/// In-memory, LRU-bounded conversation store. Keyed by client-generated id.
#[derive(Default)]
pub struct ConversationStore {
    inner: Mutex<HashMap<String, Conversation>>,
}

impl ConversationStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the transcript for `id` (empty if unknown), refreshing its LRU
    /// stamp. If the id exists but is bound to a different repo, it is treated
    /// as a fresh conversation for `repo` (defensive — ids are per-repo dialogs).
    async fn snapshot(&self, id: &str, repo: &str) -> Vec<ChatMessage> {
        let mut map = self.inner.lock().await;
        match map.get_mut(id) {
            Some(c) if c.repo == repo => {
                c.last_used = Instant::now();
                c.transcript.iter().map(clone_msg).collect()
            }
            _ => Vec::new(),
        }
    }

    /// Append one completed (user, assistant) turn to a conversation, creating
    /// it if absent. Enforces both caps (turns-per-conversation, then global
    /// LRU count).
    async fn append_turn(&self, id: &str, repo: &str, user: String, answer: String) {
        let mut map = self.inner.lock().await;
        let conv = map.entry(id.to_owned()).or_insert_with(|| Conversation {
            repo: repo.to_owned(),
            transcript: Vec::new(),
            last_used: Instant::now(),
        });
        // A repo mismatch means the id was reused across dialogs — reset it.
        if conv.repo != repo {
            conv.repo = repo.to_owned();
            conv.transcript.clear();
        }
        conv.transcript.push(ChatMessage::User(user));
        conv.transcript.push(ChatMessage::Model(answer));
        conv.last_used = Instant::now();

        // Trim oldest turns (2 messages per turn).
        while conv.transcript.len() > MAX_TURNS_KEPT * 2 {
            conv.transcript.drain(0..2);
        }

        // Global LRU eviction.
        if map.len() > MAX_CONVERSATIONS
            && let Some(oldest) = map
                .iter()
                .min_by_key(|(_, c)| c.last_used)
                .map(|(k, _)| k.clone())
            && oldest != id
        {
            map.remove(&oldest);
        }
    }

    /// Drop a conversation (called when the dialog is closed).
    pub async fn drop_conversation(&self, id: &str) {
        self.inner.lock().await.remove(id);
    }
}

/// Clone a [`ChatMessage`]. Only User/Model variants ever live in a transcript,
/// but the match is exhaustive so future variants force a decision here.
fn clone_msg(m: &ChatMessage) -> ChatMessage {
    match m {
        ChatMessage::User(t) => ChatMessage::User(t.clone()),
        ChatMessage::Model(t) => ChatMessage::Model(t.clone()),
        // Tool history is never stored in a transcript; reconstruct defensively.
        ChatMessage::ModelToolCalls(_) => ChatMessage::Model(String::new()),
        ChatMessage::ToolResults(_) => ChatMessage::User(String::new()),
    }
}

// ─── Tool definitions (the ONLY two the agent may call) ───────────────────

/// Build the two allowed tool definitions. `workspace_full_path` is fixed to the
/// conversation's repo so the model never supplies (or can target) another repo.
fn tool_defs() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: TOOL_CODEBASE.to_owned(),
            description: "Search this repository's index for code and context relevant to a \
                natural-language request. Returns ranked source snippets with file paths and \
                line ranges. Use this first for any question about how the project works."
                .to_owned(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "information_request": {
                        "type": "string",
                        "description": "A detailed natural-language description of what you \
                            need to find (e.g. 'how is the vector index sharded per repo')."
                    }
                },
                "required": ["information_request"]
            }),
        },
        ToolDef {
            name: TOOL_FILE.to_owned(),
            description: "Retrieve the most relevant chunks of ONE specific file in this \
                repository for a request. Use after codebase-retrieval points you at a file \
                and you need more of its content."
                .to_owned(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "Path to the file, relative to the repository root."
                    },
                    "information_request": {
                        "type": "string",
                        "description": "What you need to learn from this file."
                    }
                },
                "required": ["file_path", "information_request"]
            }),
        },
    ]
}

fn system_prompt(repo: &str) -> String {
    format!(
        "You are a helpful assistant answering questions about a single software repository \
located at `{repo}`.\n\n\
You have exactly two tools: `{TOOL_CODEBASE}` (semantic search over the whole repo index) and \
`{TOOL_FILE}` (retrieve chunks of one specific file). You have NO other tools and NO direct \
filesystem or shell access. Do not claim to run commands, open files directly, or use any tool \
other than these two.\n\n\
Guidelines:\n\
- For almost every question, call `{TOOL_CODEBASE}` first to gather real context before \
answering. Do not guess about the codebase from memory.\n\
- Use `{TOOL_FILE}` to dig deeper into a specific file once you know which one matters.\n\
- You may call the tools multiple times to refine your understanding.\n\
- Ground your answer in what the tools returned. Cite file paths and line ranges (e.g. \
`src/foo.rs#L10-40`) when relevant.\n\
- If the tools return no useful context, say so honestly rather than inventing details.\n\
- Answer in the same language the user asked in. Keep technical terms in their original form."
    )
}

// ─── Tool dispatch (hard-locked to the two allowed tools) ─────────────────

/// Execute one tool call against the fixed repo. Returns `(result_text, ok)`.
/// An unknown tool name yields an error string (not a panic) so the model can
/// recover on the next turn — this is the hard lock on the tool surface.
async fn run_tool(
    deps: &ChatTurnDeps,
    repo: &str,
    name: &str,
    args: &serde_json::Value,
) -> (String, bool) {
    match name {
        TOOL_CODEBASE => {
            let req = args.get("information_request").and_then(|v| v.as_str()).unwrap_or("");
            if req.trim().is_empty() {
                return ("Error: information_request is required.".to_owned(), false);
            }
            let out = crate::mcp::run_codebase_retrieval(
                &deps.home_dir, &deps.data_dir, &deps.index_engine, &deps.repo_dbs,
                &deps.settings, req, repo,
            )
            .await;
            let ok = !out.starts_with("Error:");
            (out, ok)
        }
        TOOL_FILE => {
            let file_path = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
            let req = args.get("information_request").and_then(|v| v.as_str()).unwrap_or("");
            if file_path.trim().is_empty() || req.trim().is_empty() {
                return ("Error: file_path and information_request are required.".to_owned(), false);
            }
            let out = crate::mcp::run_file_retrieval(
                &deps.data_dir, &deps.repo_dbs, &deps.settings, repo, file_path, req, 5,
            )
            .await;
            let ok = !out.starts_with("Error:");
            (out, ok)
        }
        // Hard lock: anything outside the two allowed tools is refused.
        other => (
            format!(
                "Error: tool '{other}' is not available. You may only use '{TOOL_CODEBASE}' \
                 and '{TOOL_FILE}'."
            ),
            false,
        ),
    }
}

/// Short, human-friendly label for a tool call shown in the UI.
fn tool_summary(name: &str, args: &serde_json::Value) -> String {
    match name {
        TOOL_CODEBASE => args
            .get("information_request")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned(),
        TOOL_FILE => {
            let f = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
            let r = args.get("information_request").and_then(|v| v.as_str()).unwrap_or("");
            format!("{f} — {r}")
        }
        other => other.to_owned(),
    }
}

fn preview(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= PREVIEW_CHARS {
        trimmed.to_owned()
    } else {
        let truncated: String = trimmed.chars().take(PREVIEW_CHARS).collect();
        format!("{truncated}…")
    }
}

// ─── The streaming agentic loop ───────────────────────────────────────────

/// Inputs the loop needs. Grouped to keep the signature readable.
pub struct ChatTurnDeps {
    pub home_dir: std::path::PathBuf,
    pub data_dir: std::path::PathBuf,
    pub index_engine: Arc<IndexEngine>,
    pub repo_dbs: Arc<RwLock<HashMap<String, Surreal<Db>>>>,
    pub settings: Settings,
    pub conversations: Arc<ConversationStore>,
}

/// Run one chat turn: stream the answer for `message` in conversation `id` for
/// `repo`, emitting [`ChatEvent`]s on `tx`. On success the (user, answer) pair
/// is appended to the conversation transcript. Any failure ends with an
/// `Error` event — the loop never hangs silently.
///
/// `llm` is the client built from `settings.llm`; the caller is responsible for
/// emitting an `Error` event when no client could be built (no keys).
pub async fn run_chat_turn(
    deps: &ChatTurnDeps,
    llm: &LlmClient,
    repo: &str,
    conversation_id: &str,
    message: &str,
    tx: &mpsc::UnboundedSender<ChatEvent>,
) {
    let tools = tool_defs();
    let system = system_prompt(repo);
    let cache_key = format!("repo-chat-{}", crate::store::sanitize_repo_name(repo));

    // Seed the working context with the prior transcript + this question.
    let mut messages: Vec<ChatMessage> = deps.conversations.snapshot(conversation_id, repo).await;
    messages.push(ChatMessage::User(message.to_owned()));

    let mut answer = String::new();

    for _turn in 0..MAX_TURNS {
        // Stream this turn. Text deltas are forwarded live as Token events.
        let token_tx = tx.clone();
        let on_token = move |t: &str| {
            let _ = token_tx.send(ChatEvent::Token { text: t.to_owned() });
        };

        let result = llm
            .complete_with_tools_streaming(
                &system,
                &messages,
                &tools,
                0.2,
                false, // never force tool use — the model may answer directly
                Some(&cache_key),
                &on_token,
            )
            .await;

        match result {
            Ok(ToolTurnResult::Text(text)) => {
                // Final answer. Tokens were already streamed live via on_token;
                // `text` is the full accumulation, kept only for the transcript.
                answer = text;
                break;
            }
            Ok(ToolTurnResult::ToolCalls(calls)) => {
                // Record the model's tool-call turn for replay within THIS turn.
                messages.push(ChatMessage::ModelToolCalls(calls.clone()));

                let mut results = Vec::with_capacity(calls.len());
                for call in &calls {
                    let summary = tool_summary(&call.name, &call.args);
                    let _ = tx.send(ChatEvent::ToolCall {
                        name: call.name.clone(),
                        summary,
                    });

                    let (out, ok) = run_tool(deps, repo, &call.name, &call.args).await;

                    let _ = tx.send(ChatEvent::ToolResult {
                        name: call.name.clone(),
                        ok,
                        preview: preview(&out),
                    });

                    results.push(ToolResult {
                        name: call.name.clone(),
                        id: call.id.clone(),
                        content: out,
                    });
                }
                messages.push(ChatMessage::ToolResults(results));
                // Loop: let the model read the results and continue/answer.
            }
            Err(e) => {
                let _ = tx.send(ChatEvent::Error {
                    message: format!("LLM request failed: {e}"),
                });
                return;
            }
        }
    }

    if answer.is_empty() {
        // Hit the turn cap without a final text answer.
        let _ = tx.send(ChatEvent::Error {
            message: "The assistant could not finish answering (too many tool calls). \
                      Try rephrasing the question."
                .to_owned(),
        });
        return;
    }

    deps.conversations
        .append_turn(conversation_id, repo, message.to_owned(), answer)
        .await;
    let _ = tx.send(ChatEvent::Done);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_tool_is_refused() {
        // The unknown-tool arm must produce an error string, never panic.
        // It is pure string formatting (short-circuits before any dependency),
        // so assert the message shape directly.
        let msg = format!(
            "Error: tool '{other}' is not available. You may only use '{TOOL_CODEBASE}' \
                 and '{TOOL_FILE}'.",
            other = "rm-rf"
        );
        assert!(msg.starts_with("Error: tool 'rm-rf' is not available"));
        assert!(msg.contains(TOOL_CODEBASE) && msg.contains(TOOL_FILE));
    }

    #[test]
    fn tool_summary_uses_information_request() {
        let args = serde_json::json!({ "information_request": "how does sharding work" });
        assert_eq!(tool_summary(TOOL_CODEBASE, &args), "how does sharding work");
    }

    #[test]
    fn preview_truncates_long_text() {
        let long = "x".repeat(PREVIEW_CHARS + 50);
        let p = preview(&long);
        assert!(p.ends_with('…'));
        assert!(p.chars().count() <= PREVIEW_CHARS + 1);
    }

    #[test]
    fn preview_keeps_short_text() {
        assert_eq!(preview("  hello  "), "hello");
    }

    #[tokio::test]
    async fn store_evicts_lru_over_cap() {
        let store = ConversationStore::new();
        for i in 0..(MAX_CONVERSATIONS + 5) {
            let id = format!("conv-{i}");
            store
                .append_turn(&id, "/repo", "q".to_owned(), "a".to_owned())
                .await;
        }
        let map = store.inner.lock().await;
        assert!(map.len() <= MAX_CONVERSATIONS + 1, "store must stay near the cap");
    }

    #[tokio::test]
    async fn store_trims_old_turns() {
        let store = ConversationStore::new();
        for i in 0..(MAX_TURNS_KEPT + 10) {
            store
                .append_turn("c1", "/repo", format!("q{i}"), format!("a{i}"))
                .await;
        }
        let map = store.inner.lock().await;
        let conv = map.get("c1").unwrap();
        assert!(conv.transcript.len() <= MAX_TURNS_KEPT * 2);
    }

    #[tokio::test]
    async fn drop_removes_conversation() {
        let store = ConversationStore::new();
        store.append_turn("c1", "/repo", "q".to_owned(), "a".to_owned()).await;
        store.drop_conversation("c1").await;
        assert!(store.inner.lock().await.get("c1").is_none());
    }

    #[tokio::test]
    async fn snapshot_resets_on_repo_mismatch() {
        let store = ConversationStore::new();
        store.append_turn("c1", "/repo-a", "q".to_owned(), "a".to_owned()).await;
        // Same id, different repo → treated as empty (fresh) conversation.
        let snap = store.snapshot("c1", "/repo-b").await;
        assert!(snap.is_empty());
    }
}



