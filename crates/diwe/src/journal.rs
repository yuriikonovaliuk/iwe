//! An append-only transaction journal.
//!
//! On a successful commit, IWE can append one record noting which keys
//! that commit affected and how, to a configured journal file — useful for
//! building an audit trail, an undo/backup mechanism, or letting an
//! external tool (a search indexer, a sync process) learn what changed
//! without re-diffing content itself.
//!
//! IWE only ever *reports* into the journal after a write has already
//! landed; nothing reads the journal back, and nothing outside this module
//! is consulted before a commit is allowed to proceed. A journal write
//! failure — an unwritable path, a missing parent directory — is surfaced
//! as an ordinary warning and otherwise ignored: recording history is not
//! a condition of making the history, so a broken or absent journal must
//! never cost the caller its own write.
//!
//! With no journal path configured (the default), [`record_commit`] does
//! nothing at all, and IWE behaves exactly as it does without this module.

use std::io::Write as _;
use std::path::Path;

use serde::{Deserialize, Serialize};

use liwe::model::Key;

/// The kind of effect a single key experienced within a committed
/// transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effect {
    Create,
    Update,
    Delete,
}

/// One key's effect within a committed transaction's journal record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyEffect {
    pub key: String,
    pub effect: Effect,
}

impl KeyEffect {
    pub fn new(key: &Key, effect: Effect) -> Self {
        Self {
            key: key.to_string(),
            effect,
        }
    }
}

/// One journal record: a committed transaction's identifier, the keys it
/// affected and how, and this record's position in the journal.
///
/// Nothing else is carried — no content, no diff. Serializes to exactly
/// the pinned newline-delimited-JSON shape: `{"seq": <u64>, "tx":
/// "<uuid>", "effects": [{"key": "<doc key>", "effect":
/// "create"|"update"|"delete"}]}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub seq: u64,
    pub tx: String,
    pub effects: Vec<KeyEffect>,
}

/// Appends one record to the journal at `path`, for a just-committed
/// transaction that affected `effects`.
///
/// Does nothing when `path` is `None` (no `journal.path` configured, the
/// default) or `effects` is empty, so every caller can invoke this
/// unconditionally after a successful commit without checking
/// configuration itself.
///
/// A write failure is logged as a warning through IWE's ordinary logging
/// and otherwise swallowed: this function never returns an error, because
/// nothing about a caller's already-successful commit should be undone or
/// blocked by the journal failing to keep up. This is also why a record is
/// only ever appended here, from a call site that already knows its
/// commit succeeded — an aborted or rejected write must never reach this
/// function, which is what keeps the journal a log of what happened
/// rather than what was attempted.
pub fn record_commit(path: Option<&Path>, effects: Vec<KeyEffect>) {
    let Some(path) = path else {
        return;
    };
    if effects.is_empty() {
        return;
    }
    if let Err(error) = append(path, effects) {
        log::warn!(
            "failed to write transaction journal record to '{}': {}",
            path.display(),
            error
        );
    }
}

fn append(path: &Path, effects: Vec<KeyEffect>) -> std::io::Result<()> {
    let seq = next_seq(path)?;
    let record = Record {
        seq,
        tx: uuid::Uuid::new_v4().to_string(),
        effects,
    };
    let line = serde_json::to_string(&record).map_err(std::io::Error::other)?;

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{line}")?;
    file.flush()
}

/// The sequence number the next record must use: one past the last
/// record's `seq` already in the journal, or `1` if the journal doesn't
/// exist yet or holds no parseable record.
///
/// Derived from the journal file's own trailing content rather than a
/// separate counter kept in memory or in a sidecar file, so a freshly
/// started process picks up exactly where a prior process left off: a
/// restart must not reset the sequence back to `1` (colliding with
/// records already on disk) or otherwise lose continuity.
fn next_seq(path: &Path) -> std::io::Result<u64> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(1),
        Err(error) => return Err(error),
    };

    let last_seq = contents
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .and_then(|line| serde_json::from_str::<Record>(line).ok())
        .map(|record| record.seq);

    Ok(last_seq.map_or(1, |seq| seq + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;

    fn read_lines(path: &Path) -> Vec<serde_json::Value> {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn no_path_configured_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.ndjson");

        record_commit(None, vec![KeyEffect::new(&Key::name("a"), Effect::Create)]);

        assert!(!path.exists());
    }

    #[test]
    fn empty_effects_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.ndjson");

        record_commit(Some(&path), vec![]);

        assert!(!path.exists());
    }

    #[test]
    fn appends_one_record_in_the_pinned_shape() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.ndjson");

        record_commit(
            Some(&path),
            vec![
                KeyEffect::new(&Key::name("a"), Effect::Create),
                KeyEffect::new(&Key::name("b"), Effect::Delete),
            ],
        );

        let lines = read_lines(&path);
        assert_eq!(lines.len(), 1);
        let record = &lines[0];
        assert_eq!(record["seq"], 1);
        assert!(record["tx"].as_str().unwrap().len() > 0);
        assert_eq!(
            record["effects"],
            serde_json::json!([
                {"key": "a", "effect": "create"},
                {"key": "b", "effect": "delete"},
            ])
        );
        // Nothing else is carried on the record.
        let obj = record.as_object().unwrap();
        assert_eq!(obj.len(), 3);
        for effect in record["effects"].as_array().unwrap() {
            assert_eq!(effect.as_object().unwrap().len(), 2);
        }
    }

    #[test]
    fn seq_is_monotonic_across_calls_and_a_simulated_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.ndjson");

        record_commit(Some(&path), vec![KeyEffect::new(&Key::name("a"), Effect::Create)]);
        record_commit(Some(&path), vec![KeyEffect::new(&Key::name("b"), Effect::Create)]);

        // A "restart" here is nothing but a fresh call after the file
        // already has records on disk — there is no in-memory counter to
        // reset, so this already exercises the cross-restart case.
        record_commit(Some(&path), vec![KeyEffect::new(&Key::name("c"), Effect::Update)]);

        let lines = read_lines(&path);
        let seqs: Vec<u64> = lines.iter().map(|l| l["seq"].as_u64().unwrap()).collect();
        assert_eq!(seqs, vec![1, 2, 3]);
    }

    #[test]
    fn unwritable_path_warns_and_does_not_error_the_caller() {
        let dir = tempfile::tempdir().unwrap();
        let readonly_dir = dir.path().join("readonly");
        fs::create_dir(&readonly_dir).unwrap();
        let mut perms = fs::metadata(&readonly_dir).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(&readonly_dir, perms.clone()).unwrap();

        let path = readonly_dir.join("journal.ndjson");

        // Must not panic; must not return anything the caller could fail
        // on (there is nothing to return at all).
        record_commit(Some(&path), vec![KeyEffect::new(&Key::name("a"), Effect::Create)]);

        assert!(!path.exists());

        // restore so tempdir cleanup can remove it
        perms.set_readonly(false);
        fs::set_permissions(&readonly_dir, perms).unwrap();
    }
}
