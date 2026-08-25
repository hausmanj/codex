//! Non-command tool lifecycle rendering for `ChatWidget`.
//!
//! This module handles patch, MCP, web search, image, and collaborator tool
//! events as transcript cells.

use super::*;
use codex_utils_path_uri::LegacyAppPathString;

impl ChatWidget {
    pub(super) fn on_patch_apply_begin(&mut self, changes: HashMap<PathBuf, FileChange>) {
        self.add_to_history(history_cell::new_patch_event(changes, &self.config.cwd));
    }

    pub(super) fn on_view_image_tool_call(&mut self, path: LegacyAppPathString) {
        self.flush_answer_stream_with_separator();
        self.add_to_history(history_cell::new_view_image_tool_call(
            path,
            &self.config.cwd,
        ));
        self.request_redraw();
    }

    pub(super) fn on_image_generation_begin(&mut self) {
        self.flush_answer_stream_with_separator();
        if self.bottom_pane.is_task_running() {
            self.bottom_pane.ensure_status_indicator();
        }
    }

    /// Announces that compaction has started.
    ///
    /// Compaction never writes to the transcript while it runs, so without this the UI is
    /// completely silent from the moment compaction begins until the replacement history lands.
    /// On a local model summarizing a full context window that gap can run for minutes, which
    /// reads as a hang and invites the user to interrupt a turn that is in fact making progress.
    pub(super) fn on_context_compaction_begin(&mut self) {
        self.flush_answer_stream_with_separator();
        self.add_to_history(history_cell::new_info_event(
            "Compacting context".to_string(),
            Some("summarizing the thread to free up context; this can take a while".to_string()),
        ));
        self.compaction_progress = Some(CompactionProgress::default());
        if self.bottom_pane.is_task_running() {
            self.bottom_pane.ensure_status_indicator();
        }
        self.render_compaction_progress();
        self.request_redraw();
    }

    /// Records measured compaction telemetry and refreshes the status line.
    pub(super) fn on_context_compaction_progress(
        &mut self,
        notification: codex_app_server_protocol::ContextCompactionProgressNotification,
    ) {
        let Some(progress) = self.compaction_progress.as_mut() else {
            return;
        };
        progress.update(notification);
        self.render_compaction_progress();
    }

    /// Clears compaction progress once the replacement history has landed.
    pub(super) fn on_context_compaction_end(&mut self) {
        self.compaction_progress = None;
    }

    fn render_compaction_progress(&mut self) {
        let details = self
            .compaction_progress
            .as_ref()
            .map(CompactionProgress::status_details);
        self.set_status(
            String::from("Compacting context"),
            details,
            StatusDetailsCapitalization::Preserve,
            STATUS_DETAILS_DEFAULT_MAX_LINES,
        );
    }

    pub(super) fn on_image_generation_end(
        &mut self,
        call_id: String,
        status: String,
        revised_prompt: Option<String>,
        saved_path: Option<AbsolutePathBuf>,
    ) {
        self.flush_answer_stream_with_separator();
        self.add_to_history(history_cell::new_image_generation_call(
            call_id,
            &status,
            revised_prompt,
            saved_path,
        ));
        self.request_redraw();
    }

    pub(super) fn on_file_change_completed(&mut self, item: ThreadItem) {
        self.defer_or_handle(
            item,
            InterruptManager::push_item_completed,
            Self::handle_file_change_completed_now,
        );
    }

    pub(super) fn on_mcp_tool_call_started(&mut self, item: ThreadItem) {
        self.defer_or_handle(
            item,
            InterruptManager::push_item_started,
            Self::handle_mcp_tool_call_started_now,
        );
    }

    pub(super) fn on_mcp_tool_call_completed(&mut self, item: ThreadItem) {
        self.defer_or_handle(
            item,
            InterruptManager::push_item_completed,
            Self::handle_mcp_tool_call_completed_now,
        );
    }

    pub(super) fn on_web_search_begin(&mut self, call_id: String) {
        self.flush_answer_stream_with_separator();
        self.flush_active_cell();
        self.transcript.active_cell = Some(Box::new(history_cell::new_active_web_search_call(
            call_id,
            String::new(),
            self.config.animations,
        )));
        self.bump_active_cell_revision();
        self.request_redraw();
    }

    pub(super) fn on_web_search_end(
        &mut self,
        call_id: String,
        query: String,
        action: codex_app_server_protocol::WebSearchAction,
    ) {
        self.flush_answer_stream_with_separator();
        let mut handled = false;
        if let Some(cell) = self
            .transcript
            .active_cell
            .as_mut()
            .and_then(|cell| cell.as_any_mut().downcast_mut::<WebSearchCell>())
            && cell.call_id() == call_id
        {
            cell.update(action.clone(), query.clone());
            cell.complete();
            self.bump_active_cell_revision();
            self.flush_active_cell();
            handled = true;
        }

        if !handled {
            self.add_to_history(history_cell::new_web_search_call(call_id, query, action));
        }
        self.transcript.had_work_activity = true;
    }

    pub(super) fn on_collab_event(&mut self, cell: PlainHistoryCell) {
        self.flush_answer_stream_with_separator();
        self.add_to_history(cell);
        self.request_redraw();
    }

    pub(super) fn on_collab_agent_tool_call(&mut self, item: ThreadItem) {
        let ThreadItem::CollabAgentToolCall {
            id, tool, status, ..
        } = &item
        else {
            return;
        };
        if matches!(tool, CollabAgentTool::SpawnAgent)
            && let Some(spawn_request) = multi_agents::spawn_request_summary(&item)
        {
            self.pending_collab_spawn_requests
                .insert(id.clone(), spawn_request);
        }

        let cached_spawn_request = if matches!(tool, CollabAgentTool::SpawnAgent)
            && !matches!(status, CollabAgentToolCallStatus::InProgress)
        {
            self.pending_collab_spawn_requests.remove(id)
        } else {
            None
        };

        if let Some(cell) = multi_agents::tool_call_history_cell(
            &item,
            cached_spawn_request.as_ref(),
            |thread_id| self.collab_agent_metadata(thread_id),
        ) {
            self.on_collab_event(cell);
        }
    }

    pub(super) fn on_sub_agent_activity(&mut self, item: ThreadItem) {
        if let Some(cell) = multi_agents::sub_agent_activity_history_cell(&item) {
            self.on_collab_event(cell);
        }
    }

    pub(crate) fn handle_file_change_completed_now(&mut self, item: ThreadItem) {
        let ThreadItem::FileChange { status, .. } = item else {
            return;
        };
        // If the patch was successful, just let the "Edited" block stand.
        // Otherwise, add a failure block.
        if matches!(status, codex_app_server_protocol::PatchApplyStatus::Failed) {
            self.add_to_history(history_cell::new_patch_apply_failure(String::new()));
        }
        // Mark that actual work was done (patch applied)
        self.transcript.had_work_activity = true;
    }

    pub(crate) fn handle_mcp_tool_call_started_now(&mut self, item: ThreadItem) {
        let ThreadItem::McpToolCall {
            id,
            server,
            tool,
            arguments,
            ..
        } = item
        else {
            return;
        };
        self.flush_answer_stream_with_separator();
        self.flush_active_cell();
        self.transcript.active_cell = Some(Box::new(history_cell::new_active_mcp_tool_call(
            id,
            McpInvocation {
                server,
                tool,
                arguments: Some(arguments),
            },
            self.config.animations,
        )));
        self.bump_active_cell_revision();
        self.request_redraw();
    }

    pub(crate) fn handle_mcp_tool_call_completed_now(&mut self, item: ThreadItem) {
        self.flush_answer_stream_with_separator();

        let ThreadItem::McpToolCall {
            id,
            server,
            tool,
            arguments,
            result,
            error,
            duration_ms,
            ..
        } = item
        else {
            return;
        };
        let invocation = McpInvocation {
            server,
            tool,
            arguments: Some(arguments),
        };
        let duration = Duration::from_millis(duration_ms.unwrap_or_default().max(0) as u64);
        let result = match (result, error) {
            (_, Some(error)) => Err(error.message),
            (Some(result), None) => {
                let result = *result;
                Ok(codex_protocol::mcp::CallToolResult {
                    content: result.content,
                    structured_content: result.structured_content,
                    is_error: Some(false),
                    meta: None,
                })
            }
            (None, None) => Err("MCP tool call completed without a result".to_string()),
        };

        let extra_cell = match self
            .transcript
            .active_cell
            .as_mut()
            .and_then(|cell| cell.as_any_mut().downcast_mut::<McpToolCallCell>())
        {
            Some(cell) if cell.call_id() == id => cell.complete(duration, result),
            _ => {
                self.flush_active_cell();
                let mut cell =
                    history_cell::new_active_mcp_tool_call(id, invocation, self.config.animations);
                let extra_cell = cell.complete(duration, result);
                self.transcript.active_cell = Some(Box::new(cell));
                extra_cell
            }
        };

        self.flush_active_cell();
        if let Some(extra) = extra_cell {
            self.add_boxed_history(extra);
        }
        // Mark that actual work was done (MCP tool call)
        self.transcript.had_work_activity = true;
    }

    pub(crate) fn handle_queued_item_started_now(&mut self, item: ThreadItem) {
        match item {
            item @ ThreadItem::CommandExecution { .. } => {
                self.handle_command_execution_started_now(item);
            }
            item @ ThreadItem::McpToolCall { .. } => {
                self.handle_mcp_tool_call_started_now(item);
            }
            _ => {}
        }
    }

    pub(crate) fn handle_queued_item_completed_now(&mut self, item: ThreadItem) {
        match item {
            item @ ThreadItem::CommandExecution { .. } => {
                self.handle_command_execution_completed_now(item);
            }
            item @ ThreadItem::FileChange { .. } => self.handle_file_change_completed_now(item),
            item @ ThreadItem::McpToolCall { .. } => self.handle_mcp_tool_call_completed_now(item),
            _ => {}
        }
    }
}

/// Measured progress for an in-flight compaction.
#[derive(Default)]
pub(super) struct CompactionProgress {
    phase: Option<codex_app_server_protocol::ContextCompactionProgressPhase>,
    attempt: u32,
    output_bytes: u64,
    output_chunks: u64,
    output_tokens: Option<i64>,
}

const COMPACTION_METER_CELLS: usize = 20;
const COMPACTION_BYTES_PER_CELL: u64 = 1024;
const COMPACTION_BYTES_PER_PARTIAL_CELL: u64 = COMPACTION_BYTES_PER_CELL / 8;
const PARTIAL_BLOCKS: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];

impl CompactionProgress {
    fn update(
        &mut self,
        notification: codex_app_server_protocol::ContextCompactionProgressNotification,
    ) {
        self.phase = Some(notification.phase);
        self.attempt = notification.attempt;
        self.output_bytes = notification.output_bytes;
        self.output_chunks = notification.output_chunks;
        self.output_tokens = notification.output_tokens;
    }

    fn status_details(&self) -> String {
        let meter = self.output_meter();
        match self.phase {
            None => format!("{meter} waiting for first model output · 1 block = 1 KiB"),
            Some(codex_app_server_protocol::ContextCompactionProgressPhase::Generating) => {
                format!(
                    "{meter} {} bytes received · {} chunks · attempt {} · 1 block = 1 KiB",
                    self.output_bytes, self.output_chunks, self.attempt
                )
            }
            Some(codex_app_server_protocol::ContextCompactionProgressPhase::Finalizing) => {
                let output = self.output_tokens.map_or_else(
                    || "model response complete".to_string(),
                    |tokens| format!("{tokens} output tokens"),
                );
                format!(
                    "[{}] {output} · installing compacted context",
                    "█".repeat(COMPACTION_METER_CELLS)
                )
            }
        }
    }

    fn output_meter(&self) -> String {
        let full_cells = usize::try_from(self.output_bytes / COMPACTION_BYTES_PER_CELL)
            .unwrap_or(usize::MAX)
            .min(COMPACTION_METER_CELLS);
        let mut cells = "█".repeat(full_cells);
        if full_cells < COMPACTION_METER_CELLS {
            let partial_index = usize::try_from(
                (self.output_bytes % COMPACTION_BYTES_PER_CELL) / COMPACTION_BYTES_PER_PARTIAL_CELL,
            )
            .unwrap_or(0);
            if partial_index > 0 {
                cells.push(PARTIAL_BLOCKS[partial_index]);
            }
            cells.push_str(&"·".repeat(COMPACTION_METER_CELLS - cells.chars().count()));
        }
        format!("[{cells}]")
    }
}
