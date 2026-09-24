//! The `[commit]` trigger: an out-of-process command IWE runs once per
//! successful commit whose journal append produced a record (see
//! [`crate::journal`]).
//!
//! The command is run through the shell (`sh -c`), with the store root as
//! its working directory and the parent process's environment plus two
//! variables: [`ENV_STORE_ROOT`] (the absolute store path) and
//! [`ENV_LOCK_GENERATION`] (the decimal generation of the commit-lock
//! hold the commit ran under). It is invoked at the journal-record commit
//! point — after the record actually landed on disk, while the commit
//! lock is still held (or, for validated paths whose backend already
//! released its hold, inside a fresh hold acquired for the trigger's own
//! window) — and before the caller's tool call returns.
//!
//! **Fail-open by construction.** The trigger is a report, never a gate:
//! a command that cannot start, exits non-zero, or outlives
//! `[commit] timeout_seconds` must not cost the caller its already-landed
//! write or journal record, must not change the commit's result or the
//! caller's exit code, and may print at most one line to stderr, always
//! prefixed `iwe: commit trigger failed (ignored):`.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use liwe::write_lock::{acquire_commit_lock, CommitLockGuard};

use crate::config::CommitOptions;
use crate::journal::{self, KeyEffect};

/// Env var telling the trigger where the store is: the absolute store
/// root it runs in.
pub const ENV_STORE_ROOT: &str = "IWE_STORE_ROOT";
/// Env var telling the trigger which commit-lock hold it ran under: the
/// decimal `.0` of the hold's [`iwe_lock::Generation`].
pub const ENV_LOCK_GENERATION: &str = "IWE_COMMIT_LOCK_GENERATION";

/// `[commit] timeout_seconds`'s default when the key is absent.
pub const DEFAULT_TIMEOUT_SECONDS: u64 = 30;

/// Everything a journal-record commit point needs to run the `[commit]`
/// trigger: the configured options plus the store root the trigger's
/// `cwd` and [`ENV_STORE_ROOT`] resolve from.
#[derive(Debug, Clone, Copy)]
pub struct CommitTriggerContext<'a> {
    pub options: &'a CommitOptions,
    pub store_root: &'a Path,
}

/// The single choke point every production journal-record commit site
/// routes through: appends the record when a journal is configured and
/// `effects` is non-empty, then runs the `[commit]` trigger exactly when
/// that append produced a record — and not otherwise.
///
/// `hold` names the commit-lock hold currently live at this call site:
/// `Some(&guard)` for a write path that still holds the lock while
/// journaling (the CLI's `NoopTransaction` paths, iwec's non-transaction
/// write paths), `None` for a validated path whose backend took and
/// released its own hold inside `commit()` — the trigger then acquires a
/// fresh hold for its own window, so it always observes a current, held
/// lock. With no committed hold available the trigger is skipped
/// silently (the journal record still lands).
pub fn record_commit_and_trigger(
    journal_path: Option<&Path>,
    effects: Vec<KeyEffect>,
    trigger: Option<CommitTriggerContext<'_>>,
    hold: Option<&CommitLockGuard>,
) {
    let Some(trigger) = trigger else {
        journal::record_commit(journal_path, effects);
        return;
    };
    if journal::record_commit(journal_path, effects) {
        run_trigger(trigger.options, trigger.store_root, hold);
    }
}

/// Runs one trigger invocation, fail-open and time-bounded.
fn run_trigger(commit: &CommitOptions, store_root: &Path, hold: Option<&CommitLockGuard>) {
    let Some(command) = &commit.command else {
        return;
    };
    let timeout = Duration::from_secs(commit.timeout_seconds.unwrap_or(DEFAULT_TIMEOUT_SECONDS));

    // The generation to present as IWE_COMMIT_LOCK_GENERATION: the live
    // hold's when one was handed in; otherwise acquire a fresh hold so the
    // trigger still runs inside a commit-lock window and observes a
    // current, held lock. Fail-open: an acquire failure (lock busy for the
    // entire acquire window) skips the trigger without a word — the
    // commitment already landed.
    let (generation, _held) = match hold {
        Some(guard) => (guard.generation().0, None),
        None => match acquire_commit_lock(store_root) {
            Ok(guard) => (guard.generation().0, Some(guard)),
            Err(_) => return,
        },
    };

    // `cwd` must be the absolute store path (spawn fails otherwise), and
    // both env vars carry the absolute form of it.
    let store_root = std::path::absolute(store_root).unwrap_or_else(|_| store_root.to_path_buf());

    // The trigger's `kc` lives beside this binary (both installed into the
    // owner's bin directory); a caller's PATH may not reach it — sudo
    // resets PATH when the store's owner runs iwe — so this binary's own
    // directory goes first.
    let path = {
        let own_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf()));
        let current = std::env::var("PATH").unwrap_or_default();
        match own_dir {
            Some(dir) => format!("{}:{current}", dir.display()),
            None => current,
        }
    };

    let mut child = match Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(&store_root)
        .env("PATH", path)
        .env(ENV_STORE_ROOT, &store_root)
        .env(ENV_LOCK_GENERATION, generation.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            notice(format!("could not start '{command}': {error}"));
            return;
        }
    };

    // The trigger's stderr is drained on a thread (a full pipe must never
    // stall it) and one line kept as the reason a failure notice names:
    // the last line carrying a `KC-` code (kc prints detail lines after
    // it), else the last non-empty line.
    let last_stderr_line = child.stderr.take().map(|stderr| {
        std::thread::spawn(move || {
            use std::io::BufRead;
            let mut last = None;
            let mut coded = None;
            for line in std::io::BufReader::new(stderr).lines().map_while(Result::ok) {
                if line.trim().is_empty() {
                    continue;
                }
                if line.contains("KC-") {
                    coded = Some(line.clone());
                }
                last = Some(line);
            }
            coded.or(last)
        })
    });
    let reason = move || {
        last_stderr_line
            .and_then(|reader| reader.join().ok().flatten())
            .map(|line| {
                let line = line.trim();
                let cut: String = line.chars().take(300).collect();
                format!(": {cut}")
            })
            .unwrap_or_default()
    };

    // Bounded wait: never block the caller past ~timeout. `try_wait` is
    // reaped on a short poll so a hung child cannot wedge the commit.
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    notice(format!("'{command}' exited with {status}{}", reason()));
                }
                return;
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                notice(format!(
                    "'{command}' did not finish within {}s",
                    timeout.as_secs()
                ));
                return;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(error) => {
                let _ = child.kill();
                notice(format!("waiting for '{command}' failed: {error}"));
                return;
            }
        }
    }
}

/// The one line a trigger failure may print — and the only line this
/// module may print to stderr at all.
fn notice(message: String) {
    let _ = writeln!(
        std::io::stderr(),
        "iwe: commit trigger failed (ignored): {message}"
    );
}
