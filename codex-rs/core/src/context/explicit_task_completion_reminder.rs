use codex_protocol::models::ContentItemKind;

use super::ContextualUserFragment;

pub(crate) struct ExplicitTaskCompletionReminder;

impl ContextualUserFragment for ExplicitTaskCompletionReminder {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("task_completion.required".to_string())
    }

    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<task_completion_required>", "</task_completion_required>")
    }

    fn body(&self) -> String {
        "This turn has not declared a terminal disposition. Continue the work if it is unfinished. If the user's request is fully satisfied, call `complete_task` with status `completed` and the final user-facing message. If progress is impossible without user action or information, call it with status `blocked` and state exactly what is needed. Do not end the response without that control call."
            .to_string()
    }
}
