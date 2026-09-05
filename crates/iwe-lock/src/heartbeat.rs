//! Background heartbeat: a thread owned by a `LockGuard` that periodically
//! touches the on-disk lock state so other acquirers can tell this holder
//! is still alive (property 1's liveness signal is active heartbeating,
//! not OS process-existence).

use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::state::{now_millis, open_state_file, read_state, write_state, LockState};

pub(crate) struct Heartbeat {
    stop: Sender<()>,
    handle: Option<JoinHandle<()>>,
}

impl Heartbeat {
    /// Spawns the background thread. It touches the state's heartbeat
    /// timestamp every `interval`, but only while `generation` is still
    /// the generation recorded on disk — once superseded (reclaimed by
    /// someone else), it stops touching the file and idles until told to
    /// stop.
    pub fn spawn(path: PathBuf, generation: u64, interval: Duration) -> Self {
        let (stop, rx) = mpsc::channel::<()>();
        let handle = thread::spawn(move || loop {
            match rx.recv_timeout(interval) {
                Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {
                    let _ = touch_if_current(&path, generation);
                }
            }
        });
        Heartbeat {
            stop,
            handle: Some(handle),
        }
    }

    /// Stops the background thread and blocks until it has exited, so the
    /// caller can safely perform the final "mark free" write afterward
    /// without racing the heartbeat thread's own writes.
    pub fn stop_and_join(&mut self) {
        let _ = self.stop.send(());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Heartbeat {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

/// Touches the heartbeat timestamp of the on-disk state, but only if it
/// still records `generation` — a guard whose hold was already reclaimed
/// must not resurrect stale ownership.
fn touch_if_current(path: &std::path::Path, generation: u64) -> std::io::Result<()> {
    let mut file = open_state_file(path)?;
    file.lock()?;
    let result = (|| {
        let state = read_state(&mut file)?;
        if state.generation == generation {
            write_state(
                &mut file,
                LockState {
                    generation,
                    heartbeat_millis: now_millis(),
                },
            )?;
        }
        Ok(())
    })();
    let _ = file.unlock();
    result
}
