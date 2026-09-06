//! Read-only lock-state observables introduced for the `[commit]` trigger
//! (`efforts/knowledge-compositor/m6-b-cutover-preconditions/
//! 6-t1-iwe-commit-trigger`, acceptance criterion 1), pinned exactly as
//! the contract's shared surface names them:
//!
//!   - `current_generation(path) -> io::Result<Option<Generation>>`,
//!     `Ok(None)` iff the state file is missing or shorter than one record;
//!   - `is_held_now(path, stale_after) -> io::Result<bool>`, true iff the
//!     recorded heartbeat is fresh — not `0` (never held / explicitly
//!     released) and not older than `stale_after`.
//!
//! Written from the contract alone. The 24-byte on-disk record format
//! (8-byte LE generation, 16-byte LE heartbeat millis) is read from this
//! crate's own committed `state.rs` module and used here only to *craft*
//! deterministic stale/fresh states — the same byte-level technique
//! `crates/iwe/tests/cli_lock_wiring_test.rs` already uses to engineer its
//! lock races from outside the lock API.

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use iwe_lock::{current_generation, is_held_now, FileLock, Generation, LockConfig};

/// A lock config for exercising a live guard deterministically: the
/// production heartbeat interval (100ms) against a 1s stale bound is the
/// same margin the store-wide commit lock's own default uses, so a live
/// guard's recorded heartbeat is always comfortably fresh whenever the
/// observation happens.
fn live_config(path: &Path) -> LockConfig {
    LockConfig {
        path: path.to_path_buf(),
        heartbeat_interval: Duration::from_millis(100),
        stale_after: Duration::from_secs(1),
        acquire_timeout: Duration::from_secs(2),
    }
}

/// A guard that heartbeats so slowly that its on-disk heartbeat stays the
/// acquire-time value for the whole test — the deterministic "stale hold"
/// shape, mirroring `crates/liwe/src/write_lock.rs`'s own
/// `check_fencing_reports_stale_after_reclaim` technique.
fn stale_when_observed_config(path: &Path) -> LockConfig {
    LockConfig {
        path: path.to_path_buf(),
        heartbeat_interval: Duration::from_secs(10),
        stale_after: Duration::from_secs(1),
        acquire_timeout: Duration::from_secs(2),
    }
}

/// Writes a raw 24-byte lock-state record (8-byte LE generation, 16-byte
/// LE heartbeat millis), per this crate's committed `state.rs` layout.
fn write_lock_state(path: &Path, generation: u64, heartbeat_millis: u128) {
    let mut bytes = Vec::with_capacity(24);
    bytes.extend_from_slice(&generation.to_le_bytes());
    bytes.extend_from_slice(&heartbeat_millis.to_le_bytes());
    fs::write(path, bytes).expect("write crafted lock state");
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[test]
fn current_generation_is_none_when_the_state_file_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("write.lock");

    assert_eq!(current_generation(&path).unwrap(), None);
}

#[test]
fn current_generation_is_none_when_the_state_file_is_truncated() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("write.lock");
    // Shorter than the 24-byte record: generation-only frame.
    fs::write(&path, 7u64.to_le_bytes()).unwrap();

    assert_eq!(current_generation(&path).unwrap(), None);
}

#[test]
fn current_generation_reports_the_guards_generation_right_after_acquire() {
    let dir = tempfile::tempdir().unwrap();
    let lock = FileLock::new(live_config(&dir.path().join("write.lock")));

    let guard = lock.acquire().expect("acquire succeeds on a fresh path");

    assert_eq!(
        current_generation(&dir.path().join("write.lock")).unwrap(),
        Some(guard.generation())
    );
}

#[test]
fn current_generation_reports_the_same_generation_after_release() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("write.lock");
    let lock = FileLock::new(live_config(&path));

    let guard = lock.acquire().expect("acquire succeeds");
    let generation = guard.generation();
    drop(guard);

    // Release marks the lock free in place; the recorded generation must
    // survive the cycle so the counter stays monotonic across restarts.
    assert_eq!(
        current_generation(&path).unwrap(),
        Some(generation),
        "release must not erase the recorded generation"
    );
    assert!(
        !is_held_now(&path, Duration::from_secs(1)).unwrap(),
        "the same file must read as not held once released"
    );
}

#[test]
fn is_held_now_is_true_while_a_live_guard_is_heartbeating() {
    let dir = tempfile::tempdir().unwrap();
    let lock = FileLock::new(live_config(&dir.path().join("write.lock")));

    let _guard = lock.acquire().expect("acquire succeeds");

    assert!(
        is_held_now(&dir.path().join("write.lock"), Duration::from_secs(1)).unwrap(),
        "a live, heartbeating guard's hold must be observed as held"
    );
}

#[test]
fn is_held_now_is_false_after_the_guard_is_released() {
    let dir = tempfile::tempdir().unwrap();
    let lock = FileLock::new(live_config(&dir.path().join("write.lock")));

    let guard = lock.acquire().expect("acquire succeeds");
    drop(guard);

    assert!(
        !is_held_now(&dir.path().join("write.lock"), Duration::from_secs(1)).unwrap(),
        "an explicitly released hold (heartbeat 0) must read as not held"
    );
}

#[test]
fn is_held_now_is_false_when_the_state_file_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("write.lock");

    assert!(!is_held_now(&path, Duration::from_secs(1)).unwrap());
}

#[test]
fn is_held_now_is_false_for_a_stale_heartbeat_and_true_for_a_fresh_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("write.lock");

    // Same generation, two heartbeat ages: 5 seconds old (stale under a
    // 1s bound) and now (fresh under the same bound).
    write_lock_state(&path, 7, now_millis() - 5_000);
    assert!(
        !is_held_now(&path, Duration::from_secs(1)).unwrap(),
        "a heartbeat older than stale_after must read as not held"
    );
    // The same old heartbeat reads as held under a generous bound —
    // proves `stale_after` is the caller's parameter, not a fixed value.
    assert!(is_held_now(&path, Duration::from_secs(10)).unwrap());

    write_lock_state(&path, 7, now_millis());
    assert!(is_held_now(&path, Duration::from_secs(1)).unwrap());
    assert_eq!(current_generation(&path).unwrap(), Some(Generation(7)));
}

#[test]
fn is_held_now_is_false_for_a_live_guard_whose_hold_has_gone_stale() {
    let dir = tempfile::tempdir().unwrap();
    let lock = FileLock::new(stale_when_observed_config(&dir.path().join("write.lock")));

    // Guard stays alive for the whole test; only its heartbeat goes quiet.
    let _guard = lock.acquire().expect("acquire succeeds");

    // Wait comfortably past the (tiny) stale bound: the recorded heartbeat
    // is the acquire-time value, never refreshed before the 10s interval.
    std::thread::sleep(Duration::from_millis(80));
    assert!(
        !is_held_now(&dir.path().join("write.lock"), Duration::from_millis(20)).unwrap(),
        "a hold whose heartbeat has aged past stale_after must read as not held"
    );
}