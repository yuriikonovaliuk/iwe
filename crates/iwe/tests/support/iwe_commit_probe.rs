//! Probe-only fixture for the `[commit]` trigger end-to-end tests
//! (`efforts/knowledge-compositor/m6-b-cutover-preconditions/
//! 6-t1-iwe-commit-trigger`, acceptance criteria 2, 4 and 5).
//!
//! This binary exists *only* to be spawned by IWE's `[commit]` trigger
//! machinery inside the tests; it is not part of any product surface and
//! carries no behavior of its own beyond recording what it observes.
//! It appends exactly **one** newline-delimited JSON line per invocation
//! to the log file named by its first argument, capturing:
//!
//!   - `root`: the `IWE_STORE_ROOT` env var (absolute store path),
//!   - `presented`: `IWE_COMMIT_LOCK_GENERATION` parsed as a decimal u64,
//!   - `current`: `iwe_lock::current_generation(<root>/.iwe/write.lock)`,
//!   - `held`: `iwe_lock::is_held_now(<root>/.iwe/write.lock, 1s)`,
//!   - `at_ms`: wall-clock epoch milliseconds at record time.
//!
//! Tests assert `presented == current` and `held == true` — the probe ran
//! inside the caller's commit-lock window — and count lines here as
//! trigger invocations.
//!
//! Flags (after the log path):
//!   - `--sleep-ms N`: sleep N ms *before* recording, so a trigger that
//!     is killed by its `timeout_seconds` bound records nothing (the
//!     boundedness test's probe side);
//!   - `--exit N`: exit with status N after recording.

use std::env;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;

fn epoch_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn main() {
    let mut args = env::args().skip(1);
    let log_path = match args.next() {
        Some(path) => path,
        None => {
            eprintln!("iwe_commit_probe: missing log-file argument");
            std::process::exit(125);
        }
    };
    let mut sleep_ms: u64 = 0;
    let mut exit_code: i32 = 0;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--sleep-ms" => {
                sleep_ms = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .expect("--sleep-ms takes a millisecond count");
            }
            "--exit" => {
                exit_code = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .expect("--exit takes a status code");
            }
            other => {
                eprintln!("iwe_commit_probe: unknown flag {other:?}");
                std::process::exit(126);
            }
        }
    }

    if sleep_ms > 0 {
        std::thread::sleep(Duration::from_millis(sleep_ms));
    }

    let root = env::var("IWE_STORE_ROOT").unwrap_or_default();
    let lock_path = Path::new(&root).join(".iwe").join("write.lock");
    let presented = env::var("IWE_COMMIT_LOCK_GENERATION")
        .ok()
        .and_then(|value| value.parse::<u64>().ok());
    let current = iwe_lock::current_generation(&lock_path)
        .ok()
        .flatten()
        .map(|generation| generation.0);
    let held = iwe_lock::is_held_now(&lock_path, Duration::from_secs(1)).ok();

    let line = json!({
        "root": root,
        "presented": presented,
        "current": current,
        "held": held,
        "at_ms": epoch_millis(),
    });
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .expect("probe log file opens");
    writeln!(file, "{line}").expect("probe log line appends");
    drop(file);

    if exit_code != 0 {
        std::process::exit(exit_code);
    }
}