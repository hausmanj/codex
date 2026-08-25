use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::ResponseItem;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_output_truncation::truncate_text;

use super::ContextualUserFragment;

const MAX_CURSOR_TOKENS: usize = 6_000;
const MAX_CURSOR_ITEMS: usize = 12;
const MAX_ASSISTANT_ITEM_TOKENS: usize = 1_200;
const MAX_TOOL_CALL_ITEM_TOKENS: usize = 1_600;
const MAX_TOOL_RESULT_ITEM_TOKENS: usize = 2_000;

/// A deterministic, bounded record of the execution boundary immediately before compaction.
///
/// Model-written summaries are useful for broad task context but are not authoritative lifecycle
/// state. This fragment retains the latest concrete actions and results so compaction cannot turn
/// an in-progress tool loop into a fresh task-planning boundary.
pub(crate) struct CompactionExecutionCursor {
    entries: Vec<String>,
}

impl CompactionExecutionCursor {
    pub(crate) fn from_history<'a>(
        items: impl DoubleEndedIterator<Item = &'a ResponseItem>,
    ) -> Option<Self> {
        let mut entries = Vec::new();
        let mut remaining_tokens = MAX_CURSOR_TOKENS;

        // Select newest entries first so a large command cannot displace the result that followed
        // it. Reverse only after the bounded set is complete to restore chronological order.
        for item in items.rev() {
            if entries.len() == MAX_CURSOR_ITEMS || remaining_tokens == 0 {
                break;
            }
            let Some((kind, content, item_limit)) = cursor_entry(item) else {
                continue;
            };
            let entry = format!("Event ({kind}):\n{content}");
            let entry = truncate_text(
                &entry,
                TruncationPolicy::Tokens(item_limit.min(remaining_tokens)),
            );
            if entry.is_empty() {
                continue;
            }
            remaining_tokens = remaining_tokens.saturating_sub(approx_token_count(&entry));
            entries.push(entry);
        }

        if entries.is_empty() {
            return None;
        }
        entries.reverse();
        Some(Self { entries })
    }
}

impl ContextualUserFragment for CompactionExecutionCursor {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("compaction.execution_cursor".to_string())
    }

    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        (
            "<compaction_execution_cursor>",
            "</compaction_execution_cursor>",
        )
    }

    fn body(&self) -> String {
        format!(
            "\nThis is the authoritative execution tail from immediately before compaction. \
             Continue from the latest event; do not treat compaction as a task restart.\n\n{}\n",
            self.entries.join("\n\n")
        )
    }
}

fn cursor_entry(item: &ResponseItem) -> Option<(&'static str, String, usize)> {
    match item {
        ResponseItem::Message { role, content, .. } if role == "assistant" => {
            assistant_text(content)
                .map(|text| ("assistant_message", text, MAX_ASSISTANT_ITEM_TOKENS))
        }
        ResponseItem::AgentMessage { .. } => {
            serialized_entry("agent_message", item, MAX_ASSISTANT_ITEM_TOKENS)
        }
        ResponseItem::LocalShellCall { .. }
        | ResponseItem::FunctionCall { .. }
        | ResponseItem::ToolSearchCall { .. }
        | ResponseItem::CustomToolCall { .. }
        | ResponseItem::WebSearchCall { .. }
        | ResponseItem::ImageGenerationCall { .. } => {
            serialized_entry("tool_call", item, MAX_TOOL_CALL_ITEM_TOKENS)
        }
        ResponseItem::FunctionCallOutput { .. }
        | ResponseItem::CustomToolCallOutput { .. }
        | ResponseItem::ToolSearchOutput { .. } => {
            serialized_entry("tool_result", item, MAX_TOOL_RESULT_ITEM_TOKENS)
        }
        _ => None,
    }
}

// TODO(must-fix next time this file is touched): this serializes the raw `ResponseItem`,
// which includes `internal_chat_message_metadata_passthrough` (turn_id, create_time, ...).
// That leaks internal, non-deterministic bookkeeping straight into a model-visible prompt
// as noise, and it's what made two `compact.rs` tests unfixable via exact string match
// (2026-08-25: manual_compact_twice_preserves_latest_user_messages and
// multiple_auto_compact_per_task_runs_after_token_limit_hit both had to switch to
// prefix/strip-based comparisons against `strip_execution_cursor_tail()` instead of
// pinning an exact string, because turn_id/create_time differ run to run — verified by
// running the same test twice and diffing). Strip internal metadata (e.g. via
// `ResponseItem::clear_internal_chat_message_metadata_passthrough()`, already used
// elsewhere for this exact purpose) before serializing here, then those tests can go
// back to exact-match assertions.
fn serialized_entry(
    kind: &'static str,
    item: &ResponseItem,
    token_limit: usize,
) -> Option<(&'static str, String, usize)> {
    serde_json::to_string(item)
        .ok()
        .map(|content| (kind, content, token_limit))
}

fn assistant_text(content: &[ContentItem]) -> Option<String> {
    let text = content
        .iter()
        .filter_map(|item| match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text }
                if !text.is_empty() =>
            {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::models::FunctionCallOutputPayload;

    #[test]
    fn cursor_preserves_large_call_and_following_failure_result() {
        let items = [
            ResponseItem::Message {
                id: None,
                role: "assistant".to_string(),
                content: vec![ContentItem::OutputText {
                    text: "Writing the sources now.".to_string(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::FunctionCall {
                id: None,
                name: "exec_command".to_string(),
                namespace: None,
                arguments: format!(r#"{{"cmd":"{}"}}"#, "x".repeat(30_000)),
                encrypted_function_args: None,
                call_id: "write-call".to_string(),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::FunctionCallOutput {
                id: None,
                call_id: Some("write-call".to_string()),
                name: Some("exec_command".to_string()),
                namespace: None,
                output: FunctionCallOutputPayload::from_text("zsh:53: unmatched '\n".to_string()),
                internal_chat_message_metadata_passthrough: None,
            },
        ];

        let cursor = CompactionExecutionCursor::from_history(items.iter())
            .expect("execution cursor should be created")
            .render();

        assert!(cursor.contains("Writing the sources now."));
        assert!(cursor.contains("write-call"));
        assert!(cursor.contains("zsh:53: unmatched '"));
        assert!(approx_token_count(&cursor) < MAX_CURSOR_TOKENS + 200);
        assert!(
            cursor.find("write-call").expect("call should be present")
                < cursor
                    .find("zsh:53: unmatched '")
                    .expect("result should be present")
        );
    }
}
