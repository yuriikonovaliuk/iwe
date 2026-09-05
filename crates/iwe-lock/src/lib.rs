//! `iwe-lock`: a standalone, crash-safe file lock.
//!
//! Three properties this crate exists to provide, independent of any
//! particular caller:
//!
//! 1. **Heartbeat liveness.** A held lock's holder is considered alive only
//!    while it is actively touching the on-disk lock state at
//!    [`LockConfig::heartbeat_interval`]. A holder whose heartbeat is older
//!    than [`LockConfig::stale_after`] is reclaimable — a hung-but-not-crashed
//!    holder is reclaimed exactly like a crashed one. This is deliberately
//!    *not* based on OS process-existence checks.
//!
//!    **Invariant the caller must uphold:** `stale_after` must exceed
//!    `heartbeat_interval` by a safety margin. If it doesn't, a correctly
//!    heartbeating, genuinely live holder can be spuriously reclaimed by a
//!    concurrent acquirer — the margin has to absorb scheduling jitter and
//!    the time an acquire attempt's own critical section takes.
//!
//! 2. **Monotonic generations.** Every successful [`FileLock::acquire`]
//!    (including one that reclaims a stale lock) returns a [`Generation`]
//!    strictly greater than every generation previously returned for that
//!    lock path. The counter is persisted on disk (not only in process
//!    memory), so this holds across the holder's own crash and a fresh
//!    process starting cold.
//!
//! 3. **Fencing.** [`LockGuard::is_current`] lets a holder check, immediately
//!    before an irreversible step, whether its generation is still the one
//!    recorded on disk — i.e. whether its hold has been superseded by a
//!    later acquire or reclaim.
//!
//! Bounded wait (property 5): [`FileLock::acquire`] never blocks
//! indefinitely. It waits up to [`LockConfig::acquire_timeout`] and then
//! returns [`AcquireError::Timeout`].

mod heartbeat;
mod state;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use heartbeat::Heartbeat;
use state::{now_millis, open_state_file, read_state, write_state, LockState};

/// Configuration for a [`FileLock`].
#[derive(Debug, Clone)]
pub struct LockConfig {
    /// Path to the lock's on-disk state file. Created if it doesn't exist.
    pub path: PathBuf,
    /// How often a held lock's guard touches the on-disk state to prove
    /// it is still alive.
    pub heartbeat_interval: Duration,
    /// How old a holder's last heartbeat must be before its lock is
    /// considered reclaimable.
    ///
    /// Must exceed `heartbeat_interval` by a safety margin: this crate
    /// does not enforce that relationship, but violating it means a
    /// correctly-heartbeating live holder can be spuriously reclaimed.
    pub stale_after: Duration,
    /// Maximum time [`FileLock::acquire`] will wait for a held,
    /// non-stale lock before giving up with [`AcquireError::Timeout`].
    pub acquire_timeout: Duration,
}

/// A monotonically increasing token identifying one successful acquire of
/// a lock. Every acquire (including a reclaim of a stale lock) returns a
/// generation strictly greater than every generation previously returned
/// for that lock's path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Generation(pub u64);

/// Error returned by [`FileLock::acquire`].
#[derive(Debug)]
pub enum AcquireError {
    /// The lock was held (and not stale) for the entire `acquire_timeout`
    /// window.
    Timeout,
    /// An I/O error occurred while accessing the lock's on-disk state.
    Io(std::io::Error),
}

impl std::fmt::Display for AcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AcquireError::Timeout => write!(f, "timed out waiting to acquire lock"),
            AcquireError::Io(e) => write!(f, "I/O error acquiring lock: {e}"),
        }
    }
}

impl std::error::Error for AcquireError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AcquireError::Io(e) => Some(e),
            AcquireError::Timeout => None,
        }
    }
}

impl From<std::io::Error> for AcquireError {
    fn from(e: std::io::Error) -> Self {
        AcquireError::Io(e)
    }
}

/// How aggressively `acquire` polls a held, non-stale lock while waiting.
/// Scaled off the configured heartbeat interval so it stays responsive to
/// fast test configurations without busy-looping on slow ones.
fn poll_interval(heartbeat_interval: Duration) -> Duration {
    let candidate = heartbeat_interval / 4;
    candidate.clamp(Duration::from_millis(1), Duration::from_millis(25))
}

/// A crash-safe file lock at a configured path.
///
/// Carries no state of its own beyond its [`LockConfig`] — all lock state
/// lives on disk at `config.path`, which is what makes generations and
/// liveness survive a crash of any single holder.
pub struct FileLock {
    config: LockConfig,
}

impl FileLock {
    pub fn new(config: LockConfig) -> Self {
        FileLock { config }
    }

    /// Attempts to acquire the lock, waiting for a currently-held,
    /// non-stale lock to free up or go stale, up to `acquire_timeout`.
    ///
    /// Never blocks indefinitely: returns [`AcquireError::Timeout`] rather
    /// than waiting past the configured timeout.
    pub fn acquire(&self) -> Result<LockGuard, AcquireError> {
        let deadline = Instant::now() + self.config.acquire_timeout;

        loop {
            let mut file = open_state_file(&self.config.path)?;
            file.lock()?;
            let outcome = (|| -> Result<Option<Generation>, std::io::Error> {
                let now = now_millis();
                let state = read_state(&mut file)?;
                if state.is_free(now, self.config.stale_after.as_millis()) {
                    let new_generation = state.generation + 1;
                    write_state(
                        &mut file,
                        LockState {
                            generation: new_generation,
                            heartbeat_millis: now,
                        },
                    )?;
                    Ok(Some(Generation(new_generation)))
                } else {
                    Ok(None)
                }
            })();
            let _ = file.unlock();
            let acquired = outcome?;

            if let Some(generation) = acquired {
                let heartbeat = Heartbeat::spawn(
                    self.config.path.clone(),
                    generation.0,
                    self.config.heartbeat_interval,
                );
                return Ok(LockGuard {
                    path: self.config.path.clone(),
                    generation,
                    heartbeat: Some(heartbeat),
                });
            }

            if Instant::now() >= deadline {
                return Err(AcquireError::Timeout);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            std::thread::sleep(poll_interval(self.config.heartbeat_interval).min(remaining));
        }
    }
}

/// A held lock. Owns a background thread that heartbeats on-disk state at
/// `heartbeat_interval` for as long as the guard is alive and its
/// generation hasn't been superseded.
pub struct LockGuard {
    path: PathBuf,
    generation: Generation,
    heartbeat: Option<Heartbeat>,
}

impl LockGuard {
    /// The generation this guard was granted on acquire.
    pub fn generation(&self) -> Generation {
        self.generation
    }

    /// Fencing check: is this guard's generation still the one currently
    /// recorded on disk?
    ///
    /// `Ok(false)` means this hold has been superseded — a later acquire
    /// (a reclaim, most likely) has taken over the lock. Intended to be
    /// called immediately before an irreversible step.
    pub fn is_current(&self) -> Result<bool, std::io::Error> {
        let mut file = open_state_file(&self.path)?;
        file.lock_shared()?;
        let result = read_state(&mut file);
        let _ = file.unlock();
        Ok(result?.generation == self.generation.0)
    }

    /// Releases the lock: stops the heartbeat and marks the lock free on
    /// disk (unless it was already superseded). Equivalent to dropping the
    /// guard — provided as an explicit, readable alternative.
    pub fn release(self) {
        // All the work happens in `Drop`.
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        if let Some(mut heartbeat) = self.heartbeat.take() {
            heartbeat.stop_and_join();
        }
        let _ = mark_free_if_current(&self.path, self.generation.0);
    }
}

/// Marks the lock free on disk, but only if it still records `generation`
/// — a guard being released after its hold was already reclaimed by
/// someone else must not clobber the new holder's state.
fn mark_free_if_current(path: &std::path::Path, generation: u64) -> std::io::Result<()> {
    let mut file = open_state_file(path)?;
    file.lock()?;
    let result = (|| {
        let state = read_state(&mut file)?;
        if state.generation == generation {
            write_state(
                &mut file,
                LockState {
                    generation,
                    heartbeat_millis: 0,
                },
            )?;
        }
        Ok(())
    })();
    let _ = file.unlock();
    result
}
