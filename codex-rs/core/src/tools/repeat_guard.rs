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
    /// Set when `check` blocks a call; cleared the moment any further tool
    /// call executes (`record`) or genuine progress is observed
    /// (`note_state_change`). If this is still set when the turn ends with no
    /// further tool call at all, the model hit the guard and gave up without
    /// trying anything else — see `take_stall_for_auto_nudge`.
    blocked_without_followup: bool,
    /// Set by `mark_no_progress_stall` when a turn is cut for producing no
    /// completed item within the configured timeout. Independent of
    /// `blocked_without_followup` -- this stall never touches the repeat-call
    /// tracking at all, since it fires when the model never even reached a
    /// tool call. See `take_stall_for_auto_nudge`.
    no_progress_stall: bool,
    /// Consecutive auto-nudges issued for this unresolved stall streak. Reset
    /// to 0 on genuine progress (`note_state_change`), never by a mere retry.
    /// Shared across both stall kinds -- either one counts toward the same
    /// cap, since both mean the model isn't converging.
    consecutive_nudges: u32,
}

/// Which condition produced the stall `take_stall_for_auto_nudge` reports.
/// Selects the auto-nudge message text: the two failure modes look nothing
/// alike from the model's side (one is a tool call it can see was blocked,
/// the other is a reasoning turn that never reached a tool call at all), so
/// telling the model which one happened is what makes the nudge actionable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StallKind {
    /// The guard blocked a repeated identical tool call and nothing else
    /// followed before the turn ended.
    RepeatedToolCall,
    /// The turn was cut for producing no completed item within the
    /// configured no-progress timeout.
    NoProgress,
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
            blocked_without_followup: false,
            no_progress_stall: false,
            consecutive_nudges: 0,
        }
    }

    /// Marks the current stall streak as a no-progress timeout rather than a
    /// blocked repeated call. Called from the turn loop when it cuts a
    /// generation that produced no completed item within the configured
    /// timeout -- see `RepeatGuardConfig::no_progress_timeout_secs`.
    pub fn mark_no_progress_stall(&mut self) {
        self.no_progress_stall = true;
    }

    /// Non-consuming peek at whether a no-progress stall is pending, without
    /// touching the repeated-tool-call stall flag. Used only to let
    /// `on_task_finished` auto-nudge even when the watchdog's own abort left
    /// `idle_cause` as `Interrupted`/`Failed` rather than `Completed` --
    /// deliberately narrower than a generic "any stall pending" peek, so it
    /// cannot let a repeat-guard block ride along and auto-nudge after a
    /// genuine user interrupt, which is exactly the case the `Completed`-only
    /// gate exists to prevent for that stall kind.
    pub fn has_no_progress_stall_pending(&self) -> bool {
        self.no_progress_stall
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
            self.blocked_without_followup = true;
            Some(block_message())
        } else {
            None
        }
    }

    /// Records the outcome of an executed invocation under its signature. The
    /// stored count is how many identical results have been observed so far for
    /// this signature (the first observation stores 1).
    pub fn record(&mut self, signature: &str, result_hash: String) {
        // Any tool call that actually ran — even a read-only one, even if it
        // hits the guard again later — means the model is still trying
        // something, not sitting stuck. Only "guard blocked, then nothing
        // else happened before the turn ended" counts as a stall.
        self.blocked_without_followup = false;
        self.no_progress_stall = false;
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
    /// next identical call is treated as fresh, and resets the auto-nudge
    /// streak since genuine progress restores trust.
    pub fn note_state_change(&mut self) {
        if !self.entries.is_empty() {
            tracing::debug!("repeat guard: state change observed, clearing history");
        }
        self.entries.clear();
        self.insertion_order.clear();
        self.blocked_without_followup = false;
        self.no_progress_stall = false;
        self.consecutive_nudges = 0;
    }

    /// Called once per completing turn. Returns the stall kind exactly when:
    /// a stall (blocked repeat, or a no-progress timeout) was recorded during
    /// this turn, no further tool call followed (the model gave up rather
    /// than pivoting), and the per-streak nudge cap (`max`, 0 = disabled) has
    /// not been reached — in which case the caller should start a follow-up
    /// turn nudging the model to pivot instead of leaving the session idle.
    /// Always clears both pending-stall flags so a turn that merely repeats
    /// the same block doesn't re-trigger without a fresh one. A repeated-call
    /// block takes priority when (implausibly) both fired in the same turn,
    /// since it carries more specific information for the nudge.
    pub fn take_stall_for_auto_nudge(&mut self, max: u32) -> Option<StallKind> {
        let blocked = std::mem::take(&mut self.blocked_without_followup);
        let no_progress = std::mem::take(&mut self.no_progress_stall);
        let kind = if blocked {
            Some(StallKind::RepeatedToolCall)
        } else if no_progress {
            Some(StallKind::NoProgress)
        } else {
            None
        };
        if kind.is_none() || max == 0 || self.consecutive_nudges >= max {
            return None;
        }
        self.consecutive_nudges += 1;
        kind
    }

    /// Whether the flat tool name is one of the state-changing tools that should
    /// reset history when it succeeds.
    pub fn is_state_changing_tool(flat_name: &str) -> bool {
        matches!(
            flat_name,
            "write_stdin" | "request_permissions" | "spawn_agent" | "send_message"
        ) || flat_name.contains("patch")
    }

    /// Whether a shell script plausibly changed state, so that repeating an
    /// identical command could legitimately produce a different result.
    ///
    /// This must be decided from what the script *does*, never from whether the
    /// parser recognized it. `ParsedCommand` has no mutating variant — anything
    /// outside read/list/search becomes `Unknown` — so treating `Unknown` as
    /// mutating (as this did originally) marked ordinary read-only pipelines
    /// like `curl ... | grep ...` as state changes. Every such call then cleared
    /// the history, repeats never accumulated, and the guard could not fire for
    /// exactly the commands agents loop on.
    ///
    /// Erring toward "not mutating" is the safe direction: the cost is telling a
    /// model to stop repeating itself, versus a guard that never engages.
    pub fn is_mutating_shell_script(script: &str) -> bool {
        script_writes_output(script)
            || parse_shell_script(script)
                .iter()
                .any(|command| match command {
                    ParsedCommand::Unknown { cmd } => invokes_mutating_program(cmd),
                    ParsedCommand::Read { .. }
                    | ParsedCommand::ListFiles { .. }
                    | ParsedCommand::Search { .. } => false,
                })
    }

    /// Whether the global env-var bypass is active for this process:
    /// `CODEX_REPEAT_GUARD=0` or legacy `CODEX_ALLOW_REPEAT_TOOLS=1`.
    pub fn env_bypass_active() -> bool {
        std::env::var_os(REPEAT_GUARD_DISABLE_ENV).is_some_and(|value| value == "0")
            || std::env::var_os(LEGACY_REPEAT_GUARD_ALLOW_ENV).is_some_and(|value| value == "1")
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
                    value
                        .get("workdir")
                        .and_then(|w| w.as_str())
                        .map(str::to_string),
                ),
                "shell_command" => (
                    value.get("command")?.as_str()?.to_string(),
                    value
                        .get("workdir")
                        .and_then(|w| w.as_str())
                        .map(str::to_string),
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
        return Some(format!("bypass|{}|{}", tool_name.name, command_text));
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
pub fn repeat_result_hash(output: &dyn ToolOutput, call_id: &str, payload: &ToolPayload) -> String {
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

/// Follow-up turn injected by `take_stall_for_auto_nudge` when the model hit
/// the guard and then ended its turn without trying anything else. Shorter
/// and more directive than `block_message`: the model already saw that text
/// once and didn't act on it, so this leads with the outcome, not the
/// mechanism.
///
/// `NoProgress` gets different wording on purpose: the model has no memory of
/// what happened during the cut turn (it was mid-generation, not between
/// steps), so telling it "you hit the guard" would be describing an event it
/// never saw. Confirmed necessary 2026-08-18: a genuine 22-minute stall
/// produced zero completed items the entire time, and the model's own
/// after-the-fact account of what happened during it was fabricated (a
/// stalled generation cannot observe itself) -- so this message says plainly
/// that nothing is known about what happened, instead of inviting a guess.
pub fn auto_nudge_message(kind: StallKind) -> String {
    match kind {
        StallKind::RepeatedToolCall => {
            "You hit the repeat-tool-call guard on your last turn and stopped without \
             taking a new action. Stop retrying the same command. In one sentence, \
             state what you were trying to find or do and exactly what failed, then \
             take ONE concrete, materially different next step right now — a \
             different file, a different tool, or a direct question back to the user \
             if you are genuinely blocked."
                .to_string()
        }
        StallKind::NoProgress => {
            "Your previous turn was cut off after running for several minutes without \
             completing a single message or tool call. Nothing is known about what you \
             were doing in there — do not guess or narrate a story about it, and do not \
             cite your own prior reasoning as evidence of what happened. In one sentence, \
             state what you are trying to accomplish right now based on the conversation \
             so far, then take ONE concrete, small step toward it — a single tool call or \
             a short direct answer, not another long uninterrupted stretch of reasoning."
                .to_string()
        }
    }
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
        let blocked = guard
            .check(&signature)
            .expect("fourth call must be blocked");
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
        assert!(
            guard.check(&signature).is_none(),
            "must be allowed after state change"
        );
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
        assert!(RepeatCallGuard::is_mutating_shell_script(
            "echo hi > /tmp/x"
        ));
        assert!(RepeatCallGuard::is_mutating_shell_script(
            "rm -rf build && make"
        ));
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

        let clamped = RepeatCallGuard::new(1);
        assert_eq!(clamped.threshold, 2, "threshold must clamp to minimum 2");
    }

    /// The core stall signal: guard blocks, then the turn ends with no further
    /// tool call at all. This is what `on_task_finished` checks for.
    #[test]
    fn stall_is_flagged_when_block_is_the_last_action_in_the_turn() {
        let mut guard = RepeatCallGuard::new(2);
        let signature = sig("exec_command", "grep foo bar.dart");
        guard.record(&signature, hash_result("true", "same"));
        guard.record(&signature, hash_result("true", "same"));
        assert!(guard.check(&signature).is_some(), "third call is blocked");

        assert_eq!(
            guard.take_stall_for_auto_nudge(3),
            Some(StallKind::RepeatedToolCall),
            "block with nothing after it must be a stall"
        );
    }

    /// If the model tries anything else after the block — even a read-only,
    /// even one that hits the guard again — it is not stalled: it's working.
    #[test]
    fn any_followup_tool_call_clears_the_stall() {
        let mut guard = RepeatCallGuard::new(2);
        let signature = sig("exec_command", "grep foo bar.dart");
        guard.record(&signature, hash_result("true", "same"));
        guard.record(&signature, hash_result("true", "same"));
        assert!(guard.check(&signature).is_some());

        // Model tries something different afterward.
        let other_signature = sig("exec_command", "grep foo baz.dart");
        guard.record(&other_signature, hash_result("true", "different"));

        assert_eq!(
            guard.take_stall_for_auto_nudge(3),
            None,
            "a followup call means it is not stalled, even though it was blocked earlier"
        );
    }

    /// Genuine progress (a state-changing call) resets the nudge streak, not
    /// just the stall flag — a model that got back on track shouldn't have a
    /// stale nudge count held against it if it stalls again later.
    #[test]
    fn state_change_resets_the_nudge_streak() {
        let mut guard = RepeatCallGuard::new(2);
        let signature = sig("exec_command", "grep foo bar.dart");
        for _ in 0..3 {
            guard.record(&signature, hash_result("true", "same"));
            guard.check(&signature);
        }
        assert!(guard.take_stall_for_auto_nudge(3).is_some());
        assert_eq!(
            guard.take_stall_for_auto_nudge(3),
            None,
            "already cleared, nothing to take"
        );

        // Force another stall, consume up to the cap.
        for _ in 0..3 {
            guard.record(&signature, hash_result("true", "same"));
            guard.check(&signature);
        }
        guard.take_stall_for_auto_nudge(1); // consecutive_nudges now at cap (1)

        guard.note_state_change();

        for _ in 0..3 {
            guard.record(&signature, hash_result("true", "same"));
            guard.check(&signature);
        }
        assert!(
            guard.take_stall_for_auto_nudge(1).is_some(),
            "state change must reset the streak so a later stall can nudge again"
        );
    }

    /// Cap enforcement: once `max` nudges have fired for an unresolved streak,
    /// further stalls are reported as-is (not nudged again) until real
    /// progress resets the streak.
    #[test]
    fn nudge_cap_stops_firing_after_max_consecutive_nudges() {
        let mut guard = RepeatCallGuard::new(2);
        let signature = sig("exec_command", "grep foo bar.dart");

        for expected in [
            Some(StallKind::RepeatedToolCall),
            Some(StallKind::RepeatedToolCall),
            None,
        ] {
            for _ in 0..3 {
                guard.record(&signature, hash_result("true", "same"));
                guard.check(&signature);
            }
            assert_eq!(guard.take_stall_for_auto_nudge(2), expected);
        }
    }

    /// `max == 0` disables auto-nudging outright, even with an unresolved stall.
    #[test]
    fn zero_max_disables_auto_nudge() {
        let mut guard = RepeatCallGuard::new(2);
        let signature = sig("exec_command", "grep foo bar.dart");
        for _ in 0..3 {
            guard.record(&signature, hash_result("true", "same"));
            guard.check(&signature);
        }
        assert_eq!(guard.take_stall_for_auto_nudge(0), None);
    }

    /// A no-progress timeout must be reported as its own kind, distinct from a
    /// repeated-tool-call block, and must go through the same cap/streak
    /// machinery.
    #[test]
    fn no_progress_stall_reports_its_own_kind_and_respects_the_cap() {
        let mut guard = RepeatCallGuard::new(2);
        guard.mark_no_progress_stall();
        assert_eq!(
            guard.take_stall_for_auto_nudge(3),
            Some(StallKind::NoProgress)
        );
        // Cleared after taking; nothing left to report.
        assert_eq!(guard.take_stall_for_auto_nudge(3), None);
    }

    /// A repeated-tool-call block takes priority if (implausibly) both stall
    /// flags are set at once, since it carries more specific information.
    #[test]
    fn repeated_call_block_takes_priority_over_no_progress_when_both_are_set() {
        let mut guard = RepeatCallGuard::new(2);
        let signature = sig("exec_command", "grep foo bar.dart");
        guard.record(&signature, hash_result("true", "same"));
        guard.record(&signature, hash_result("true", "same"));
        assert!(guard.check(&signature).is_some());
        guard.mark_no_progress_stall();
        assert_eq!(
            guard.take_stall_for_auto_nudge(3),
            Some(StallKind::RepeatedToolCall)
        );
    }
}

/// Shell tokens that write somewhere: redirections and in-place stream writers.
/// `2>&1` is deliberately absent — it redirects a stream to another stream and
/// touches nothing.
fn script_writes_output(script: &str) -> bool {
    let mut chars = script.char_indices().peekable();
    while let Some((idx, ch)) = chars.next() {
        if ch != '>' {
            continue;
        }
        // `2>&1`, `>&2` and friends rewire descriptors without writing a file.
        let redirects_to_descriptor =
            script[idx + 1..].starts_with('&') || script[idx + 1..].starts_with(">&");
        if !redirects_to_descriptor {
            return true;
        }
    }
    // `tee` writes files even without a redirection operator.
    script
        .split(|c: char| c.is_whitespace() || c == '|' || c == ';' || c == '&')
        .any(|token| token == "tee")
}

/// Programs that change state on success. Read-only tools an agent commonly
/// loops on (curl, ping, nc, git status/log/diff, docker ps, python -c ...) are
/// intentionally absent so their repeats accumulate.
fn invokes_mutating_program(cmd: &str) -> bool {
    const MUTATING: &[&str] = &[
        "rm", "rmdir", "mv", "cp", "mkdir", "touch", "chmod", "chown", "ln", "truncate", "dd",
        "install", "patch", "tee", "make", "cmake", "ninja",
    ];
    // Subcommand-sensitive tools: only some verbs mutate.
    const MUTATING_SUBCOMMANDS: &[(&str, &[&str])] = &[
        (
            "git",
            &[
                "commit",
                "add",
                "rm",
                "mv",
                "checkout",
                "switch",
                "restore",
                "reset",
                "merge",
                "rebase",
                "cherry-pick",
                "revert",
                "push",
                "pull",
                "fetch",
                "clone",
                "apply",
                "stash",
                "clean",
                "tag",
                "branch",
                "init",
            ],
        ),
        (
            "npm",
            &[
                "install",
                "i",
                "ci",
                "uninstall",
                "update",
                "run",
                "publish",
            ],
        ),
        ("pnpm", &["install", "i", "add", "remove", "update", "run"]),
        ("yarn", &["install", "add", "remove", "upgrade", "run"]),
        ("pip", &["install", "uninstall"]),
        ("pip3", &["install", "uninstall"]),
        ("uv", &["pip", "add", "remove", "sync", "install"]),
        (
            "cargo",
            &["add", "remove", "install", "publish", "fix", "clean"],
        ),
        (
            "brew",
            &["install", "uninstall", "upgrade", "link", "unlink"],
        ),
        (
            "docker",
            &[
                "run", "rm", "rmi", "build", "start", "stop", "restart", "compose",
            ],
        ),
        ("kubectl", &["apply", "delete", "create", "patch", "scale"]),
        (
            "systemctl",
            &["start", "stop", "restart", "enable", "disable"],
        ),
    ];

    let mut tokens = cmd
        .split_whitespace()
        .skip_while(|token| token.contains('=') || matches!(*token, "sudo" | "env" | "command"));
    let Some(program) = tokens.next() else {
        return false;
    };
    let program = program.rsplit('/').next().unwrap_or(program);

    if MUTATING.contains(&program) {
        return true;
    }
    // `sed -i` edits in place; plain `sed` is a filter.
    if program == "sed" {
        return cmd
            .split_whitespace()
            .any(|t| t == "-i" || t.starts_with("-i."));
    }
    if let Some((_, verbs)) = MUTATING_SUBCOMMANDS
        .iter()
        .find(|(name, _)| *name == program)
    {
        let subcommand = tokens.find(|token| !token.starts_with('-'));
        return subcommand.is_some_and(|verb| verbs.contains(&verb));
    }
    false
}

#[cfg(test)]
mod mutation_classification_tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// The exact command that looped in a live session. It is read-only, so it
    /// must not count as a state change; otherwise every call clears the history
    /// and the guard can never reach its threshold.
    #[test]
    fn looping_curl_pipeline_is_not_a_state_change() {
        let script = "cd ~/source/immich && timeout 8 curl -s \
            http://127.0.0.1:2285/api/server-info/ping -H 'Accept: text/html' -v 2>&1 \
            | grep -E '^< (HTTP|Content)'";
        assert!(!RepeatCallGuard::is_mutating_shell_script(script));
    }

    #[test]
    fn read_only_probes_are_not_state_changes() {
        for script in [
            "ping -c 3 192.168.0.253",
            "nc -z -w 4 192.168.0.253 2285",
            "git status -sb",
            "git log --oneline -15",
            "git diff --stat",
            "docker ps --filter name=mobile-test",
            "cat README.md",
            "ls -la /tmp",
            "python3 -c 'print(1)'",
            "curl -s http://localhost:8080 2>&1 | head -c 300",
        ] {
            assert!(
                !RepeatCallGuard::is_mutating_shell_script(script),
                "should not be mutating: {script}"
            );
        }
    }

    #[test]
    fn genuine_mutations_are_state_changes() {
        for script in [
            "rm -rf build",
            "mkdir -p out",
            "mv a b",
            "echo hi > file.txt",
            "echo hi >> file.txt",
            "cat x | tee out.txt",
            "git commit -m 'x'",
            "git checkout main",
            "npm install",
            "pip install requests",
            "docker run --rm alpine",
            "sed -i 's/a/b/' file.txt",
            "make",
        ] {
            assert!(
                RepeatCallGuard::is_mutating_shell_script(script),
                "should be mutating: {script}"
            );
        }
    }

    /// `2>&1` rewires a descriptor; it writes nothing.
    #[test]
    fn stream_redirection_is_not_a_write() {
        assert!(!script_writes_output("curl -v http://x 2>&1 | grep a"));
        assert!(!script_writes_output("cmd >&2"));
        assert!(script_writes_output("cmd > out.txt"));
    }

    /// End-to-end: three identical read-only calls, then the fourth is blocked.
    #[test]
    fn repeated_read_only_call_now_reaches_the_threshold() {
        let mut guard = RepeatCallGuard::new(3);
        let signature = "shell_command|curl -s http://x|/tmp||remote=false|approval=never|mode=default|win_sandbox=none";
        let hash = "same-result".to_string();
        for _ in 0..3 {
            assert_eq!(guard.check(signature).is_none(), true);
            guard.record(signature, hash.clone());
        }
        assert!(
            guard.check(signature).is_some(),
            "fourth identical call must be blocked"
        );
    }
}
