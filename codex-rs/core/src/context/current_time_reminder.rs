use chrono::DateTime;
use chrono::Utc;
use codex_features::CurrentTimeReminderDeliveryMode;

use super::ContextualUserFragment;

pub(crate) struct CurrentTimeReminder {
    current_time: DateTime<Utc>,
    delivery_mode: CurrentTimeReminderDeliveryMode,
}

impl CurrentTimeReminder {
    pub(crate) fn new(
        current_time: DateTime<Utc>,
        delivery_mode: CurrentTimeReminderDeliveryMode,
    ) -> Self {
        Self {
            current_time,
            delivery_mode,
        }
    }

    pub(crate) fn formatted_time(&self) -> String {
        self.current_time
            .format("%Y-%m-%d %H:%M:%S UTC")
            .to_string()
    }
}

impl ContextualUserFragment for CurrentTimeReminder {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<current_time_reminder>", "</current_time_reminder>")
    }

    fn body(&self) -> String {
        match self.delivery_mode {
            CurrentTimeReminderDeliveryMode::CheckpointMemory => format!(
                concat!(
                    "It is {}. Before continuing the current task, update the local Codex/Qwen ",
                    "session checkpoint file at exactly `.qwen/codex/SESSION_MEMORY.md`. ",
                    "Do not use `.codex/SESSION_MEMORY.md`. The file must contain a `Rolling Summary` ",
                    "followed by at most the newest 10 detailed checkpoints. Append one new checkpoint ",
                    "of approximately 150-300 tokens recording: timestamp, current objective, progress ",
                    "since the previous checkpoint, files touched, commands/tests run, decisions, blockers, ",
                    "and the exact next action. If appending would create checkpoint 11, first merge any ",
                    "still-useful information from the oldest detailed checkpoint into `Rolling Summary`, ",
                    "then delete that oldest detailed checkpoint so exactly the newest 10 remain. Preserve ",
                    "existing useful summary information. After the file update, immediately resume the exact ",
                    "task that was in progress; do not stop merely because the checkpoint was written."
                ),
                self.formatted_time()
            ),
            CurrentTimeReminderDeliveryMode::AnyInference
            | CurrentTimeReminderDeliveryMode::AfterUserOrToolOutput => {
                format!("It is {}.", self.formatted_time())
            }
        }
    }
}
