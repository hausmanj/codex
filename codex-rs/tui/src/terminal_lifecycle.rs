//! Detect terminal teardown that would otherwise orphan the TUI process.

#[cfg(unix)]
use std::future;
#[cfg(unix)]
use std::time::Duration;

/// Wait for the terminal session to end.
///
/// A terminal closing normally sends SIGHUP to its foreground process group. The parent check
/// also catches terminals or shells that exit without relaying that signal, which would otherwise
/// leave Codex reparented to PID 1.
#[cfg(unix)]
pub(crate) async fn wait_for_shutdown() {
    use tokio::signal::unix::SignalKind;

    let mut sighup = match tokio::signal::unix::signal(SignalKind::hangup()) {
        Ok(sighup) => Some(sighup),
        Err(err) => {
            tracing::warn!(error = %err, "failed to listen for terminal hangups");
            None
        }
    };
    let mut parent_check = tokio::time::interval(Duration::from_secs(1));
    parent_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = async {
                if let Some(sighup) = sighup.as_mut() {
                    let _ = sighup.recv().await;
                } else {
                    future::pending::<()>().await;
                }
            } => return,
            _ = parent_check.tick() => {
                // SAFETY: getppid has no preconditions and does not dereference pointers.
                if unsafe { libc::getppid() } == 1 {
                    return;
                }
            }
        }
    }
}

#[cfg(not(unix))]
pub(crate) async fn wait_for_shutdown() {
    std::future::pending::<()>().await;
}
