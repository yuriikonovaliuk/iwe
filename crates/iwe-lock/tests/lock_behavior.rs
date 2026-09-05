//! Behavioral test suite for iwe-lock, built directly from the task
//! contract's acceptance criteria and shared surface
//! (efforts/knowledge-compositor/m6-b-cutover-preconditions/5-iwe-t1-lock-crate/contract).
//! Written without reading Developer's implementation of crates/iwe-lock;
//! every name used below comes from the contract's "Shared surface" section.
//!
//! Timing note: all waiting in these tests happens *inside* iwe_lock's own
//! bounded acquire()/heartbeat machinery (short configured durations), not
//! via manual `thread::sleep` races against real-world timing -- that keeps
//! the suite both fast and deterministic.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use iwe_lock::{AcquireError, FileLock, LockConfig};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A fresh, collision-free lock file path per test (and per call within a
/// test), so parallel `#[test]` runs never contend with each other.
fn unique_lock_path(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX_EPOCH")
        .as_nanos();
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!(
        "iwe-lock-test-{}-{}-{}-{}.lock",
        std::process::id(),
        tag,
        nanos,
        n
    ))
}

fn config(
    path: PathBuf,
    heartbeat_interval: Duration,
    stale_after: Duration,
    acquire_timeout: Duration,
) -> LockConfig {
    LockConfig {
        path,
        heartbeat_interval,
        stale_after,
        acquire_timeout,
    }
}

/// Contract acceptance criterion (a) / property 2 (generation monotonicity):
/// "two sequential acquire/release cycles yield strictly increasing
/// generations." Uses two independent `FileLock` instances against the same
/// path -- standing in for two separate processes -- to also exercise the
/// requirement that the generation counter is persisted on disk, not only
/// held in the first instance's process memory.
#[test]
fn sequential_acquire_release_cycles_yield_increasing_generations() {
    let path = unique_lock_path("seq-gen");

    let lock1 = FileLock::new(config(
        path.clone(),
        Duration::from_millis(20),
        Duration::from_millis(300),
        Duration::from_millis(300),
    ));
    let guard1 = lock1.acquire().expect("first acquire on a free lock must succeed");
    let gen1 = guard1.generation();
    guard1.release();

    let lock2 = FileLock::new(config(
        path,
        Duration::from_millis(20),
        Duration::from_millis(300),
        Duration::from_millis(300),
    ));
    let guard2 = lock2
        .acquire()
        .expect("second acquire, after the first release, must succeed");
    let gen2 = guard2.generation();
    guard2.release();

    assert!(
        gen2 > gen1,
        "second cycle's generation ({:?}) must be strictly greater than the first's ({:?})",
        gen2,
        gen1
    );
}

/// Contract acceptance criterion (b) / properties 1 & 3 (heartbeat liveness
/// and fencing): a lock whose holder's heartbeat has gone stale is reclaimed
/// by a second acquirer, and the first guard's `is_current()` subsequently
/// reports `false`. Holder1's heartbeat_interval is set far longer than the
/// test so it heartbeats once and then goes quiet -- a hung-but-not-crashed
/// holder, matching property 1's "liveness = active heartbeat, not OS
/// process-existence" framing (no real crash or OS-level check is used).
#[test]
fn stale_holder_is_reclaimed_and_original_guard_loses_fencing() {
    let path = unique_lock_path("stale-reclaim");

    let holder = FileLock::new(config(
        path.clone(),
        Duration::from_secs(3600), // effectively never heartbeats again
        Duration::from_millis(300),
        Duration::from_millis(300),
    ));
    let guard1 = holder.acquire().expect("holder must acquire the free lock");
    let gen1 = guard1.generation();

    // Short stale_after relative to holder's (silent) heartbeat interval,
    // with an acquire_timeout generous enough to poll past that threshold
    // and reclaim rather than time out.
    let reclaimer = FileLock::new(config(
        path,
        Duration::from_millis(20),
        Duration::from_millis(30),
        Duration::from_millis(500),
    ));
    let guard2 = reclaimer
        .acquire()
        .expect("a lock whose holder has gone stale must be reclaimable, not time out");
    let gen2 = guard2.generation();

    assert!(
        gen2 > gen1,
        "reclaimed generation ({:?}) must be strictly greater than the reclaimed-from generation ({:?})",
        gen2,
        gen1
    );
    assert_eq!(
        guard1.is_current().expect("fencing check must not itself error"),
        false,
        "original guard must report it is no longer current once its lock has been reclaimed"
    );

    guard2.release();
}

/// Contract acceptance criterion (c) / property 1 (heartbeat liveness,
/// negative case): a lock with an actively heartbeating holder is never
/// reclaimed by a concurrent acquire attempt, even after waiting several
/// multiples of heartbeat_interval. The wait itself comes from the
/// contender's own acquire_timeout (set to 5x heartbeat_interval) rather
/// than a manual sleep, so the holder's background heartbeat has ample
/// opportunity to keep refreshing during that window.
#[test]
fn live_heartbeating_holder_is_never_reclaimed() {
    let path = unique_lock_path("live-holder");
    let heartbeat_interval = Duration::from_millis(20);

    let holder = FileLock::new(config(
        path.clone(),
        heartbeat_interval,
        Duration::from_secs(3600), // never stale during this test
        Duration::from_millis(200),
    ));
    let guard1 = holder.acquire().expect("holder must acquire the free lock");
    let gen1 = guard1.generation();

    let contender = FileLock::new(config(
        path,
        heartbeat_interval,
        Duration::from_secs(3600),
        heartbeat_interval * 5,
    ));
    let result = contender.acquire();

    match result {
        Err(AcquireError::Timeout) => {}
        Ok(guard) => panic!(
            "an actively heartbeating holder must not be reclaimed; contender wrongly acquired generation {:?}",
            guard.generation()
        ),
        Err(AcquireError::Io(e)) => {
            panic!("expected AcquireError::Timeout against a live holder, got Io error: {e:?}")
        }
    }

    assert_eq!(
        guard1.is_current().expect("fencing check must not itself error"),
        true,
        "the still-live holder's guard must remain current"
    );
    assert_eq!(guard1.generation(), gen1);

    guard1.release();
}

/// Contract acceptance criterion (d) / property 5 (bounded wait): acquiring
/// an actively-held, non-stale lock waits up to acquire_timeout and then
/// returns AcquireError::Timeout rather than hanging.
#[test]
fn acquire_times_out_on_held_non_stale_lock() {
    let path = unique_lock_path("timeout");

    let holder = FileLock::new(config(
        path.clone(),
        Duration::from_millis(20),
        Duration::from_secs(3600),
        Duration::from_millis(200),
    ));
    let guard1 = holder.acquire().expect("holder must acquire the free lock");

    let acquire_timeout = Duration::from_millis(80);
    let contender = FileLock::new(config(
        path,
        Duration::from_millis(20),
        Duration::from_secs(3600),
        acquire_timeout,
    ));

    let started = Instant::now();
    let result = contender.acquire();
    let elapsed = started.elapsed();

    match result {
        Err(AcquireError::Timeout) => {}
        Ok(guard) => panic!(
            "expected AcquireError::Timeout against a held, non-stale lock, but acquired generation {:?}",
            guard.generation()
        ),
        Err(AcquireError::Io(e)) => {
            panic!("expected AcquireError::Timeout against a held, non-stale lock, got Io error: {e:?}")
        }
    }

    assert!(
        elapsed >= acquire_timeout,
        "acquire() returned before its configured timeout elapsed ({:?} < {:?})",
        elapsed,
        acquire_timeout
    );
    // Generous sanity ceiling (not a tight timing assertion): a genuine hang
    // fails the test instead of blocking the suite forever.
    assert!(
        elapsed < acquire_timeout * 10,
        "acquire() took much longer than its configured timeout ({:?} vs configured {:?}); it must not hang",
        elapsed,
        acquire_timeout
    );

    guard1.release();
}
