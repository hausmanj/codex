use std::collections::BTreeMap;
use std::sync::Arc;

use codex_protocol::ResponseItemId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ResponseItem;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde_json::json;

use crate::function_tool::FunctionCallError;
use crate::state::ExplicitTaskCompletionStatus;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;

const TOOL_NAME: &str = "complete_task";

pub struct ExplicitTaskCompletionHandler;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum CompletionStatus {
    Completed,
    Blocked,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompletionArgs {
    status: CompletionStatus,
    final_message: String,
}

impl ToolExecutor<ToolInvocation> for ExplicitTaskCompletionHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        let properties = BTreeMap::from([
            (
                "status".to_string(),
                JsonSchema::string_enum(
                    vec![json!("completed"), json!("blocked")],
                    Some(
                        "Use completed only when the user's request is fully satisfied; use blocked only when user action or information is required."
                            .to_string(),
                    ),
                ),
            ),
            (
                "final_message".to_string(),
                JsonSchema::string(Some(
                    "The complete user-facing final answer, or the exact blocking question."
                        .to_string(),
                )),
            ),
        ]);

        ToolSpec::Function(ResponsesApiTool {
            name: TOOL_NAME.to_string(),
            description: "Declare that the current task is genuinely finished or blocked. This is the only valid way to end a turn when this tool is available. Call it alone, after all other tool calls and verification are complete. Its final_message is shown directly to the user."
                .to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                properties,
                Some(vec!["status".to_string(), "final_message".to_string()]),
                Some(false.into()),
            ),
            output_schema: None,
        })
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolInvocation {
                session,
                turn,
                payload,
                ..
            } = invocation;
            let ToolPayload::Function { arguments } = payload else {
                return Err(FunctionCallError::RespondToModel(format!(
                    "{TOOL_NAME} handler received unsupported payload"
                )));
            };
            let args: CompletionArgs = parse_arguments(&arguments)?;
            let final_message = args.final_message.trim();
            if final_message.is_empty() {
                return Err(FunctionCallError::RespondToModel(
                    "final_message must not be empty".to_string(),
                ));
            }
            if args.status == CompletionStatus::Completed
                && session
                    .services
                    .plan_progress
                    .lock()
                    .await
                    .has_unfinished_work()
            {
                return Err(FunctionCallError::RespondToModel(
                    "The current update_plan still has unfinished steps. Complete the work and update the plan before declaring the task completed, or use blocked if user action is genuinely required."
                        .to_string(),
                ));
            }

            let final_message = final_message.to_string();
            let status = match args.status {
                CompletionStatus::Completed => ExplicitTaskCompletionStatus::Completed,
                CompletionStatus::Blocked => ExplicitTaskCompletionStatus::Blocked,
            };
            {
                let turn_state = {
                    let active_turn = session.active_turn.lock().await;
                    let Some(active_turn) = active_turn.as_ref() else {
                        return Err(FunctionCallError::Fatal(
                            "active turn disappeared while recording task completion".to_string(),
                        ));
                    };
                    Arc::clone(&active_turn.turn_state)
                };
                turn_state
                    .lock()
                    .await
                    .record_explicit_task_completion(status, final_message.clone());
            }

            let response_item = ResponseItem::Message {
                id: Some(ResponseItemId::new("msg")),
                role: "assistant".to_string(),
                content: vec![ContentItem::OutputText {
                    text: final_message.clone(),
                }],
                phase: Some(MessagePhase::FinalAnswer),
                internal_chat_message_metadata_passthrough: None,
            };
            session
                .record_response_item_and_emit_turn_item(turn.as_ref(), response_item)
                .await;

            Ok(boxed_tool_output(FunctionToolOutput::from_text(
                r#"{"accepted":true}"#.to_string(),
                /*success*/ Some(true),
            )))
        })
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        false
    }
}

impl CoreToolRuntime for ExplicitTaskCompletionHandler {
    fn is_builtin_control_tool(&self) -> bool {
        true
    }
}
