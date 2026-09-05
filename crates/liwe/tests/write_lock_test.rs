//! Behavioral test suite for `liwe::write_lock`, built directly from the
//! task contract's acceptance criteria and shared surface
//! (efforts/knowledge-compositor/m6-b-cutover-preconditions/5-iwe-t2-write-lock-wrapper/contract).
//! Written without reading Developer's implementation of
//! `crates/liwe/src/write_lock.rs` -- every name used below comes from the
//! contract's "Shared surface" section.
//!
//! TIMING-INJECTION GAP (contract delivery requirement 2): the pinned
//! public signature is
//! `acquire_commit_lock(repo_root: &Path) -> Result<CommitLockGuard, CommitLockError>`
//! -- no `LockConfig` (or equivalent) is exposed for callers to inject
//! short heartbeat/stale/timeout durations. Two consequences, handled
//! differently below:
//!
//!   - The reclaim/Stale required test case does NOT need config
//!     injection on `acquire_commit_lock` itself: it drives a *second*,
//!     independent `iwe_lock::FileLock` directly at the same on-disk path
//!     (`repo_root.join(DEFAULT_LOCK_PATH)`, per that constant's own doc
//!     comment) with its own short-lived `LockConfig`, to reclaim the lock
//!     out from under the guard `acquire_commit_lock` returned. iwe-lock's
//!     on-disk state format is common to both lock instances regardless of
//!     which durations either side is configured with, so this is fast and
//!     deterministic without needing to know or guess Developer's internal
//!     defaults.
//!   - The concurrent-timeout required test case has no such workaround:
//!     the *second* `acquire_commit_lock` call's own internal wait is what
//!     must expire, and nothing outside that call can shrink it. Its real
//!     running time is whatever internal `acquire_timeout` Developer's
//!     implementation defaults to -- a structural gap in the pinned public
//!     surface regardless of what that default turns out to be. Once
//!     Developer's module landed, running it once (black-box: observing
//!     only pass/fail and elapsed wall time, not reading the
//!     implementation) confirmed the default completes in a few seconds,
//!     not minutes, so it is kept as a normal (non-`#[ignore]`d) test
//!     rather than gated behind `--ignored`. See Test-builder's delivery
//!     report for this task for the full note.
//!
//! PROVENANCE: this suite was drafted, and every assertion validated for
//! sense, against a private, non-authoritative reference double
//! implementing exactly the pinned shared surface (built in the sandbox's
//! scratch area, not part of this delivery, not committed) -- written
//! before `crates/liwe/src/write_lock.rs` existed in this worktree, so
//! nothing here was derived from reading Developer's implementation.
//! Developer's module landed while this suite was being finished; running
//! this file against it (black-box: `cargo test`, observing only
//! pass/fail and elapsed time) is reported in Test-builder's delivery
//! report for this task, not used to alter what any test asserts.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use liwe::write_lock::{acquire_commit_lock, CommitLockError, DEFAULT_LOCK_PATH};

/// A fresh temp directory laid out like an existing iwe repo root: `.iwe/`
/// already exists (mirrors "no user migration required" -- a real repo
/// already has `.iwe`), but no lock file yet.
fn temp_repo_root() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("create temp dir");
    std::fs::create_dir_all(dir.path().join(".iwe")).expect("create .iwe dir");
    dir
}

// ---------------------------------------------------------------------------
// Shared surface: DEFAULT_LOCK_PATH
// ---------------------------------------------------------------------------

/// Shared-surface sanity: the documented default lock path is relative
/// (to be joined onto a repo root) and matches the pre-existing
/// `.iwe/write.lock` convention this module reimplements -- "no user
/// migration required".
#[test]
fn default_lock_path_matches_pre_existing_convention() {
    assert_eq!(DEFAULT_LOCK_PATH, ".iwe/write.lock");
    assert!(
        Path::new(DEFAULT_LOCK_PATH).is_relative(),
        "DEFAULT_LOCK_PATH must be relative to repo root, not absolute"
    );
}

// ---------------------------------------------------------------------------
// Shared surface: acquire_commit_lock / CommitLockGuard::check_fencing
// (baseline, uncontended path)
// ---------------------------------------------------------------------------

/// Acceptance criterion: `acquire_commit_lock` succeeds against a fresh,
/// uncontended repo root and yields a guard whose fencing check reports
/// current (`Ok(())`) immediately after acquire -- the `Ok(true)` baseline
/// underlying the Stale/Io mapping exercised by the tests below.
#[test]
fn acquire_succeeds_on_fresh_repo_and_guard_is_initially_current() {
    let repo = temp_repo_root();
    let guard = acquire_commit_lock(repo.path()).expect(
        "acquire_commit_lock must succeed against a fresh repo root with no contending holder",
    );
    assert!(
        guard.check_fencing().is_ok(),
        "a freshly acquired guard, unsuperseded, must pass check_fencing()"
    );
}

/// Acceptance criterion (documentation/design property 5, "widened to the
/// whole commit-attempt window"): the guard remains valid/held across a
/// simulated multi-step commit-attempt sequence -- several no-op "steps",
/// standing in for validate-final-state / apply / journal-append -- as
/// long as nothing reclaims it. Approximates, without a real call site,
/// "callers acquire at commit-attempt start, hold to end."
#[test]
fn guard_remains_current_across_a_multi_step_sequence() {
    let repo = temp_repo_root();
    let guard = acquire_commit_lock(repo.path())
        .expect("acquire_commit_lock must succeed against a fresh repo root");

    for step in 0..5 {
        assert!(
            guard.check_fencing().is_ok(),
            "guard must remain current at step {step} of the sequence, absent any reclaim"
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    assert!(
        guard.check_fencing().is_ok(),
        "guard must still be current at the end of the sequence"
    );
}

// ---------------------------------------------------------------------------
// Required test case: reclaimed lock reports Stale via check_fencing
// ---------------------------------------------------------------------------

/// Required test case: "a guard whose underlying lock was reclaimed
/// (simulate via a short stale_after) reports check_fencing() ==
/// Err(CommitLockError::Stale), not a panic or generic I/O error."
///
/// Reclaim is driven by a second, independent `iwe_lock::FileLock` aimed
/// directly at the same on-disk path `acquire_commit_lock` must be using
/// (`repo_root.join(DEFAULT_LOCK_PATH)`, per that constant's doc comment),
/// configured with a very short `stale_after`. iwe-lock's on-disk state
/// format doesn't depend on which `LockConfig` either side was built with,
/// so as soon as any real time has passed since the guard's last
/// heartbeat write, the reclaimer's own short `stale_after` sees it as
/// free and takes over -- independent of whatever heartbeat/stale
/// durations `acquire_commit_lock` uses internally.
#[test]
fn reclaimed_lock_reports_stale_via_check_fencing() {
    let repo = temp_repo_root();
    let repo_root = repo.path();

    let guard = acquire_commit_lock(repo_root)
        .expect("acquire_commit_lock must succeed against a fresh repo root");
    assert!(
        guard.check_fencing().is_ok(),
        "guard must be current immediately after acquire, before any reclaim"
    );

    // Let a little real time pass so the guard's own heartbeat has had at
    // least one chance to write, then reclaim with a stale_after far
    // shorter than any sane heartbeat/stale configuration.
    std::thread::sleep(Duration::from_millis(20));

    let reclaimer = iwe_lock::FileLock::new(iwe_lock::LockConfig {
        path: repo_root.join(DEFAULT_LOCK_PATH),
        heartbeat_interval: Duration::from_millis(5),
        stale_after: Duration::from_millis(1),
        acquire_timeout: Duration::from_secs(2),
    });
    let reclaimed = reclaimer
        .acquire()
        .expect("reclaimer must be able to take over a lock stale relative to its own short stale_after");

    match guard.check_fencing() {
        Err(CommitLockError::Stale) => {}
        Err(CommitLockError::Io(e)) => panic!(
            "expected CommitLockError::Stale for a superseded guard, got a generic I/O error: {e:?}"
        ),
        Err(_) => panic!("expected CommitLockError::Stale for a superseded guard"),
        Ok(()) => panic!("guard must not still report current once its lock has been reclaimed"),
    }

    reclaimed.release();
}

// ---------------------------------------------------------------------------
// check_fencing's other mapping: Err(_) -> CommitLockError::Io
// ---------------------------------------------------------------------------

/// Acceptance criterion: "any Err(_) [from `LockGuard::is_current()`] to
/// CommitLockError::Io -- never panics, never blocks." Forced by removing
/// the lock file's parent directory out from under a held guard, so the
/// next on-disk read genuinely fails at the OS level (not just "state
/// looks free/stale") -- `check_fencing()` must map that to `Io`, not
/// panic, and not silently report `Stale`.
#[test]
fn check_fencing_maps_io_error_when_lock_state_path_becomes_inaccessible() {
    let repo = temp_repo_root();
    let repo_root = repo.path();

    let guard = acquire_commit_lock(repo_root)
        .expect("acquire_commit_lock must succeed against a fresh repo root");

    std::fs::remove_dir_all(repo_root.join(".iwe"))
        .expect("remove .iwe dir to force the next lock-state access to fail at the OS level");

    match guard.check_fencing() {
        Err(CommitLockError::Io(_)) => {}
        Err(CommitLockError::Stale) => {
            panic!("expected CommitLockError::Io once the lock state is inaccessible, got Stale")
        }
        Err(_) => panic!("expected CommitLockError::Io once the lock state is inaccessible"),
        Ok(()) => panic!("check_fencing must not report current once its lock state is gone"),
    }
}

// ---------------------------------------------------------------------------
// Required test case: second concurrent acquire_commit_lock times out,
// never hangs
// ---------------------------------------------------------------------------

/// Outcome of the second, contending `acquire_commit_lock` call, reduced
/// to a plain, `Send`-safe shape so nothing from `liwe::write_lock` itself
/// (whose `Send`/`Debug` bounds the pinned surface doesn't guarantee) has
/// to cross the thread boundary below.
enum SecondCallOutcome {
    TimedOut,
    Succeeded,
    OtherError,
}

/// Required test case: "two concurrent acquire_commit_lock calls on the
/// same repo root -- the second blocks then returns
/// CommitLockError::Timeout, never hangs."
///
/// TIMING GAP (see module doc): `acquire_commit_lock` exposes no
/// config-injection point, so this test's real duration depends on
/// whatever `acquire_timeout` Developer's implementation defaults to
/// internally -- observed at a few seconds in practice, not ignored. The
/// bounded `recv_timeout` below is a hang-guard, not a timing assertion:
/// it exists so that even a much-longer-than-observed default fails
/// loudly with a clear message rather than blocking the test run forever.
#[test]
fn second_concurrent_acquire_times_out_and_never_hangs() {
    let repo = temp_repo_root();
    let repo_root: PathBuf = repo.path().to_path_buf();

    // First holder: acquired and kept alive for the whole test (its Drop,
    // at the end of this function, releases it).
    let _holder = acquire_commit_lock(&repo_root)
        .expect("first acquire_commit_lock on a free repo root must succeed");

    // Second call, against the same repo root, run on a worker thread so a
    // genuine hang (as opposed to a slow-but-bounded wait) fails this test
    // instead of blocking the suite forever.
    let (tx, rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let outcome = match acquire_commit_lock(&repo_root) {
            Err(CommitLockError::Timeout) => SecondCallOutcome::TimedOut,
            Ok(guard) => {
                drop(guard);
                SecondCallOutcome::Succeeded
            }
            Err(_) => SecondCallOutcome::OtherError,
        };
        let _ = tx.send(outcome);
    });

    // Generous hang-guard, not a timing assertion: any real, working
    // implementation should report Timeout well within this.
    let outcome = rx.recv_timeout(Duration::from_secs(120)).unwrap_or_else(|_| {
        panic!(
            "second acquire_commit_lock on an already-held repo root did not return within \
             120s -- looks like a hang, not a bounded timeout"
        )
    });

    match outcome {
        SecondCallOutcome::TimedOut => {}
        SecondCallOutcome::Succeeded => panic!(
            "second, concurrent acquire_commit_lock on an already-held repo root must not \
             succeed"
        ),
        SecondCallOutcome::OtherError => panic!(
            "second, concurrent acquire_commit_lock on an already-held repo root returned an \
             error other than CommitLockError::Timeout"
        ),
    }

    let _ = handle.join();
}
