//! State the HTTP daemon keeps on disk under `--state-dir` so a restart is
//! invisible to its clients: one file per MCP session (so rmcp can restore a
//! session it has never seen in memory) and the set of open transaction keys
//! (so a transaction a restart dropped is refused by name instead of letting
//! the agent's next write land outside it).
//!
//! Every file is written whole to a sibling temporary file with mode 0600 and
//! renamed into place, so a reader only ever sees a complete old or new copy.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use chrono::{DateTime, SecondsFormat, Utc};
use rmcp::transport::streamable_http_server::session::store::{
    SessionState, SessionStore, SessionStoreError,
};
use serde::{Deserialize, Serialize};

/// Session files untouched for this long are pruned at startup.
pub const SESSION_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// A lost-transaction tombstone expires this long after it was recorded.
pub const TOMBSTONE_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Session activity refreshes the session file's mtime at most this often.
const SESSION_TOUCH_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// The only session IDs the store accepts: they become file names, so
/// nothing that could name a path outside `sessions/` gets through.
pub fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Writes `bytes` to `path` atomically (temporary sibling + rename), mode 0600.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other(format!("{} has no parent directory", path.display())))?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other(format!("{} has no file name", path.display())))?
        .to_string_lossy();
    let tmp = dir.join(format!(
        ".{name}.tmp.{}.{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)?;
        // Make the rename itself durable; best effort on filesystems that
        // refuse to fsync a directory.
        if let Ok(dir) = File::open(dir) {
            let _ = dir.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn create_private_dir(dir: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct SessionFile {
    session_id: String,
    created_at: String,
    state: SessionState,
}

/// rmcp [`SessionStore`] backed by one JSON file per session under
/// `<state-dir>/sessions/`. rmcp stores an entry after a session's
/// `initialize` handshake, deletes it on the client's HTTP DELETE, and loads
/// it when a request names a session this process does not hold in memory —
/// which is how a client survives a daemon restart without re-initializing.
pub struct FileSessionStore {
    dir: PathBuf,
    touched: Mutex<std::collections::HashMap<String, Instant>>,
}

impl FileSessionStore {
    pub fn open(dir: PathBuf) -> io::Result<Self> {
        create_private_dir(&dir)?;
        Ok(Self {
            dir,
            touched: Mutex::new(std::collections::HashMap::new()),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path_for(&self, id: &str) -> Option<PathBuf> {
        valid_session_id(id).then(|| self.dir.join(format!("{id}.json")))
    }

    /// Removes session files not touched for `max_age`, and any temporary
    /// file a crashed write left behind. Returns how many sessions went.
    pub fn prune(&self, max_age: Duration) -> io::Result<usize> {
        let now = SystemTime::now();
        let mut pruned = 0;
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            if name.starts_with('.') && name.contains(".tmp.") {
                let _ = fs::remove_file(&path);
                continue;
            }
            let Some(id) = name.strip_suffix(".json") else {
                continue;
            };
            if !valid_session_id(id) {
                continue;
            }
            let age = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .unwrap_or_default();
            if age > max_age && fs::remove_file(&path).is_ok() {
                pruned += 1;
            }
        }
        Ok(pruned)
    }

    /// Records activity on a session by refreshing its file's mtime (at most
    /// once an hour per session), so pruning counts inactivity, not age.
    /// Never creates a file.
    pub fn touch(&self, id: &str) {
        let Some(path) = self.path_for(id) else {
            return;
        };
        {
            let mut touched = self.touched.lock().expect("session touch lock");
            let now = Instant::now();
            match touched.get(id) {
                Some(last) if now.duration_since(*last) < SESSION_TOUCH_INTERVAL => return,
                _ => {
                    touched.insert(id.to_string(), now);
                }
            }
        }
        if let Ok(file) = OpenOptions::new().write(true).open(&path) {
            let _ = file.set_modified(SystemTime::now());
        }
    }
}

#[async_trait::async_trait]
impl SessionStore for FileSessionStore {
    async fn load(&self, session_id: &str) -> Result<Option<SessionState>, SessionStoreError> {
        // An ID the store would never have written is simply unknown: rmcp
        // answers 404 and nothing outside `sessions/` is ever read.
        let Some(path) = self.path_for(session_id) else {
            return Ok(None);
        };
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(Box::new(error)),
        };
        match serde_json::from_slice::<SessionFile>(&bytes) {
            Ok(file) if file.session_id == session_id => {
                tracing::info!(session_id, "restoring persisted MCP session");
                Ok(Some(file.state))
            }
            Ok(_) | Err(_) => {
                tracing::warn!(session_id, path = %path.display(), "ignoring unreadable session file");
                Ok(None)
            }
        }
    }

    async fn store(&self, session_id: &str, state: &SessionState) -> Result<(), SessionStoreError> {
        let path = self.path_for(session_id).ok_or_else(|| {
            Box::new(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("refusing to persist session with invalid id {session_id:?}"),
            )) as SessionStoreError
        })?;
        let file = SessionFile {
            session_id: session_id.to_string(),
            created_at: now_rfc3339(),
            state: state.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&file)?;
        write_atomic(&path, &bytes)?;
        Ok(())
    }

    async fn delete(&self, session_id: &str) -> Result<(), SessionStoreError> {
        let Some(path) = self.path_for(session_id) else {
            return Ok(());
        };
        self.touched
            .lock()
            .expect("session touch lock")
            .remove(session_id);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(Box::new(error)),
        }
    }
}

// ---------------------------------------------------------------------------
// Transactions
// ---------------------------------------------------------------------------

/// Why a transaction key was tombstoned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LostCause {
    /// The daemon stopped (or crashed) while the transaction was open.
    Restart,
    /// The idle reaper force-aborted it.
    Idle,
}

/// A transaction the daemon dropped while its agent still believes it open.
/// Every write and commit resolved to its key is refused until the agent
/// acknowledges the loss with `iwe_tx_abort` (or begins afresh).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LostTransaction {
    pub key: String,
    pub cause: LostCause,
    /// RFC 3339, UTC.
    pub at: String,
    /// For [`LostCause::Idle`]: the idle timeout that expired, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_timeout_secs: Option<u64>,
}

impl LostTransaction {
    pub fn now(key: &str, cause: LostCause, idle_timeout_secs: Option<u64>) -> Self {
        Self {
            key: key.to_string(),
            cause,
            at: now_rfc3339(),
            idle_timeout_secs,
        }
    }

    pub fn recorded_at(&self) -> Option<DateTime<Utc>> {
        DateTime::parse_from_rfc3339(&self.at)
            .ok()
            .map(|t| t.with_timezone(&Utc))
    }

    /// True once the tombstone is older than [`TOMBSTONE_MAX_AGE`]. An
    /// unparseable timestamp never expires on its own: acknowledging it is
    /// the safe way out.
    pub fn expired(&self, now: DateTime<Utc>) -> bool {
        self.recorded_at()
            .and_then(|at| (now - at).to_std().ok())
            .is_some_and(|age| age > TOMBSTONE_MAX_AGE)
    }

    pub fn message(&self) -> String {
        match self.cause {
            LostCause::Restart => format!(
                "transaction {} was dropped by a daemon restart at {}; none of its staged writes were applied; call iwe_tx_abort to acknowledge, then begin again",
                self.key, self.at
            ),
            LostCause::Idle => format!(
                "transaction {} was force-aborted after {}s idle at {}; none of its staged writes were applied; call iwe_tx_abort to acknowledge, then begin again",
                self.key,
                self.idle_timeout_secs.unwrap_or_default(),
                self.at
            ),
        }
    }
}

/// `<state-dir>/open-transactions.json`: every transaction key open (or in
/// the middle of committing) right now, plus the standing tombstones.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct TxStateSnapshot {
    pub version: u32,
    pub open: Vec<String>,
    #[serde(default)]
    pub lost: Vec<LostTransaction>,
}

pub struct TxStateFile {
    path: PathBuf,
}

impl TxStateFile {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The snapshot on disk; an absent file is an empty one. A file that
    /// exists but cannot be parsed is an error: guessing would risk letting a
    /// lost transaction's writes through.
    pub fn load(&self) -> io::Result<TxStateSnapshot> {
        match fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "{} is not a valid transaction state file: {e}",
                        self.path.display()
                    ),
                )
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(TxStateSnapshot::default()),
            Err(error) => Err(error),
        }
    }

    pub fn save(&self, open: &HashSet<String>, lost: &[LostTransaction]) -> io::Result<()> {
        let mut open: Vec<String> = open.iter().cloned().collect();
        open.sort();
        let mut lost = lost.to_vec();
        lost.sort_by(|a, b| a.key.cmp(&b.key));
        let snapshot = TxStateSnapshot {
            version: 1,
            open,
            lost,
        };
        let bytes = serde_json::to_vec_pretty(&snapshot).map_err(io::Error::other)?;
        write_atomic(&self.path, &bytes)
    }
}

/// Creates `<state-dir>` (mode 0700) and returns the two stores inside it.
pub fn open_state_dir(state_dir: &Path) -> io::Result<(FileSessionStore, TxStateFile)> {
    create_private_dir(state_dir)?;
    let sessions = FileSessionStore::open(state_dir.join("sessions"))?;
    let txs = TxStateFile::new(state_dir.join("open-transactions.json"));
    Ok((sessions, txs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids_are_restricted_to_a_safe_alphabet() {
        assert!(valid_session_id("0b6c9f3e-8d1a-4f55-9b1e-2f1c7a0d9e11"));
        assert!(valid_session_id("abc_DEF-123"));
        assert!(valid_session_id(&"a".repeat(128)));
        for bad in [
            "",
            "../etc/passwd",
            "a/b",
            "a.json",
            ".hidden",
            "a b",
            "ä",
            "a\0b",
            &"a".repeat(129),
        ] {
            assert!(!valid_session_id(bad), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn atomic_writes_are_private_and_leave_no_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.json");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"two");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
    }

    #[test]
    fn invalid_ids_never_touch_the_filesystem() {
        let root = tempfile::tempdir().unwrap();
        let store = FileSessionStore::open(root.path().join("sessions")).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let state: SessionState = serde_json::from_value(serde_json::json!({
            "initialize_params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "t", "version": "0"}
            }
        }))
        .unwrap();
        runtime.block_on(async {
            assert!(store.store("../escape", &state).await.is_err());
            assert!(store.load("../escape").await.unwrap().is_none());
            store.delete("../escape").await.unwrap();
            store.store("good-id", &state).await.unwrap();
            assert!(store.load("good-id").await.unwrap().is_some());
            store.delete("good-id").await.unwrap();
            assert!(store.load("good-id").await.unwrap().is_none());
        });
        assert!(!root.path().join("escape.json").exists());
        assert_eq!(fs::read_dir(store.dir()).unwrap().count(), 0);
    }

    #[test]
    fn prune_removes_only_sessions_idle_past_the_limit() {
        let root = tempfile::tempdir().unwrap();
        let store = FileSessionStore::open(root.path().join("sessions")).unwrap();
        let old = store.dir().join("old.json");
        let fresh = store.dir().join("fresh.json");
        fs::write(&old, "{}").unwrap();
        fs::write(&fresh, "{}").unwrap();
        let eight_days_ago = SystemTime::now() - Duration::from_secs(8 * 24 * 60 * 60);
        File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(eight_days_ago)
            .unwrap();
        assert_eq!(store.prune(SESSION_MAX_AGE).unwrap(), 1);
        assert!(!old.exists());
        assert!(fresh.exists());
    }

    #[test]
    fn tombstones_expire_after_a_day() {
        let mut lost = LostTransaction::now("session:x", LostCause::Restart, None);
        assert!(!lost.expired(Utc::now()));
        lost.at = (Utc::now() - chrono::Duration::hours(25)).to_rfc3339();
        assert!(lost.expired(Utc::now()));
    }
}
