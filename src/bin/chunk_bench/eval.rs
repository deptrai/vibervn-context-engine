//! Retrieval eval set for the chunking-quality benchmark.
//!
//! ~40 (query, expected_file, expected_symbol) triples derived from the
//! notepad-ade source. The expected line range is NOT hardcoded — it is
//! resolved at run time from the symbol name via the FROZEN extraction
//! (`parse_file`), so the eval set is independent of any chunk boundary and
//! cannot favour either chunker. Each query is phrased as a natural-language
//! intent a developer would type, not as the literal symbol name.
//!
//! `expected_file` is relative to the repo root and uses forward slashes.

/// Returns the (query, relative_file, symbol_name) eval triples.
pub fn eval_set() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        // CronExpression.cpp
        ("parse a cron expression string into a schedule", "NotepadADE/src/CronExpression.cpp", "parse"),
        ("compute the next time a cron schedule fires", "NotepadADE/src/CronExpression.cpp", "nextFireTime"),
        ("list the upcoming fire times for a cron schedule", "NotepadADE/src/CronExpression.cpp", "nextFireTimes"),
        // ai/SsePartialParser.cpp
        ("extract the streamed token text from an SSE json chunk", "NotepadADE/src/ai/SsePartialParser.cpp", "extractToken"),
        ("detect the finish/stop signal in a streaming response", "NotepadADE/src/ai/SsePartialParser.cpp", "hasFinishSignal"),
        ("feed new bytes into the server-sent-events parser", "NotepadADE/src/ai/SsePartialParser.cpp", "feed"),
        ("parse a single SSE event payload", "NotepadADE/src/ai/SsePartialParser.cpp", "processEvent"),
        // ai/DiffCompressor.cpp
        ("compress a unified diff to fit a byte budget", "NotepadADE/src/ai/DiffCompressor.cpp", "compress"),
        ("parse a diff into per-file groups", "NotepadADE/src/ai/DiffCompressor.cpp", "parseGroups"),
        ("serialize file groups back into diff text", "NotepadADE/src/ai/DiffCompressor.cpp", "serializeGroups"),
        // AcpErrorClassifier.cpp
        ("classify an agent error message into a kind", "NotepadADE/src/AcpErrorClassifier.cpp", "classify"),
        ("produce a user-friendly error message from an error kind", "NotepadADE/src/AcpErrorClassifier.cpp", "friendlyMessage"),
        ("build a login hint for a failed agent command", "NotepadADE/src/AcpErrorClassifier.cpp", "loginHint"),
        // ai/PromptAssembler.cpp
        ("assemble the final prompt from template and blocks", "NotepadADE/src/ai/PromptAssembler.cpp", "assemble"),
        ("render the rules block of a prompt", "NotepadADE/src/ai/PromptAssembler.cpp", "renderRulesBlock"),
        ("render the diff section of a commit prompt", "NotepadADE/src/ai/PromptAssembler.cpp", "renderDiffBlock"),
        ("provide the default prompt template", "NotepadADE/src/ai/PromptAssembler.cpp", "defaultTemplate"),
        // ai/LlmHttpClient.cpp
        ("build the JSON request payload for the LLM call", "NotepadADE/src/ai/LlmHttpClient.cpp", "buildPayload"),
        ("open a streaming HTTP connection to the LLM", "NotepadADE/src/ai/LlmHttpClient.cpp", "openStream"),
        ("normalize the chat completions endpoint URL", "NotepadADE/src/ai/LlmHttpClient.cpp", "normalizeChatCompletionsUrl"),
        ("cancel the in-flight LLM request", "NotepadADE/src/ai/LlmHttpClient.cpp", "cancel"),
        ("handle incoming bytes as they arrive on the stream", "NotepadADE/src/ai/LlmHttpClient.cpp", "onReadyRead"),
        // ai/RulesLocator.cpp
        ("locate the rules file for a workspace", "NotepadADE/src/ai/RulesLocator.cpp", "locate"),
        ("truncate rules content to a byte budget", "NotepadADE/src/ai/RulesLocator.cpp", "truncateToBudget"),
        ("read a file only if it exists", "NotepadADE/src/ai/RulesLocator.cpp", "readIfExists"),
        // ai/CommitMessageGenerator.cpp
        ("trigger generation of a commit message", "NotepadADE/src/ai/CommitMessageGenerator.cpp", "trigger"),
        ("check whether commit message generation can fire", "NotepadADE/src/ai/CommitMessageGenerator.cpp", "canFireGenerate"),
        // ai/PromptImprover.cpp
        ("check whether the prompt can be improved", "NotepadADE/src/ai/PromptImprover.cpp", "canImprove"),
        ("trigger prompt improvement from a user draft", "NotepadADE/src/ai/PromptImprover.cpp", "trigger"),
        // AcpProtocol.cpp
        ("extract complete framed messages from a byte buffer", "NotepadADE/src/AcpProtocol.cpp", "acpExtractFrames"),
        ("pick the auto-approve permission option", "NotepadADE/src/AcpProtocol.cpp", "pickAutoApproveOptionId"),
        ("check if a path is inside the working directory", "NotepadADE/src/AcpProtocol.cpp", "pathIsInsideWorkingDir"),
        ("quote an argument for a windows command line", "NotepadADE/src/AcpProtocol.cpp", "windowsCommandLineQuote"),
        // AcpAgentRegistry.cpp
        ("load the agent registry from disk", "NotepadADE/src/AcpAgentRegistry.cpp", "load"),
        ("persist user-defined agents", "NotepadADE/src/AcpAgentRegistry.cpp", "persistUserAgents"),
        ("the built-in claude code agent definition", "NotepadADE/src/AcpAgentRegistry.cpp", "builtinClaudeCodeDefinition"),
        // AcpConnection.cpp
        ("spawn an agent process for a connection", "NotepadADE/src/AcpConnection.cpp", "spawn"),
        ("set the auto-approve policy provider callback", "NotepadADE/src/AcpConnection.cpp", "setAutoApprovePolicyProvider"),
        // AcpHistoryStore.cpp
        ("compute the file path for a session's history", "NotepadADE/src/AcpHistoryStore.cpp", "filePathForSession"),
        ("ensure a debounce timer exists for a session", "NotepadADE/src/AcpHistoryStore.cpp", "ensureTimer"),
        // AcpAgentManager.cpp
        ("open an agent dock for an agent id", "NotepadADE/src/AcpAgentManager.cpp", "openAgent"),
        ("delete a session's stored history", "NotepadADE/src/AcpAgentManager.cpp", "deleteSessionHistory"),
        ("restart an agent session", "NotepadADE/src/AcpAgentManager.cpp", "restartSession"),
    ]
}
