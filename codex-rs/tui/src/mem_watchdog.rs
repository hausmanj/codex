//! Background RSS watchdog.
//!
//! 2026-08-26: three codex_tui crashes in one session, all the same
//! deterministic panic signature (Rust `abort()` via the panic hook, inside
//! `run_ratatui_app`), and a fourth incident where John watched this
//! process's RSS climb past 75 GB in Activity Monitor and killed it before
//! macOS did. Every one of those is a postmortem: a `.ips` crash report or a
//! killed window, discovered after the fact, with no record of what the
//! memory curve actually looked like on the way up.
//!
//! This does not fix whatever is allocating -- that needs the growth curve
//! this produces before it can be found. It exists so the *next* occurrence
//! comes with a timeline instead of a single data point: RSS sampled on a
//! fixed interval, plus an immediate log line the moment a threshold is
//! crossed, so a runaway climb is visible while it is still climbing rather
//! than only once it has already forced a kill.
//!
//! ru_maxrss from getrusage(2) is a high-water mark (never decreases within
//! the process), and on macOS/Darwin it is already in bytes -- unlike Linux,
//! where the same field is kilobytes. Deliberately not using a crate for
//! this (sysinfo etc. are not already a dependency anywhere in this
//! workspace; libc is, in this crate's own Cargo.toml).
//!
//! Writes directly to disk with std::fs rather than going through `tracing`:
//! this needs to work regardless of whatever log level / subscriber
//! configuration is active for a given run, and a background thread it not a
//! tokio task specifically so a stalled/blocked async runtime cannot delay
//! or drop a sample right when it would matter most.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;

const SAMPLE_INTERVAL: Duration = Duration::from_secs(3);

// Escalating GiB thresholds. Logged once each, the moment ru_maxrss first
// crosses them, so a fast climb still gets one line per order of magnitude
// instead of either total silence or a line every 3 seconds once parked
// above the top one.
const THRESHOLDS_GIB: &[u64] = &[1, 2, 4, 8, 12, 16, 24, 32, 48, 64, 80, 96];

// Baseline heartbeat even with no threshold crossed, so a log with nothing
// but threshold lines in it is still known-good evidence ("still under 1
// GiB as of <time>") and not just an absence that could mean anything.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);

fn current_rss_bytes() -> Option<u64> {
    // SAFETY: `usage` is a plain POD struct fully initialized by the kernel
    // before getrusage returns 0; no pointers/lifetimes cross the FFI
    // boundary beyond the single out-param.
    unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &raw mut usage) == 0 {
            Some(usage.ru_maxrss as u64)
        } else {
            None
        }
    }
}

fn log_path(codex_home: &std::path::Path) -> PathBuf {
    codex_home.join("codex-memory.log")
}

fn append_line(path: &std::path::Path, line: &str) {
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{line}");
    }
}

fn now_str() -> String {
    match SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => format!("{}", d.as_secs()),
        Err(_) => "unknown".to_string(),
    }
}

/// Spawn the watchdog. Fire-and-forget: an OS thread that outlives nothing
/// in particular and is killed along with the process on exit, same as any
/// other detached background thread in this binary.
pub fn spawn(codex_home: &std::path::Path) {
    let path = log_path(codex_home);
    let pid = std::process::id();
    append_line(
        &path,
        &format!("=== watchdog start unix={} pid={pid} ===", now_str()),
    );

    std::thread::Builder::new()
        .name("mem-watchdog".to_string())
        .spawn(move || {
            let mut next_threshold_idx = 0usize;
            let mut last_heartbeat = std::time::Instant::now() - HEARTBEAT_INTERVAL;
            loop {
                std::thread::sleep(SAMPLE_INTERVAL);
                let Some(rss) = current_rss_bytes() else {
                    continue;
                };
                let rss_gib = rss as f64 / (1024.0 * 1024.0 * 1024.0);

                while next_threshold_idx < THRESHOLDS_GIB.len()
                    && rss_gib >= THRESHOLDS_GIB[next_threshold_idx] as f64
                {
                    append_line(
                        &path,
                        &format!(
                            "unix={} pid={pid} THRESHOLD {} GiB crossed, rss={:.2} GiB",
                            now_str(),
                            THRESHOLDS_GIB[next_threshold_idx],
                            rss_gib
                        ),
                    );
                    next_threshold_idx += 1;
                }

                if last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
                    append_line(
                        &path,
                        &format!("unix={} pid={pid} heartbeat rss={rss_gib:.2} GiB", now_str()),
                    );
                    last_heartbeat = std::time::Instant::now();
                }
            }
        })
        .ok();
}
