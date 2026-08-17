//! Live proof-of-life for an in-flight sampling request.
//!
//! A turn against a local model can sit for many minutes with the status line
//! reading only `Working (14m 11s)`. That number says the turn has not finished;
//! it says nothing about whether the model is thinking, whether the runtime is
//! wedged, or whether anything is coming back at all -- which is exactly what
//! the user needs in order to decide whether to interrupt.
//!
//! The stream is not actually quiet. Reasoning deltas arrive throughout, but
//! `ReasoningTextDelta` is dropped unless `show_raw_agent_reasoning` is on, so a
//! model that spends its whole run inside a `<think>` block produces a
//! completely silent UI. This records those deltas -- whatever the display
//! setting -- and reports motion on the status line without putting raw
//! reasoning into the transcript.

use std::time::Instant;

use super::ChatWidget;
use crate::status_indicator_widget::STATUS_DETAILS_DEFAULT_MAX_LINES;
use crate::status_indicator_widget::StatusDetailsCapitalization;

/// Characters of streamed output kept for the status line.
const TURN_TAIL_CHARS: usize = 120;

/// Rough characters-per-token ratio. This drives a progress readout, not
/// accounting, so a cheap approximation beats pulling a tokenizer into the TUI.
const TURN_CHARS_PER_TOKEN: usize = 4;

/// Accumulated progress for the in-flight sampling request.
pub(super) struct TurnProgress {
    approx_chars: usize,
    tail: String,
    last_delta_at: Option<Instant>,
}

impl TurnProgress {
    pub(super) fn new() -> Self {
        Self {
            approx_chars: 0,
            tail: String::new(),
            last_delta_at: None,
        }
    }

    fn record(&mut self, delta: &str, now: Instant) {
        self.approx_chars = self.approx_chars.saturating_add(delta.chars().count());
        self.last_delta_at = Some(now);
        self.tail.push_str(&delta.replace('\n', " "));
        // Keep the buffer bounded; a reasoning block can run to many thousands
        // of tokens.
        let excess = self.tail.chars().count().saturating_sub(TURN_TAIL_CHARS);
        if excess > 0 {
            self.tail = self.tail.chars().skip(excess).collect();
        }
    }

    /// The status-line text, or `None` before the first delta.
    ///
    /// Silence before the first token is its own diagnosis -- on a local model
    /// that gap is prompt processing, which can run for minutes on a long
    /// context -- so it gets a distinct message rather than a zero count.
    fn status_details(&self) -> String {
        if self.last_delta_at.is_none() {
            return "waiting for the model's first token".to_string();
        }
        let approx_tokens = self.approx_chars / TURN_CHARS_PER_TOKEN;
        let tail = self.tail.trim();
        if tail.is_empty() {
            format!("~{approx_tokens} tokens generated")
        } else {
            format!("~{approx_tokens} tokens · {tail}")
        }
    }
}

impl ChatWidget {
    /// Starts tracking progress for a new turn.
    pub(super) fn on_turn_progress_start(&mut self) {
        self.turn_progress = Some(TurnProgress::new());
    }

    /// Stops tracking; the status line reverts to whatever else owns it.
    pub(super) fn on_turn_progress_end(&mut self) {
        self.turn_progress = None;
    }

    /// Records a chunk of the model stream and refreshes the status line.
    ///
    /// Called for reasoning and answer deltas alike: a model that thinks for
    /// ten minutes before writing a word would otherwise show no motion at all.
    pub(super) fn on_turn_progress_delta(&mut self, delta: &str) {
        // Compaction runs its own progress readout over the same channels and
        // owns the status line while it is active.
        if self.compaction_progress.is_some() {
            return;
        }
        let Some(progress) = self.turn_progress.as_mut() else {
            return;
        };
        progress.record(delta, Instant::now());
        self.render_turn_progress();
    }

    /// Writes current progress to the status line, leaving the header alone so
    /// whatever owns it (`Working`, a retry notice, MCP startup) still shows.
    pub(super) fn render_turn_progress(&mut self) {
        if self.compaction_progress.is_some() {
            return;
        }
        let Some(details) = self
            .turn_progress
            .as_ref()
            .map(TurnProgress::status_details)
        else {
            return;
        };
        let header = self.status_state.current_status.header.clone();
        self.set_status(
            header,
            Some(details),
            StatusDetailsCapitalization::Preserve,
            STATUS_DETAILS_DEFAULT_MAX_LINES,
        );
    }
}
