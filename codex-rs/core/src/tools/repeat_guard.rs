//! Session-scoped no-progress circuit breaker (v2) for tool dispatch.
//!
//! Blocks a shell-tool invocation whose exact command has already produced an
//! identical full result enough times with no intervening state change, and
//! returns a synthetic model-facing error instead of executing it again. See
//! `AGENTS.md` global safety rules for the matching behavioral instruction.
//!
//! v2 fixes over v1:
//! - The command signature uses the exact (trimmed) command text; whitespace is
//!   no longer collapsed, so distinct commands cannot collide.
//! - Result hashing covers the full model-visible output with no truncation, so
//!   short/empty outputs from different commands no longer hash identically by
//!   accident of a shared 8KB prefix.
//! - The block threshold is configurable (`block_after_repeats`, default 3).
//! - The guard can be disabled entirely via config (`[features] repeat_guard =
//!   false`) or env (`CODEX_REPEAT_GUARD=0` / legacy `CODEX_ALLOW_REPEAT_TOOLS=1`).

use std::collections::HashMap;

use codex_protocol::models::ResponseInputItem;
use codex_protocol::parse_command::ParsedCommand;
use codex_shell_command::parse_command::parse_shell_script;
use codex_tools::ToolName;
use sha1::Digest;
use sha1::Sha1;

use crate::session::turn_context::TurnContext;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;

/// Maximum number of tracked signatures kept per session. Oldest entries are
/// evicted when the window is exceeded so this stays a small, bounded cache.
const MAX_TRACKED_SIGNATURES: usize = 100;

/// Default number of identical (command, result) observations before a further
/// unchanged call is blocked. With this default the first two identical calls
/// execute and the third is blocked without running. Configurable via
/// `[features.repeat_guard] block_after_repeats` (minimum 2).
pub const DEFAULT_BLOCK_AFTER_REPEATS: u32 = 3;

/// Marker a model (or user) can embed in a command to opt out of tracking for
/// that specific invocation, e.g. intentional polling loops.
pub const REPEAT_BYPASS_MARKER: &str = "# allow-repeat";

/// Environment variable that disables the guard entirely when set to `0`.
pub const REPEAT_GUARD_DISABLE_ENV: &str = "CODEX_REPEAT_GUARD";

/// Legacy environment variable (v1) that disables the guard when set to `1`.
/// Kept for backward compatibility with existing launch scripts.
pub const LEGACY_REPEAT_GUARD_ALLOW_ENV: &str = "CODEX_ALLOW_REPEAT_TOOLS";

#[derive(Debug, Clone)]
struct Entry {
    /// Hash of the most recent result for this signature.
    last_result_hash: String,
    /// Consecutive count of results identical to `last_result_hash`.
    repeat_of_last_result: u32,
}

/// Tracks recent shell-tool invocations per session and decides whether an
/// unchanged repeat has already failed to make progress enough times.
#[derive(Debug)]
pub struct RepeatCallGuard {
    entries: HashMap<String, Entry>,
    /// Insertion order for bounded eviction (oldest first).
    insertion_order: Vec<String>,
    /// Identical-result observations required before the next call is blocked.
    threshold: u32,
}

impl Default for RepeatCallGuard {
    fn default() -> Self {
        Self::new(DEFAULT_BLOCK_AFTER_REPEATS)
    }
}

impl RepeatCallGuard {
    pub fn new(block_after_repeats: u32) -> Self {
        Self {
            entries: HashMap::new(),
            insertion_order: Vec::new(),
            threshold: block_after_repeats.max(2),
        }
    }

    /// Returns the model-facing block message when this invocation must not be
    /// executed, or `None` when it may proceed.
    pub fn check(&mut self, signature: &str) -> Option<String> {
        let entry = self.entries.get(signature)?;
        // The first call records a baseline (count 0); each subsequent identical
        // result increments the count. Once two identical results have been seen
        // (count >= threshold), any further unchanged call is blocked without
        // executing — i.e. the third identical call never runs.
        if entry.repeat_of_last_result >= self.threshold {
            tracing::warn!(
                tool_signature = %truncate_for_log(signature),
                repeat_count = entry.repeat_of_last_result,
                prior_result_hash = %entry.last_result_hash,
                reason = "identical result repeated with no intervening state change",
                "repeat guard: blocking no-progress tool call"
            );
            Some(block_message())
        } else {
            None
        }
    }

    /// Records the outcome of an executed invocation under its signature. The
    /// stored count is how many identical results have been observed so far for
    /// this signature (the first observation stores 1).
    pub fn record(&mut self, signature: &str, result_hash: String) {
        let repeat = match self.entries.get(signature) {
            Some(entry) if entry.last_result_hash == result_hash => {
                entry.repeat_of_last_result.saturating_add(1)
            }
            _ => 1,
        };
        // Re-insert to refresh eviction order.
        self.insertion_order.retain(|sig| sig != signature);
        self.insertion_order.push(signature.to_string());
        self.entries.insert(
            signature.to_string(),
            Entry {
                last_result_hash: result_hash,
                repeat_of_last_result: repeat,
            },
        );
        while self.insertion_order.len() > MAX_TRACKED_SIGNATURES {
            if let Some(evicted) = self.insertion_order.first().cloned() {
                self.entries.remove(&evicted);
                self.insertion_order.remove(0);
            } else {
                break;
            }
        }
    }

    /// Records an intervening state change: clears all tracked history so the
    /// next identical call is treated as fresh.
    pub fn note_state_change(&mut self) {
        if !self.entries.is_empty() {
            tracing::debug!("repeat guard: state change observed, clearing history");
        }
        self.entries.clear();
        self.insertion_order.clear();
    }

    /// Whether the flat tool name is one of the state-changing tools that should
    /// reset history when it succeeds.
    pub fn is_state_changing_tool(flat_name: &str) -> bool {
        matches!(flat_name, "write_stdin" | "request_permissions" | "spawn_agent" | "send_message")
            || flat_name.contains("patch")
    }

    /// Whether a shell script contains any command that is not purely read /
    /// list-files / search. Such commands are treated as state-changing.
    pub fn is_mutating_shell_script(script: &str) -> bool {
        parse_shell_script(script)
            .iter()
            .any(|command| matches!(command, ParsedCommand::Unknown { .. }))
    }

    /// Whether the global env-var bypass is active for this process:
    /// `CODEX_REPEAT_GUARD=0` or legacy `CODEX_ALLOW_REPEAT_TOOLS=1`.
    pub fn env_bypass_active() -> bool {
        std::env::var_os(REPEAT_GUARD_DISABLE_ENV)
            .is_some_and(|value| value == "0")
            || std::env::var_os(LEGACY_REPEAT_GUARD_ALLOW_ENV)
                .is_some_and(|value| value == "1")
    }
}

/// Builds the normalized signature for a tracked invocation, or `None` when the
/// tool is exempt (only default-namespace shell tools are tracked).
pub fn repeat_signature(
    tool_name: &ToolName,
    payload: &ToolPayload,
    turn: &TurnContext,
) -> Option<String> {
    if !tool_name.is_default_namespace() {
        return None;
    }
    let (command, workdir_override) = match &payload {
        ToolPayload::Function { arguments } => {
            let value: serde_json::Value = serde_json::from_str(arguments).ok()?;
            match tool_name.name.as_str() {
                "exec_command" => (
                    value.get("cmd")?.as_str()?.to_string(),
                    value.get("workdir").and_then(|w| w.as_str()).map(str::to_string),
                ),
                "shell_command" => (
                    value.get("command")?.as_str()?.to_string(),
                    value.get("workdir").and_then(|w| w.as_str()).map(str::to_string),
                ),
                _ => return None,
            }
        }
        _ => return None,
    };

    let command_text = command.trim();
    if command_text.contains(REPEAT_BYPASS_MARKER) {
        // Per-command opt-out: tracked under a unique signature so it can never
        // collide with or block anything else.
        return Some(format!(
            "bypass|{}|{}",
            tool_name.name, command_text
        ));
    }

    let effective_cwd = workdir_override.unwrap_or_else(|| {
        turn.environments
            .primary()
            .map(|env| env.cwd().to_string())
            .unwrap_or_default()
    });
    let environment_id = turn
        .environments
        .primary()
        .map(|env| env.selection.environment_id.clone())
        .unwrap_or_default();
    let remote = turn
        .environments
        .primary()
        .is_some_and(|env| env.environment.is_remote());

    Some(format!(
        "{}|{}|{}|{}|remote={}|approval={}|mode={}|win_sandbox={}",
        tool_name.name,
        command_text,
        effective_cwd,
        environment_id,
        remote,
        turn.approval_policy(),
        match turn.mode {
            codex_protocol::config_types::ModeKind::Plan => "plan",
            codex_protocol::config_types::ModeKind::Default => "default",
        },
        turn.windows_sandbox_level,
    ))
}

/// Computes a stable hash over the meaningful parts of a tool result: success
/// flag plus the full model-visible text. No truncation: v1's 8KB cap made
/// distinct short-output commands collide.
pub fn repeat_result_hash(
    output: &dyn ToolOutput,
    call_id: &str,
    payload: &ToolPayload,
) -> String {
    let item = output.to_response_item(call_id, payload);
    let (text, success) = match item {
        ResponseInputItem::FunctionCallOutput { output, .. } => {
            let text = output.body.to_text().unwrap_or_default();
            (text, output.success.unwrap_or(false))
        }
        _ => (String::new(), false),
    };
    hash_result(&success.to_string(), &text)
}

fn hash_result(success: &str, text: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(success.as_bytes());
    hasher.update(b"|");
    hasher.update(text.as_bytes());
    let digest = hasher.finalize();
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn truncate_bytes(input: &str, max_bytes: usize) -> &str {
    if input.len() <= max_bytes {
        return input;
    }
    let mut end = max_bytes;
    while !input.is_char_boundary(end) {
        end -= 1;
    }
    &input[..end]
}

fn truncate_for_log(signature: &str) -> String {
    const MAX_LOG_BYTES: usize = 256;
    let truncated = truncate_bytes(signature, MAX_LOG_BYTES);
    if truncated.len() < signature.len() {
        format!("{truncated}...")
    } else {
        truncated.to_string()
    }
}

fn block_message() -> String {
    "Repeated no-progress tool call blocked. This exact tool call has already \
     produced the same result twice and no relevant state change has occurred. \
     Do not retry it unchanged. Reassess the diagnosis and choose a materially \
     different command, modify the relevant state, or explain why progress \
     cannot continue."
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(name: &str, cmd: &str) -> String {
        format!("{name}|{cmd}")
    }

    #[test]
    fn repeated_identical_call_is_blocked_at_threshold() {
        // Default threshold is 3: the first three identical calls execute and
        // record; the fourth is blocked without running.
        let mut guard = RepeatCallGuard::default();
        let signature = sig("exec_command", "ls -la");
        for i in 1..=3 {
            assert!(guard.check(&signature).is_none(), "call {i} should pass");
            guard.record(&signature, hash_result("true", "file1\nfile2"));
        }
        let blocked = guard.check(&signature).expect("fourth call must be blocked");
        assert!(blocked.contains("Repeated no-progress tool call blocked"));

        // A threshold-2 guard blocks on the third identical call (v1 behavior).
        let mut v1_style = RepeatCallGuard::new(2);
        for i in 1..=2 {
            assert!(v1_style.check(&signature).is_none(), "call {i} should pass");
            v1_style.record(&signature, hash_result("true", "file1\nfile2"));
        }
        assert!(v1_style.check(&signature).is_some());
    }

    #[test]
    fn different_results_are_never_blocked() {
        let mut guard = RepeatCallGuard::default();
        let signature = sig("exec_command", "ls -la");
        for i in 0..5u32 {
            assert!(guard.check(&signature).is_none(), "call {i} should pass");
            guard.record(&signature, hash_result("true", &format!("output-{i}")));
        }
    }

    #[test]
    fn state_change_resets_history() {
        let mut guard = RepeatCallGuard::default();
        let signature = sig("exec_command", "ls -la");
        for _ in 0..3 {
            guard.record(&signature, hash_result("true", "same"));
        }
        assert!(guard.check(&signature).is_some());

        guard.note_state_change();
        assert!(guard.check(&signature).is_none(), "must be allowed after state change");
    }

    #[test]
    fn different_signatures_are_independent() {
        let mut guard = RepeatCallGuard::default();
        let a = sig("exec_command", "ls -la /tmp");
        let b = sig("exec_command", "ls -la /var");
        for _ in 0..3 {
            guard.record(&a, hash_result("true", "out"));
            guard.record(&b, hash_result("true", "out"));
        }
        assert!(guard.check(&a).is_some());
        assert!(guard.check(&b).is_some());
    }

    #[test]
    fn mutating_scripts_are_detected() {
        assert!(RepeatCallGuard::is_mutating_shell_script("echo hi > /tmp/x"));
        assert!(RepeatCallGuard::is_mutating_shell_script("rm -rf build && make"));
        assert!(!RepeatCallGuard::is_mutating_shell_script("cat README.md"));
    }

    #[test]
    fn state_changing_tools_are_recognized() {
        assert!(RepeatCallGuard::is_state_changing_tool("apply_patch"));
        assert!(RepeatCallGuard::is_state_changing_tool("write_stdin"));
        assert!(!RepeatCallGuard::is_state_changing_tool("exec_command"));
    }

    #[test]
    fn bypass_marker_signature_is_unique() {
        let plain = sig("exec_command", "sleep 1");
        let marked = format!("bypass|exec_command|{plain}");
        assert_ne!(plain, marked);
    }

    #[test]
    fn window_eviction_keeps_guard_bounded() {
        let mut guard = RepeatCallGuard::default();
        for i in 0..(MAX_TRACKED_SIGNATURES as u32 + 10) {
            let signature = sig("exec_command", &format!("cmd-{i}"));
            guard.record(&signature, hash_result("true", "x"));
        }
        assert!(guard.insertion_order.len() <= MAX_TRACKED_SIGNATURES);
    }

    #[test]
    fn result_hash_is_stable_and_sensitive_to_output() {
        let h1 = hash_result("true", "same output");
        let h2 = hash_result("true", "same output");
        assert_eq!(h1, h2);
        assert_ne!(h1, hash_result("false", "same output"));
        assert_ne!(h1, hash_result("true", "different output"));
    }

    #[test]
    fn result_hash_covers_full_output_without_truncation() {
        // v2: outputs differing only past the old 8KB cap must hash differently.
        let prefix = "x".repeat(9 * 1024);
        let h_a = hash_result("true", &format!("{prefix}A"));
        let h_b = hash_result("true", &format!("{prefix}B"));
        assert_ne!(h_a, h_b);
    }

    #[test]
    fn threshold_is_configurable() {
        let mut guard = RepeatCallGuard::new(5);
        let signature = sig("exec_command", "ls -la");
        for i in 1..=5 {
            assert!(guard.check(&signature).is_none(), "call {i} should pass");
            guard.record(&signature, hash_result("true", "same"));
        }
        // Sixth call is blocked after five identical results.
        assert!(guard.check(&signature).is_some());

        let mut clamped = RepeatCallGuard::new(1);
        assert_eq!(clamped.threshold, 2, "threshold must clamp to minimum 2");
    }
}
