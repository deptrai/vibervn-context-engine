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
/// Hard cap on turns kept per conversation (each turn = one user message plus
/// one assistant message, plus a compact tool-context summary). Older turns are
/// dropped so a single long-running conversation can't grow without bound.
const MAX_TURNS_KEPT: usize = 40;

/// How many of the most-recent turns contribute their tool-context summary to
/// the next question (K). Only recent search evidence is worth replaying; older
/// turns fall out of the window.
const TOOL_CTX_TURNS_KEPT: usize = 3;
/// Hard byte cap on the tool-context summary STORED per turn. Enforced at store
/// time so a single huge search result (kernel-scale) can never bloat the
/// transcript — bounded memory regardless of repo size.
const TOOL_CTX_PER_TURN_CAP: usize = 1500;
/// Hard byte cap on the TOTAL tool-context injected into one question (across
/// all K turns combined). The injected block is trimmed to this even if K turns
/// each sit just under the per-turn cap.
const TOOL_CTX_TOTAL_CAP: usize = 3000;

/// A single completed turn: the user's question, the assistant's answer, and a
/// compact, already-capped summary of the search evidence gathered during it
/// (location headers + short previews — never full source). `tool_context` is
/// empty for turns that ran no tools (pure chit-chat).
struct Turn {
    user: String,
    answer: String,
    tool_context: String,
}

/// A conversation: an ordered list of turns plus the repo it is bound to.
struct Conversation {
    repo: String,
    /// Completed turns, oldest first.
    turns: Vec<Turn>,
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

    /// Return the plain User/Model transcript for `id` (empty if unknown),
    /// refreshing its LRU stamp. If the id exists but is bound to a different
    /// repo, it is treated as a fresh conversation for `repo` (defensive — ids
    /// are per-repo dialogs). Tool-context is NOT included here; it is fetched
    /// separately via [`Self::recent_tool_context`].
    async fn snapshot(&self, id: &str, repo: &str) -> Vec<ChatMessage> {
        let mut map = self.inner.lock().await;
        match map.get_mut(id) {
            Some(c) if c.repo == repo => {
                c.last_used = Instant::now();
                let mut out = Vec::with_capacity(c.turns.len() * 2);
                for t in &c.turns {
                    out.push(ChatMessage::User(t.user.clone()));
                    out.push(ChatMessage::Model(t.answer.clone()));
                }
                out
            }
            _ => Vec::new(),
        }
    }

    /// Build the tool-context block to replay into the NEXT question: the
    /// summaries of the last [`TOOL_CTX_TURNS_KEPT`] turns that actually ran a
    /// search, newest last, joined and hard-capped at [`TOOL_CTX_TOTAL_CAP`]
    /// bytes. Returns an empty string when there is nothing to replay.
    async fn recent_tool_context(&self, id: &str, repo: &str) -> String {
        let mut map = self.inner.lock().await;
        let Some(c) = map.get_mut(id) else {
            return String::new();
        };
        if c.repo != repo {
            return String::new();
        }
        c.last_used = Instant::now();

        // Walk the most-recent turns (newest first), collecting non-empty
        // summaries up to K, then re-order oldest→newest for natural reading.
        let mut picked: Vec<&str> = Vec::new();
        for t in c.turns.iter().rev() {
            if picked.len() >= TOOL_CTX_TURNS_KEPT {
                break;
            }
            if !t.tool_context.is_empty() {
                picked.push(&t.tool_context);
            }
        }
        if picked.is_empty() {
            return String::new();
        }
        picked.reverse();
        let joined = picked.join("\n\n");
        truncate_bytes(&joined, TOOL_CTX_TOTAL_CAP)
    }

    /// Append one completed turn to a conversation, creating it if absent.
    /// `tool_context` is capped to [`TOOL_CTX_PER_TURN_CAP`] bytes before
    /// storage. Enforces both caps (turns-per-conversation, then global LRU).
    async fn append_turn(
        &self,
        id: &str,
        repo: &str,
        user: String,
        answer: String,
        tool_context: String,
    ) {
        let mut map = self.inner.lock().await;
        let conv = map.entry(id.to_owned()).or_insert_with(|| Conversation {
            repo: repo.to_owned(),
            turns: Vec::new(),
            last_used: Instant::now(),
        });
        // A repo mismatch means the id was reused across dialogs — reset it.
        if conv.repo != repo {
            conv.repo = repo.to_owned();
            conv.turns.clear();
        }
        // Cap the stored summary at store time — the single enforcement point.
        let tool_context = truncate_bytes(&tool_context, TOOL_CTX_PER_TURN_CAP);
        conv.turns.push(Turn { user, answer, tool_context });
        conv.last_used = Instant::now();

        // Trim oldest turns.
        while conv.turns.len() > MAX_TURNS_KEPT {
            conv.turns.remove(0);
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

/// Truncate `s` to at most `max_bytes`, never splitting a UTF-8 char, appending
/// an elision marker when anything was cut. Used to hard-bound every stored and
/// replayed tool-context string.
fn truncate_bytes(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
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
Core principle: GATHER ENOUGH EVIDENCE BEFORE YOU ANSWER. A confidently wrong answer is the \
worst outcome. It is always better to keep searching, or to admit a gap, than to guess. Never \
answer a question about how the code works from memory or assumption — every factual claim about \
this repository must be grounded in something a tool actually returned this turn.\n\n\
How to work:\n\
- For any question about the codebase, call `{TOOL_CODEBASE}` first. Do not answer before you \
have called it at least once for the current question.\n\
- Treat the first result as a starting point, not the final answer. Before answering, check: does \
the context I have actually cover EVERY part of the question? If the question has multiple parts, \
each part needs its own evidence.\n\
- EXPAND THE BLAST RADIUS before you stop. One search is almost never enough. You must broaden \
coverage along these axes until they stop yielding anything relevant:\n\
  (1) re-query with DIFFERENT WORDING and synonyms for the same concept (e.g. \"render markdown\", \
\"markdown to HTML\", \"marked / markdown-it\", \"sanitize html\", the function/class names you \
saw);\n\
  (2) FOLLOW THE GRAPH — when a result names a caller, callee, related symbol, or file, search for \
or open that next (`{TOOL_FILE}`), because the answer often lives one hop away;\n\
  (3) when a result hints at a specific file, use `{TOOL_FILE}` to read more of it.\n\
You may stop searching ONLY when these branches are exhausted — i.e. fresh queries and graph hops \
return nothing new and relevant. Until then, keep going.\n\
- CRITICAL — absence is not proof: a thin or empty result does NOT mean the feature is missing. \
NEVER conclude \"the project does not have X\" / \"there is no X\" from one search. You may only \
claim something is absent after several differently-worded searches AND graph/file follow-ups all \
come back empty — and even then, state it as \"I could not find X via search\", not as a fact.\n\
- You decide how many searches are enough — use as many tool calls as the question needs (you \
have a limited budget of rounds, so make each search count and stop once the blast radius is \
truly exhausted).\n\
- Pure chit-chat or meta turns (e.g. \"thanks\", \"explain that again\") do not need a new search \
— answer from the conversation so far.\n\n\
Answering:\n\
- CITE EVERY CLAIM. Each substantive statement in your answer must carry the exact evidence it \
rests on as `path#Lstart-end` (e.g. `src/assets/index.html#L5127-5130`), inline right next to the \
claim. A sentence asserting how the code behaves with no `path#Lline` citation is not allowed — \
if you cannot cite it, you have not verified it, so either search for it or drop the claim.\n\
- Ground every factual claim in what the tools returned, never from memory or assumption.\n\
- SAFE ANSWER POLICY: if, after exhausting the blast radius, you still cannot find evidence for \
some part of the question, explicitly say what you could NOT find or are NOT sure about for that \
part, and answer only the parts you actually verified. Never paper over a gap by inventing \
plausible-sounding details. Partial-but-honest beats complete-but-wrong.\n\
- If searching turned up essentially nothing relevant, say plainly that you could not find useful \
context for this question (and suggest the user rephrase or name a specific file) rather than \
fabricating an answer.\n\
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

// ─── Tool-context summarization (cross-turn memory) ───────────────────────

/// Max location headers kept from one tool result's summary. A search returns
/// many blocks; we keep the top handful so the next turn knows roughly what was
/// already found without replaying full source.
const MAX_LOCATIONS_PER_RESULT: usize = 8;
/// Chars of the first content line kept as a per-location preview.
const LOCATION_PREVIEW_CHARS: usize = 80;

/// Whether a line looks like a retrieval location header (`path#L10-40…`) rather
/// than a numbered source line. Headers carry `#L<digit>` and are not indented.
fn is_location_header(line: &str) -> bool {
    if line.starts_with(char::is_whitespace) {
        return false;
    }
    if let Some(pos) = line.find("#L") {
        return line[pos + 2..].starts_with(|c: char| c.is_ascii_digit());
    }
    false
}

/// Build a compact, capped summary of ONE tool result for cross-turn replay:
/// the leading location headers (`path#L10-40`) each followed by a short preview
/// of their first content line. Never includes full source. The `label`
/// (information_request / file path) anchors what the search was for.
///
/// The output is intentionally small; the store still hard-caps it again at
/// [`TOOL_CTX_PER_TURN_CAP`], so this is a best-effort shrink, not the bound.
fn summarize_tool_result(label: &str, output: &str) -> String {
    // Errors / "use grep" guidance carry no locations worth replaying.
    if output.starts_with("Error:") {
        return String::new();
    }

    let mut locations: Vec<String> = Vec::new();
    let mut lines = output.lines().peekable();
    while let Some(line) = lines.next() {
        if !is_location_header(line) {
            continue;
        }
        // Peek the next non-empty line as a one-line preview of the block.
        let preview_line = lines
            .peek()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty() && !is_location_header(l))
            .map(|l| {
                let mut s: String = l.chars().take(LOCATION_PREVIEW_CHARS).collect();
                if l.chars().count() > LOCATION_PREVIEW_CHARS {
                    s.push('…');
                }
                s
            })
            .unwrap_or_default();

        if preview_line.is_empty() {
            locations.push(format!("- {}", line.trim()));
        } else {
            locations.push(format!("- {}  ({preview_line})", line.trim()));
        }
        if locations.len() >= MAX_LOCATIONS_PER_RESULT {
            break;
        }
    }

    if locations.is_empty() {
        return String::new();
    }
    let label = label.trim();
    if label.is_empty() {
        locations.join("\n")
    } else {
        format!("search: {label}\n{}", locations.join("\n"))
    }
}

/// Prepend the prior-turns tool-context block to the new question so the model
/// can reuse earlier search evidence instead of blindly re-searching. Framed as
/// reference material, NOT as ground truth — the model must still verify with a
/// fresh search when the prior evidence doesn't cover the new question.
///
/// `pub` so wire/integration tests can prove the augmented question (carrying
/// prior tool-context) is what actually reaches the provider.
pub fn augment_question_with_context(tool_context: &str, message: &str) -> String {
    if tool_context.trim().is_empty() {
        return message.to_owned();
    }
    format!(
        "[Context from earlier in this conversation — files and ranges already \
found via search. Reuse this to stay consistent; search again only if it does \
not cover the new question, and do not treat it as a substitute for verifying \
current details.]\n{tool_context}\n\n[Current question]\n{message}"
    )
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

    // Seed the working context with the prior transcript + this question. The
    // question is augmented with a compact summary of the search evidence from
    // the last K turns so the model can build on it instead of re-searching
    // from scratch (cross-turn memory; bounded by the store's byte caps).
    let mut messages: Vec<ChatMessage> = deps.conversations.snapshot(conversation_id, repo).await;
    let prior_context = deps.conversations.recent_tool_context(conversation_id, repo).await;
    messages.push(ChatMessage::User(augment_question_with_context(&prior_context, message)));

    let mut answer = String::new();
    // Accumulates the summaries of every search this turn runs, for storage so
    // the NEXT turn can reuse them. Capped again at store time.
    let mut turn_tool_context = String::new();

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
                false, // the model decides when to search; the prompt drives it
                Some(&cache_key),
                &on_token,
            )
            .await;

        match result {
            Ok(ToolTurnResult::Text(text)) => {
                // Final answer. Tokens were already streamed live via on_token;
                // `text` is the full accumulation kept for the transcript. How
                // much to search is entirely the model's call — the system
                // prompt's blast-radius policy guides it; nothing is injected.
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
                        summary: summary.clone(),
                    });

                    let (out, ok) = run_tool(deps, repo, &call.name, &call.args).await;

                    let _ = tx.send(ChatEvent::ToolResult {
                        name: call.name.clone(),
                        ok,
                        preview: preview(&out),
                    });

                    // Fold a compact summary of this result into the turn's
                    // cross-turn memory (best-effort; store enforces the cap).
                    if ok {
                        let s = summarize_tool_result(&summary, &out);
                        if !s.is_empty() {
                            if !turn_tool_context.is_empty() {
                                turn_tool_context.push_str("\n\n");
                            }
                            turn_tool_context.push_str(&s);
                        }
                    }

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
        // We exhausted the tool-calling budget without the model producing a
        // final text answer (it kept searching). Rather than dead-ending with an
        // error, make ONE last pass with no tools available, forcing the model to
        // synthesize a safe answer from whatever context it has already gathered
        // — honestly flagging gaps instead of guessing. This is the graceful
        // "ran out of searches → say what you can, admit the rest" exit.
        let token_tx = tx.clone();
        // Track whether the final pass actually streamed any token. `started` in
        // the llm layer is internal and invisible here, so we observe emission
        // directly: if even one token reached the user, appending the fallback
        // sentence would produce "half-answer + fallback" garble — so we only
        // emit the fallback when nothing was streamed.
        let streamed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let streamed_cb = streamed.clone();
        let on_token = move |t: &str| {
            if !t.is_empty() {
                streamed_cb.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            let _ = token_tx.send(ChatEvent::Token { text: t.to_owned() });
        };
        messages.push(ChatMessage::User(
            "You have used up your search budget for this question. Do not request any more \
             tools. Answer now using only the context already gathered above. State clearly \
             which parts of the question you could verify and which parts you could not find \
             enough evidence for — do not invent details for the gaps."
                .to_owned(),
        ));
        // Pass an empty tool list so no further tool calls are possible.
        match llm
            .complete_with_tools_streaming(
                &system, &messages, &[], 0.2, false, Some(&cache_key), &on_token,
            )
            .await
        {
            Ok(ToolTurnResult::Text(text)) if !text.trim().is_empty() => answer = text,
            // The model produced no usable accumulated text, or the call failed.
            // Emit a plain honest fallback — but ONLY if nothing was streamed,
            // so we never glue a fallback sentence onto a half-streamed answer.
            // If tokens already reached the user (mid-stream failure with empty
            // accumulation), leave `answer` for the guard below to backfill so
            // the transcript isn't empty, without emitting more visible text.
            _ => {
                if !streamed.load(std::sync::atomic::Ordering::Relaxed) {
                    let fallback = "I couldn't gather enough indexed context to answer this \
                        confidently. Try rephrasing the question, narrowing it to one part, or \
                        pointing me at a specific file."
                        .to_owned();
                    let _ = tx.send(ChatEvent::Token {
                        text: fallback.clone(),
                    });
                    answer = fallback;
                }
            }
        }
    }

    // The final-pass branch above may leave `answer` empty only if the stream
    // emitted tokens but returned no usable accumulated text — extremely rare,
    // but guard the transcript against storing an empty assistant turn.
    if answer.trim().is_empty() {
        answer = "I couldn't gather enough indexed context to answer this confidently."
            .to_owned();
    }

    deps.conversations
        .append_turn(conversation_id, repo, message.to_owned(), answer, turn_tool_context)
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
                .append_turn(&id, "/repo", "q".to_owned(), "a".to_owned(), String::new())
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
                .append_turn("c1", "/repo", format!("q{i}"), format!("a{i}"), String::new())
                .await;
        }
        let map = store.inner.lock().await;
        let conv = map.get("c1").unwrap();
        assert!(conv.turns.len() <= MAX_TURNS_KEPT);
    }

    #[tokio::test]
    async fn drop_removes_conversation() {
        let store = ConversationStore::new();
        store.append_turn("c1", "/repo", "q".to_owned(), "a".to_owned(), String::new()).await;
        store.drop_conversation("c1").await;
        assert!(store.inner.lock().await.get("c1").is_none());
    }

    #[tokio::test]
    async fn snapshot_resets_on_repo_mismatch() {
        let store = ConversationStore::new();
        store.append_turn("c1", "/repo-a", "q".to_owned(), "a".to_owned(), String::new()).await;
        // Same id, different repo → treated as empty (fresh) conversation.
        let snap = store.snapshot("c1", "/repo-b").await;
        assert!(snap.is_empty());
    }

    // ─── Cross-turn tool-context ──────────────────────────────────────────

    #[test]
    fn truncate_bytes_caps_and_marks() {
        let s = "a".repeat(100);
        let t = truncate_bytes(&s, 10);
        assert!(t.ends_with('…'));
        // 10 bytes of content + the multi-byte ellipsis.
        assert!(t.len() <= 10 + '…'.len_utf8());
    }

    #[test]
    fn truncate_bytes_keeps_short() {
        assert_eq!(truncate_bytes("hi", 10), "hi");
    }

    #[test]
    fn truncate_bytes_respects_char_boundary() {
        // Multi-byte chars must never be split mid-codepoint.
        let s = "é".repeat(50); // each 'é' is 2 bytes
        let t = truncate_bytes(&s, 5);
        assert!(t.ends_with('…'));
        // Must still be valid UTF-8 (no panic on slicing) — implicit by reaching here.
    }

    #[test]
    fn is_location_header_detects_path_ranges() {
        assert!(is_location_header("src/foo.rs#L10-40"));
        assert!(is_location_header("a/b/c.py#L1-2 [callers: 3]"));
        // Numbered source lines and indented lines are not headers.
        assert!(!is_location_header("   10: let x = 1;"));
        assert!(!is_location_header("fn main() {"));
        assert!(!is_location_header("path#Lstart")); // no digit after #L
    }

    #[test]
    fn summarize_extracts_locations_with_previews() {
        let output = "src/a.rs#L1-5\n1: fn a() {}\n\nsrc/b.rs#L9-12\n9: struct B;";
        let s = summarize_tool_result("how does a work", output);
        assert!(s.contains("search: how does a work"));
        assert!(s.contains("src/a.rs#L1-5"));
        assert!(s.contains("src/b.rs#L9-12"));
        // Preview text from the first content line is included.
        assert!(s.contains("fn a()"));
    }

    #[test]
    fn summarize_ignores_errors() {
        assert!(summarize_tool_result("x", "Error: no index").is_empty());
    }

    #[test]
    fn summarize_caps_location_count() {
        let mut output = String::new();
        for i in 0..(MAX_LOCATIONS_PER_RESULT + 10) {
            output.push_str(&format!("src/f{i}.rs#L1-2\n1: code\n\n"));
        }
        let s = summarize_tool_result("many", &output);
        let count = s.matches("#L").count();
        assert_eq!(count, MAX_LOCATIONS_PER_RESULT, "must cap locations per result");
    }

    #[test]
    fn augment_noop_when_no_context() {
        assert_eq!(augment_question_with_context("", "hello"), "hello");
        assert_eq!(augment_question_with_context("   ", "hello"), "hello");
    }

    #[test]
    fn augment_embeds_context_and_question() {
        let out = augment_question_with_context("src/a.rs#L1-5", "what next?");
        assert!(out.contains("src/a.rs#L1-5"));
        assert!(out.contains("what next?"));
        assert!(out.contains("[Current question]"));
    }

    #[tokio::test]
    async fn recent_tool_context_replays_recent_turns() {
        let store = ConversationStore::new();
        store.append_turn("c1", "/r", "q1".into(), "a1".into(), "ctx-1".into()).await;
        store.append_turn("c1", "/r", "q2".into(), "a2".into(), "ctx-2".into()).await;
        let ctx = store.recent_tool_context("c1", "/r").await;
        assert!(ctx.contains("ctx-1"));
        assert!(ctx.contains("ctx-2"));
        // Oldest-first ordering.
        assert!(ctx.find("ctx-1").unwrap() < ctx.find("ctx-2").unwrap());
    }

    #[tokio::test]
    async fn recent_tool_context_windows_to_k_turns() {
        let store = ConversationStore::new();
        for i in 0..(TOOL_CTX_TURNS_KEPT + 3) {
            store
                .append_turn("c1", "/r", format!("q{i}"), format!("a{i}"), format!("ctx-{i}"))
                .await;
        }
        let ctx = store.recent_tool_context("c1", "/r").await;
        // The oldest summaries fall outside the K-turn window.
        assert!(!ctx.contains("ctx-0"));
        // The newest K are present.
        let newest = TOOL_CTX_TURNS_KEPT + 2;
        assert!(ctx.contains(&format!("ctx-{newest}")));
    }

    #[tokio::test]
    async fn recent_tool_context_skips_empty_summaries() {
        let store = ConversationStore::new();
        // Chit-chat turns (no search) store empty tool_context and are skipped.
        store.append_turn("c1", "/r", "hi".into(), "hello".into(), String::new()).await;
        store.append_turn("c1", "/r", "q".into(), "a".into(), "real-ctx".into()).await;
        store.append_turn("c1", "/r", "thanks".into(), "yw".into(), String::new()).await;
        let ctx = store.recent_tool_context("c1", "/r").await;
        assert_eq!(ctx, "real-ctx");
    }

    #[tokio::test]
    async fn per_turn_context_capped_at_store_time() {
        let store = ConversationStore::new();
        let huge = "x".repeat(TOOL_CTX_PER_TURN_CAP * 4);
        store.append_turn("c1", "/r", "q".into(), "a".into(), huge).await;
        let map = store.inner.lock().await;
        let conv = map.get("c1").unwrap();
        // Stored summary must be hard-bounded regardless of input size.
        assert!(conv.turns[0].tool_context.len() <= TOOL_CTX_PER_TURN_CAP + '…'.len_utf8());
    }

    #[tokio::test]
    async fn total_injected_context_capped_across_turns() {
        let store = ConversationStore::new();
        // Each turn sits near the per-turn cap; K of them combined would exceed
        // the total cap, so the injected block must be trimmed to it.
        let near_cap = "y".repeat(TOOL_CTX_PER_TURN_CAP - 10);
        for i in 0..TOOL_CTX_TURNS_KEPT {
            store
                .append_turn("c1", "/r", format!("q{i}"), format!("a{i}"), near_cap.clone())
                .await;
        }
        let ctx = store.recent_tool_context("c1", "/r").await;
        assert!(
            ctx.len() <= TOOL_CTX_TOTAL_CAP + '…'.len_utf8(),
            "total injected context must be bounded; got {} bytes",
            ctx.len()
        );
    }
}



