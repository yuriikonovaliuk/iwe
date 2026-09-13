//! The store-wide commit lock, shared by every caller that commits a
//! transaction against an on-disk `.iwe` store (the `iwe` CLI and the
//! `iwec` MCP server alike) — one wrapper over [`iwe_lock::FileLock`]
//! rather than each caller reimplementing lock handling on its own.
//!
//! **Scope.** This lock is meant to be held for the *entire*
//! commit-attempt window: validating the transaction's final state,
//! applying its writes, and appending to the journal
//! (validate-final-state / apply / journal-append) — not some narrower
//! sub-step of it. Callers acquire the lock at the start of a commit
//! attempt and hold the returned [`CommitLockGuard`] until the attempt is
//! fully resolved (committed or abandoned).
//!
//! This module only builds the wrapper: it does not itself change any
//! commit call site to acquire or hold this lock — that is wiring for a
//! later task to do.
//!
//! On-disk location: `.iwe/write.lock` under the repo root
//! ([`DEFAULT_LOCK_PATH`]), matching the path the store-wide commit lock
//! has always lived at, so no migration is required for an existing
//! store.

use std::fmt;
use std::fs;
use std::path::Path;
use std::time::Duration;

use iwe_lock::{AcquireError, FileLock, LockConfig, LockGuard};

/// Where the store-wide commit lock lives, relative to the repo root.
pub const DEFAULT_LOCK_PATH: &str = ".iwe/write.lock";

/// How often a held lock's guard proves it is still alive by touching the
/// on-disk lock state.
const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(100);

/// How long a holder's last heartbeat may go stale before its lock
/// becomes reclaimable — several multiples of [`HEARTBEAT_INTERVAL`], to
/// absorb scheduling jitter and the time a commit attempt's own critical
/// section takes.
const STALE_AFTER: Duration = Duration::from_secs(1);

/// How long [`acquire_commit_lock`] will wait for a held, non-stale lock
/// before giving up with [`CommitLockError::Timeout`].
// A whole-store commit under `[transactions] validate = "full"` with the
// external checkers holds the lock for tens of seconds on a 4k-document
// store (36 s measured on the 2026-09-13 rehearsal), so a contender that
// gives up after a few seconds turns every concurrent write into a
// refusal. Wait long enough for a few queued writers to drain; the
// heartbeat still reclaims a dead holder within `STALE_AFTER`.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(120);

/// Why acquiring or continuing to hold the commit lock failed.
#[derive(Debug)]
pub enum CommitLockError {
    /// The lock was held (and not stale) for the entire acquire-timeout
    /// window.
    Timeout,
    /// The store carries no `.iwe/store.toml` marker and the caller
    /// required one (`IWE_REQUIRE_STORE_MARKER=1`): a directory that
    /// does not identify itself is not written to.
    Unidentified,
    /// The store's marker is owned by another user: only the owner writes.
    NotOwner { owner: u32, me: u32 },
    /// This guard's hold has been superseded — a later acquire (most
    /// likely a reclaim of a stale lock) has taken over. Reported by
    /// [`CommitLockGuard::check_fencing`], never by [`acquire_commit_lock`]
    /// itself.
    Stale,
    /// An I/O error while accessing the lock's on-disk state.
    Io(std::io::Error),
}

impl fmt::Display for CommitLockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CommitLockError::Timeout => {
                write!(f, "timed out waiting to acquire the commit lock")
            }
            CommitLockError::Unidentified => write!(
                f,
                "this directory carries no .iwe/store.toml marker and IWE_REQUIRE_STORE_MARKER is set: not a store"
            ),
            CommitLockError::NotOwner { owner, me } => write!(
                f,
                "the store is owned by uid {owner}; this process runs as uid {me} — write through the owner's iwe"
            ),
            CommitLockError::Stale => write!(
                f,
                "commit lock hold was superseded by a later acquire"
            ),
            CommitLockError::Io(error) => write!(f, "commit lock I/O error: {error}"),
        }
    }
}

impl std::error::Error for CommitLockError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CommitLockError::Io(error) => Some(error),
            CommitLockError::Timeout
            | CommitLockError::Stale
            | CommitLockError::Unidentified
            | CommitLockError::NotOwner { .. } => None,
        }
    }
}

impl From<std::io::Error> for CommitLockError {
    fn from(error: std::io::Error) -> Self {
        CommitLockError::Io(error)
    }
}

/// A held commit lock, covering the whole commit-attempt window. Dropping
/// it (or calling nothing further) releases the underlying lock.
pub struct CommitLockGuard {
    inner: LockGuard,
}

impl CommitLockGuard {
    /// Fencing check: is this hold still the current one on disk?
    ///
    /// Call this immediately before an irreversible step of the commit
    /// (e.g. the journal append). `Ok(())` means the hold is still
    /// current; [`CommitLockError::Stale`] means it has been superseded
    /// and the commit must not proceed. Never panics, never blocks.
    pub fn check_fencing(&self) -> Result<(), CommitLockError> {
        match self.inner.is_current() {
            Ok(true) => Ok(()),
            Ok(false) => Err(CommitLockError::Stale),
            Err(error) => Err(CommitLockError::Io(error)),
        }
    }

    /// The generation this hold was granted on acquire — the value a
    /// `[commit]` trigger must present as its `IWE_COMMIT_LOCK_GENERATION`
    /// env var (the decimal of this `Generation`'s `.0`). Read-only, free.
    pub fn generation(&self) -> iwe_lock::Generation {
        self.inner.generation()
    }
}

/// Acquires the store-wide commit lock at `repo_root`/[`DEFAULT_LOCK_PATH`],
/// waiting for a held, non-stale lock to free up.
///
/// Never blocks indefinitely: returns [`CommitLockError::Timeout`] rather
/// than waiting past the configured acquire timeout.
pub fn acquire_commit_lock(repo_root: &Path) -> Result<CommitLockGuard, CommitLockError> {
    check_store_marker(repo_root)?;
    acquire_with_config(repo_root, HEARTBEAT_INTERVAL, STALE_AFTER, acquire_timeout())
}

/// The store's identity file, `.iwe/store.toml` (ruling 2026-09-13: a
/// store identifies itself). Written by the operator or by `kc
/// materialize`, owned by the store's owner, so it cannot be forged by
/// a process that may not write the store.
pub const STORE_MARKER_PATH: &str = ".iwe/store.toml";

/// Refuses to commit into a directory that is not a store this process
/// may write: with `IWE_REQUIRE_STORE_MARKER=1` (set by the protected
/// wrappers) a missing marker is refused; a present marker owned by
/// another uid is always refused. Runs before the lock is taken, on
/// every CLI and MCP commit path.
pub fn check_store_marker(repo_root: &Path) -> Result<(), CommitLockError> {
    use std::os::unix::fs::MetadataExt;

    let marker = repo_root.join(STORE_MARKER_PATH);
    match fs::metadata(&marker) {
        Ok(meta) => {
            let me = unsafe { libc::geteuid() };
            if meta.uid() != me {
                return Err(CommitLockError::NotOwner { owner: meta.uid(), me });
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if std::env::var("IWE_REQUIRE_STORE_MARKER").map(|v| v == "1").unwrap_or(false) {
                Err(CommitLockError::Unidentified)
            } else {
                Ok(())
            }
        }
        Err(error) => Err(CommitLockError::Io(error)),
    }
}

/// `IWE_COMMIT_LOCK_TIMEOUT_SECS`, when set to a positive integer,
/// overrides [`ACQUIRE_TIMEOUT`] — an operator knob for a store whose
/// commits are unusually slow or fast, and what the lock-wiring tests use
/// to observe a refusal without waiting out the production default.
fn acquire_timeout() -> Duration {
    std::env::var("IWE_COMMIT_LOCK_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs)
        .unwrap_or(ACQUIRE_TIMEOUT)
}

/// Test-only knob: same as [`acquire_commit_lock`], but with the timing
/// parameters exposed so tests can force staleness or a fast timeout
/// without waiting out the production defaults. Not part of the module's
/// public (pinned) surface.
fn acquire_with_config(
    repo_root: &Path,
    heartbeat_interval: Duration,
    stale_after: Duration,
    acquire_timeout: Duration,
) -> Result<CommitLockGuard, CommitLockError> {
    let path = repo_root.join(DEFAULT_LOCK_PATH);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let lock = FileLock::new(LockConfig {
        path,
        heartbeat_interval,
        stale_after,
        acquire_timeout,
    });

    match lock.acquire() {
        Ok(guard) => Ok(CommitLockGuard { inner: guard }),
        Err(AcquireError::Timeout) => Err(CommitLockError::Timeout),
        Err(AcquireError::Io(error)) => Err(CommitLockError::Io(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn a_marker_owned_by_this_user_passes_and_a_missing_one_is_refused_only_when_required() {
        let temp = tempfile::tempdir().unwrap();
        // no marker, not required: fine (plain stores, fixtures)
        std::env::remove_var("IWE_REQUIRE_STORE_MARKER");
        assert!(check_store_marker(temp.path()).is_ok());
        // no marker, required: refused
        std::env::set_var("IWE_REQUIRE_STORE_MARKER", "1");
        assert!(matches!(check_store_marker(temp.path()), Err(CommitLockError::Unidentified)));
        // our own marker: fine
        fs::create_dir_all(temp.path().join(".iwe")).unwrap();
        fs::write(temp.path().join(STORE_MARKER_PATH), "kind = \"git\"\n").unwrap();
        assert!(check_store_marker(temp.path()).is_ok());
        std::env::remove_var("IWE_REQUIRE_STORE_MARKER");
    }
    use std::thread;

    #[test]
    fn concurrent_acquire_commit_lock_times_out_rather_than_hanging() {
        // Observe the refusal in seconds, not the production default (120 s).
        std::env::set_var("IWE_COMMIT_LOCK_TIMEOUT_SECS", "3");
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path().to_path_buf();

        // Hold the lock on a background thread for longer than the
        // second acquirer's timeout.
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let holder_root = repo_root.clone();
        let holder = thread::spawn(move || {
            let guard = acquire_commit_lock(&holder_root).expect("first acquire must succeed");
            ready_tx.send(()).unwrap();
            let _ = release_rx.recv();
            drop(guard);
        });

        ready_rx.recv().unwrap();

        let start = std::time::Instant::now();
        let result = acquire_commit_lock(&repo_root);
        let elapsed = start.elapsed();

        release_tx.send(()).unwrap();
        holder.join().unwrap();

        assert!(
            matches!(result, Err(CommitLockError::Timeout)),
            "expected Timeout, got a non-timeout result"
        );
        // Bounded: didn't hang past a small multiple of the configured
        // acquire timeout.
        assert!(elapsed < Duration::from_secs(15), "took too long: {elapsed:?}");
        std::env::remove_var("IWE_COMMIT_LOCK_TIMEOUT_SECS");
    }

    #[test]
    fn check_fencing_reports_stale_after_reclaim() {
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path().to_path_buf();

        // A very slow heartbeat combined with a very short stale_after
        // means this guard's hold looks stale almost immediately, even
        // though nothing has crashed — simulating a reclaimed lock.
        let guard = acquire_with_config(
            &repo_root,
            Duration::from_secs(10),
            Duration::from_millis(20),
            Duration::from_secs(1),
        )
        .expect("first acquire must succeed");

        thread::sleep(Duration::from_millis(100));

        let _reclaimer = acquire_with_config(
            &repo_root,
            Duration::from_millis(20),
            Duration::from_millis(20),
            Duration::from_secs(1),
        )
        .expect("reclaim must succeed once the first hold is stale");

        assert!(matches!(guard.check_fencing(), Err(CommitLockError::Stale)));
    }

    #[test]
    fn check_fencing_ok_while_hold_is_current() {
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path().to_path_buf();

        let guard = acquire_commit_lock(&repo_root).expect("acquire must succeed");
        assert!(guard.check_fencing().is_ok());
    }

    #[test]
    fn default_lock_path_is_dot_iwe_write_lock() {
        assert_eq!(DEFAULT_LOCK_PATH, ".iwe/write.lock");
    }
}
