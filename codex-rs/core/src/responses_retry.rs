//! Shared retry and transport fallback decisions for Responses requests.

use std::time::Duration;

use crate::client::ModelClientSession;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::util::backoff;
use codex_client::RetryOperation;
use codex_features::Feature;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::WarningEvent;
use tracing::warn;

const INITIAL_CONNECTION_RETRY_DELAY: Duration = Duration::from_secs(5);
const MAX_CONNECTION_RETRY_DELAY: Duration = Duration::from_secs(60);

/// 2026-08-26: John saw codex_tui's RSS climb past 75GB and killed it, on a
/// session running against the local MLX profile only -- confirmed never
/// reproduces against normal (OpenAI-hosted) codex. This retry path is the
/// leading suspect: `UnboundedConnectionRetries` is `default_enabled: true`
/// and unconditionally live for this profile (not internal, not Bedrock), and
/// unlike the bounded branch below it has no retry-count ceiling at all on
/// `ConnectionFailed` -- it can retry forever. A connection genuinely
/// resetting is far more plausible against a single local uvicorn process
/// pegged doing MLX inference than against OpenAI's infrastructure, which is
/// consistent with "only happens locally."
///
/// This does not fix a leak -- no leak has been confirmed in this function,
/// which holds only a few bytes of retry-counter state. It exists so a
/// retry storm is visible in the SAME timeline as tui's mem_watchdog RSS
/// log, instead of the two being two separate, uncorrelated blind spots. If
/// retry lines and RSS-threshold lines climb together next time, that
/// confirms this path; if RSS climbs with no retry lines nearby, it rules
/// this path out and points elsewhere.
fn log_retry_diagnostic(kind: &str, retry_count: u64, delay: Duration, err: &CodexErr) {
    let Ok(codex_home) = codex_utils_home_dir::find_codex_home() else {
        return;
    };
    let path = codex_home.join("codex-memory.log");
    let unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        use std::io::Write;
        let _ = writeln!(
            f,
            "unix={unix} pid={} RETRY kind={kind} count={retry_count} delay={delay:?} err={err:#}",
            std::process::id()
        );
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum ResponsesStreamRequest {
    Sampling,
    RemoteCompactionV2,
}

pub(crate) struct ResponsesStreamRetryState {
    retries: u64,
    connection_retries: u64,
    connection_retry_delay: Duration,
}

impl Default for ResponsesStreamRetryState {
    fn default() -> Self {
        Self {
            retries: 0,
            connection_retries: 0,
            connection_retry_delay: INITIAL_CONNECTION_RETRY_DELAY,
        }
    }
}

/// Handles a retryable stream error and returns `Ok(())` when the caller should
/// retry the request loop.
pub(crate) async fn handle_retryable_response_stream_error(
    retry_state: &mut ResponsesStreamRetryState,
    max_retries: u64,
    err: CodexErr,
    client_session: &mut ModelClientSession,
    sess: &Session,
    turn_context: &TurnContext,
    request: ResponsesStreamRequest,
) -> Result<(), CodexErr> {
    let operation = match request {
        ResponsesStreamRequest::Sampling => RetryOperation::Sampling,
        ResponsesStreamRequest::RemoteCompactionV2 => RetryOperation::RemoteCompactionV2,
    };

    if turn_context
        .config
        .features
        .enabled(Feature::UnboundedConnectionRetries)
        && matches!(request, ResponsesStreamRequest::Sampling)
        && matches!(err.details(), CodexErrorDetails::ConnectionFailed(_))
        && !turn_context.session_source.is_internal()
        && !turn_context.provider.info().is_amazon_bedrock()
    {
        let retry_delay = retry_state.connection_retry_delay;
        warn!(
            turn_id = %turn_context.sub_id,
            error = %err,
            ?retry_delay,
            "stream connection failed; waiting to retry"
        );
        retry_state.connection_retries = retry_state.connection_retries.saturating_add(1);
        log_retry_diagnostic(
            "unbounded_connection",
            retry_state.connection_retries,
            retry_delay,
            &err,
        );
        sess.notify_stream_error(turn_context, "Reconnecting... waiting for network", err)
            .await;
        codex_client::record_retry!(retry_state.connection_retries, retry_delay, operation);
        tokio::time::sleep(retry_delay).await;
        retry_state.connection_retry_delay = retry_delay
            .saturating_mul(2)
            .min(MAX_CONNECTION_RETRY_DELAY);
        return Ok(());
    }

    if retry_state.retries >= max_retries
        && client_session.try_switch_fallback_transport(
            &turn_context.session_telemetry,
            &turn_context.model_info,
        )
    {
        sess.send_event(
            turn_context,
            EventMsg::Warning(WarningEvent {
                message: format!("Falling back from WebSockets to HTTPS transport. {err:#}"),
            }),
        )
        .await;
        retry_state.retries = 0;
        return Ok(());
    }

    if retry_state.retries < max_retries {
        retry_state.retries += 1;
        let retry_count = retry_state.retries;
        // A server-signaled delay (e.g. Retry-After) always wins. Otherwise,
        // when the provider sets stream_reconnect_delay_ms (mlx-local: a
        // single local server with stream_max_retries=1, where the default
        // ~200ms first-attempt backoff is too fast for a transient blip to
        // actually clear), floor the computed backoff at that value instead
        // of replacing it -- later attempts (if max_retries > 1) still grow
        // normally past the floor rather than getting stuck at it.
        let computed_backoff = backoff(retry_count);
        let delay = err.retry_delay().unwrap_or_else(|| {
            match turn_context.provider.info().stream_reconnect_delay() {
                Some(floor) => computed_backoff.max(floor),
                None => computed_backoff,
            }
        });
        log_retry(request, turn_context, &err, retry_count, max_retries, delay);
        log_retry_diagnostic("bounded", retry_count, delay, &err);

        // In release builds, hide the first websocket retry notification to reduce noisy
        // transient reconnect messages. In debug builds, keep full visibility for diagnosis.
        let report_error = retry_count > 1
            || cfg!(debug_assertions)
            || !sess.services.model_client.responses_websocket_enabled();
        if report_error {
            // Surface retry information to any UI/front-end so the user understands what is
            // happening instead of staring at a seemingly frozen screen.
            sess.notify_stream_error(
                turn_context,
                format!("Reconnecting... {retry_count}/{max_retries}"),
                err,
            )
            .await;
        }
        codex_client::record_retry!(retry_count, delay, operation);
        tokio::time::sleep(delay).await;
        return Ok(());
    }

    Err(err)
}

fn log_retry(
    request: ResponsesStreamRequest,
    turn_context: &TurnContext,
    err: &CodexErr,
    retries: u64,
    max_retries: u64,
    delay: Duration,
) {
    match request {
        ResponsesStreamRequest::Sampling => {
            warn!(
                turn_id = %turn_context.sub_id,
                retries,
                max_retries,
                sampling_error = %err,
                "stream disconnected - retrying sampling request ({retries}/{max_retries} in {delay:?})...",
            );
        }
        ResponsesStreamRequest::RemoteCompactionV2 => {
            warn!(
                turn_id = %turn_context.sub_id,
                retries,
                max_retries,
                compact_error = %err,
                "remote compaction v2 stream failed; retrying request after delay"
            );
        }
    }
}

#[cfg(test)]
#[path = "responses_retry_tests.rs"]
mod tests;
