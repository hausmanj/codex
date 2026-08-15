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
        ("", "")
    }

    fn body(&self) -> String {
        match self.delivery_mode {
            CurrentTimeReminderDeliveryMode::CheckpointMemory => format!(
                concat!(
                    "It is {}. Maintain two distinct memory layers for the active project.\n\n",
                    "DURABLE PROJECT STATE:\n",
                    "- Read the canonical durable project-state file declared by the active project's instructions ",
                    "(for example AGENTS.md) when it is needed for task context.\n",
                    "- Do NOT write routine rolling checkpoints into that file.\n",
                    "- Update durable project state only when the project's own instructions require it, such as a ",
                    "completed batch, durable decision, established fact, changed project state, or session-completion milestone.\n\n",
                    "QWEN ROLLING CHECKPOINT:\n",
                    "- Write the frequent execution checkpoint only to Qwen's project-scoped checkpoint file.\n",
                    "- Determine the active project root from the current project/repository context.\n",
                    "- Encode its absolute path by replacing each `/` with `-`, preserving the leading separator as ",
                    "the leading `-`. Example: `/Users/example/project/app` becomes ",
                    "`-Users-example-project-app`.\n",
                    "- The checkpoint path is exactly ",
                    "`$HOME/.qwen/projects/<encoded-project-root>/codex/SESSION_MEMORY.md`.\n",
                    "- Create the parent directory if necessary.\n",
                    "- Never create a repo-local `.qwen/codex/SESSION_MEMORY.md` and never use ",
                    "`$HOME/.qwen/codex/SESSION_MEMORY.md` as a project checkpoint.\n\n",
                    "The Qwen checkpoint is transient execution state, not project documentation. Keep it compact. ",
                    "It must preserve the exact information needed to continue after context compaction or process restart: ",
                    "current objective; exact current work item/cursor; last completed item; exact next action; blockers; ",
                    "important in-flight decisions; and newly discovered execution facts that have not yet been promoted ",
                    "to durable project state.\n\n",
                    "For iterative work, the checkpoint must advance monotonically. Record both the last completed item ",
                    "and the next unresolved item so completed work is not re-reviewed. Never write `Await further instructions`, ",
                    "`wait for instructions`, or equivalent unless the user explicitly asked you to stop or genuine user ",
                    "input is required. If there is no blocker, record `Blockers: None`.\n\n",
                    "On a fresh session or after compaction, use the active project's instructions and durable project state ",
                    "for stable context, then use this Qwen checkpoint for the precise execution cursor. Do not rediscover ",
                    "mounts, project locations, inventories, routes, commands, schemas, or other facts already recorded in ",
                    "those sources unless the recorded resource actually fails or fresh verification is required.\n\n",
                    "After writing the Qwen checkpoint, immediately continue from its exact next action. Do not stop merely ",
                    "because the checkpoint was written."
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
