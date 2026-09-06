//! On-disk lock state: a fixed-size record of the current generation and
//! the last heartbeat timestamp, persisted at the lock's own path.
//!
//! The record survives across acquire/release cycles (the file is never
//! deleted, only overwritten in place), which is what lets the generation
//! counter stay monotonic across a crash and a fresh process starting
//! cold.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Size in bytes of the persisted record: an 8-byte little-endian
/// generation followed by a 16-byte little-endian heartbeat timestamp
/// (milliseconds since the Unix epoch). Public to the crate so the
/// read-only observation API (`current_generation`) can distinguish a
/// truncated state file from a full one.
pub(crate) const RECORD_LEN: usize = 8 + 16;

/// A heartbeat of `0` means "never held, or explicitly released" — the
/// lock is free regardless of `stale_after`.
const FREE_HEARTBEAT: u128 = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LockState {
    pub generation: u64,
    pub heartbeat_millis: u128,
}

impl LockState {
    pub fn is_free(&self, now_millis: u128, stale_after_millis: u128) -> bool {
        self.heartbeat_millis == FREE_HEARTBEAT
            || now_millis.saturating_sub(self.heartbeat_millis) > stale_after_millis
    }
}

/// Opens (creating if necessary) the state file backing a lock path.
pub(crate) fn open_state_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(path)
}

/// Reads the current state. A file shorter than a full record (freshly
/// created, or truncated) is treated as never-held: generation 0, free.
///
/// Caller is responsible for holding whatever OS-level lock on `file` is
/// appropriate for the read (exclusive for read-modify-write, shared for
/// a plain observation).
pub(crate) fn read_state(file: &mut File) -> io::Result<LockState> {
    file.seek(SeekFrom::Start(0))?;
    let mut buf = [0u8; RECORD_LEN];
    let mut filled = 0;
    loop {
        match file.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    if filled < RECORD_LEN {
        return Ok(LockState {
            generation: 0,
            heartbeat_millis: FREE_HEARTBEAT,
        });
    }
    let generation = u64::from_le_bytes(buf[0..8].try_into().unwrap());
    let heartbeat_millis = u128::from_le_bytes(buf[8..24].try_into().unwrap());
    Ok(LockState {
        generation,
        heartbeat_millis,
    })
}

/// Overwrites the record in place. Caller must hold an exclusive OS-level
/// lock on `file` for the duration of the read-decide-write it belongs to.
pub(crate) fn write_state(file: &mut File, state: LockState) -> io::Result<()> {
    let mut buf = [0u8; RECORD_LEN];
    buf[0..8].copy_from_slice(&state.generation.to_le_bytes());
    buf[8..24].copy_from_slice(&state.heartbeat_millis.to_le_bytes());
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&buf)?;
    file.flush()?;
    file.sync_data()?;
    Ok(())
}

/// Current wall-clock time as milliseconds since the Unix epoch. Used for
/// the persisted heartbeat so it remains meaningful to any process that
/// later opens the same lock path, including one starting cold after a
/// crash.
pub(crate) fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
