//! T7: a [`Transaction`] backend that validates schema rules at `commit()`
//! time — over an index-backed affected set, or over the whole store.
//!
//! This is a non-default backend — [`liwe::transaction::NoopTransaction`]
//! stays the default, untouched write-permission passthrough every canonical
//! write path routes through unless `[transactions] validate` in the
//! configuration says otherwise (T3/T5/T10/T11's job: freeze /
//! immutability, not schema shape). [`ValidatingTransaction`] is a
//! different, separate mechanism: it does not check write permission at
//! all (every `write()` call succeeds), and instead checks, once at
//! `commit()`, whether the transaction's *final* state satisfies the schema
//! rules — the index-bounded link rules over the documents that state's
//! writes could have affected ([`ValidationScope::AffectedSet`], see
//! [`crate::schema::validate_affected_set`]), or everything `iwe schema
//! validate` checks ([`ValidationScope::Full`]: every schema, the
//! `[invariants]`, and the `always` checkers over the touched keys).
//!
//! Validating only at `commit()`, and only the final state, is deliberate:
//! a multi-write transaction may legitimately pass through invalid
//! intermediate states on its way to a valid final one
//! (`m2/design-transactions`). This backend never inspects the state after
//! write 1 of a 2-write transaction — only the state after every write
//! recorded since `begin()` has been applied.
//!
//! Two more things a commit guarantees, both for several writers sharing
//! one store (the compositor's materialized tree, one MCP server per
//! agent):
//!
//! - **Isolation.** The commit — conflict check, validation, application —
//!   runs under an exclusive lock on `<.iwe>/write.lock`, so two commits
//!   never validate against the same pre-state and both land. The lock is
//!   the `.iwe` directory the schemas live in; a store without one (an
//!   in-memory test) commits unlocked.
//! - **Conflict detection.** The first `write()` naming a key records what
//!   that key held on disk at that moment; `commit()` refuses with
//!   [`ValidationFailure::Conflict`] if any staged key has since changed
//!   underneath the transaction — the optimistic check a long-lived agent
//!   transaction needs, since between its `begin()` and its `commit()`
//!   other agents keep writing.

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use liwe::graph::Graph;
use liwe::model::config::Format;
use liwe::model::{Key, State};
use liwe::operations::Changes;
use liwe::transaction::{CommitError, Transaction, Write, WriteRejected};

use crate::config::Configuration;
pub use crate::config::ValidationScope;
use crate::fs::{new_for_path, write_file};
use crate::permissions::WriteOperation;
use crate::schema::{
    render_reports_text, run_checkers, validate_affected_set, validate_store_at, KeyReport,
    ValidationRun,
};
use liwe::schema::Violation;

/// The name of the store-wide commit lock, inside the `.iwe` directory.
pub const WRITE_LOCK_FILE: &str = "write.lock";

/// Why a [`ValidatingTransaction::commit`] failed, beyond the
/// [`CommitError::Failed`] state every [`Transaction`] shares.
#[derive(Debug)]
pub enum ValidationFailure {
    /// The schema configuration itself (a `.iwe/schemas/*.yaml` file, or
    /// the `[schemas]` bindings in `config.toml`) could not be compiled.
    Config(Vec<String>),
    /// The transaction's final state violates the schema rules checked at
    /// commit. None of this transaction's writes were applied (or, for a
    /// checker failure found after application, all were reverted).
    Violations(ValidationRun),
    /// A staged key changed on disk between the transaction's first write
    /// to it and its commit. Nothing was applied; the caller re-reads and
    /// retries.
    Conflict(Vec<Key>),
    /// A filesystem failure while locking, reading the current on-disk
    /// state, or writing the transaction's changes.
    Io(std::io::Error),
}

/// The name this failure type had while the backend only knew the
/// affected-set scope.
pub type AffectedSetError = ValidationFailure;

impl fmt::Display for ValidationFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(errors) => {
                write!(f, "schema configuration error: {}", errors.join("; "))
            }
            Self::Violations(run) => {
                write!(
                    f,
                    "schema validation failed:\n{}",
                    render_reports_text(&run.reports).trim_end()
                )
            }
            Self::Conflict(keys) => {
                let listed: Vec<String> = keys.iter().map(|k| k.to_string()).collect();
                write!(
                    f,
                    "write conflict: changed on disk since this transaction read them: {}",
                    listed.join(", ")
                )
            }
            Self::Io(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for ValidationFailure {}

/// A [`Transaction`] backend that performs schema validation at `commit()`
/// time (T7).
///
/// `write()` always succeeds — this backend is not a permission gate.
/// `commit()` builds the state that would result from applying every write
/// recorded since `begin()` on top of what is currently on disk and
/// validates it to the configured [`ValidationScope`]. If it is clean,
/// every pending write is applied to disk and the transaction succeeds; if
/// not, nothing is written and `commit()` returns the failure.
pub struct ValidatingTransaction {
    base_path: PathBuf,
    format: Format,
    config: Configuration,
    schemas_dir: PathBuf,
    scope: ValidationScope,
    checker_root: PathBuf,
    pending: Vec<Write>,
    /// What each staged key held on disk when this transaction first
    /// wrote it (`None`: no file). The conflict baseline.
    baseline: BTreeMap<Key, Option<u64>>,
    failed: bool,
}

/// The name this backend had while it only knew the affected-set scope;
/// [`ValidatingTransaction::new`] still defaults to that scope.
pub type AffectedSetTransaction = ValidatingTransaction;

fn digest(content: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    hasher.finish()
}

/// An exclusive advisory lock on `path`, released on drop.
struct StoreLock(#[allow(dead_code)] File);

impl StoreLock {
    fn acquire(path: &Path) -> std::io::Result<Self> {
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            // SAFETY: flock on a file descriptor this process owns.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(Self(file))
    }
}

#[cfg(unix)]
impl Drop for StoreLock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        // SAFETY: releasing the lock this struct acquired; the descriptor
        // closes right after.
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

impl ValidatingTransaction {
    /// A backend over the store at `base_path`, validating to the
    /// affected-set scope; see [`Self::with_scope`] for the full one.
    pub fn new(
        base_path: impl Into<PathBuf>,
        format: Format,
        config: Configuration,
        schemas_dir: impl Into<PathBuf>,
    ) -> Self {
        let base_path = base_path.into();
        Self {
            checker_root: base_path.clone(),
            base_path,
            format,
            config,
            schemas_dir: schemas_dir.into(),
            scope: ValidationScope::AffectedSet,
            pending: Vec::new(),
            baseline: BTreeMap::new(),
            failed: false,
        }
    }

    /// The scope validated at commit. [`ValidationScope::None`] is treated
    /// as the affected set — a backend that validates nothing is
    /// [`liwe::transaction::NoopTransaction`], not this one.
    pub fn with_scope(mut self, scope: ValidationScope) -> Self {
        self.scope = scope;
        self
    }

    /// The directory the `always` checkers run in (their `root`), when
    /// the library does not sit at the project root. Defaults to
    /// `base_path`.
    pub fn with_checker_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.checker_root = root.into();
        self
    }

    pub fn scope(&self) -> ValidationScope {
        self.scope
    }

    /// The backend `[transactions] validate` asks for over the store at
    /// `base_path` inside the project at `root` (where `.iwe/` lives), or
    /// `None` when the section is left at its default (`none`) — the
    /// caller then stays on [`liwe::transaction::NoopTransaction`]. Built
    /// fresh per write: the backend is cheap, and its conflict baseline
    /// must start empty. The one construction both binaries share, so a
    /// store gated for the MCP server is gated for the CLI too.
    pub fn for_config(config: &Configuration, base_path: &Path, root: &Path) -> Option<Self> {
        let scope = config.transactions.validate;
        if scope == ValidationScope::None {
            return None;
        }
        Some(
            Self::new(
                base_path,
                config.format,
                config.clone(),
                crate::config::schemas_dir_in(root),
            )
            .with_scope(scope)
            .with_checker_root(root),
        )
    }

    /// The writes recorded on this transaction since the last `begin`,
    /// `commit`, or `abort`.
    pub fn pending(&self) -> &[Write] {
        &self.pending
    }

    /// Commits, turning a refusal — violations, a write conflict, a
    /// configuration error — into the message the caller shows, and
    /// aborting so the transaction is reusable.
    pub fn commit_or_abort(&mut self) -> Result<(), String> {
        match self.commit() {
            Ok(()) => Ok(()),
            Err(CommitError::Failed) => {
                let _ = self.abort();
                Err("write rejected: transaction is in the failed state".to_string())
            }
            Err(CommitError::Other(failure)) => {
                let _ = self.abort();
                Err(format!("write rejected: {failure}"))
            }
        }
    }

    /// One document written through this backend: staged, permission-
    /// checked inside the bracket (`check` sees the prior on-disk content,
    /// `None` for a create), and landed by `commit()` — the store is
    /// touched inside the backend's lock and nowhere else, so the caller
    /// must not write the file itself afterwards.
    pub fn put_one(
        mut self,
        key: &Key,
        content: &str,
        check: impl FnOnce(Option<&str>) -> Result<(), String>,
    ) -> Result<(), String> {
        self.begin()
            .map_err(|_| format!("transaction backend failed to begin for '{key}'"))?;
        if self
            .write(Write::Put(key.clone(), content.to_string()))
            .is_err()
        {
            let _ = self.abort();
            return Err(format!("write rejected by transaction backend for '{key}'"));
        }
        let prior = self.on_disk(key);
        if let Err(message) = check(prior.as_deref()) {
            let _ = self.abort();
            return Err(message);
        }
        self.commit_or_abort()
    }

    /// One transaction over the whole of `changes` — removes, creates and
    /// updates staged together and validated as one final state, so a
    /// rename or extract whose intermediate states dangle is judged on
    /// where it ends up (`m2/design-transactions`). Write permission is
    /// checked per key inside the bracket, as [`crate::fs::apply_changes`]
    /// does: a removal is checked with `""` as its outgoing content and
    /// [`WriteOperation::Delete`], and only when the file exists.
    pub fn apply_changes(
        mut self,
        changes: &Changes,
        check: impl Fn(&Key, &str, Option<&str>, WriteOperation) -> Result<(), String>,
    ) -> Result<(), String> {
        self.begin()
            .map_err(|_| "transaction backend failed to begin".to_string())?;
        for key in &changes.removes {
            if self.write(Write::Remove(key.clone())).is_err() {
                let _ = self.abort();
                return Err(format!("write rejected by transaction backend for '{key}'"));
            }
            if let Some(existing) = self.on_disk(key) {
                if let Err(message) = check(key, "", Some(&existing), WriteOperation::Delete) {
                    let _ = self.abort();
                    return Err(message);
                }
            }
        }
        for (key, markdown) in changes.creates.iter().chain(changes.updates.iter()) {
            if self
                .write(Write::Put(key.clone(), markdown.clone()))
                .is_err()
            {
                let _ = self.abort();
                return Err(format!("write rejected by transaction backend for '{key}'"));
            }
            let prior = self.on_disk(key);
            if let Err(message) = check(key, markdown, prior.as_deref(), WriteOperation::Write) {
                let _ = self.abort();
                return Err(message);
            }
        }
        self.commit_or_abort()
    }

    fn file_path(&self, key: &Key) -> PathBuf {
        self.base_path.join(key.to_path(self.format))
    }

    fn on_disk(&self, key: &Key) -> Option<String> {
        fs::read_to_string(self.file_path(key)).ok()
    }

    /// Where the store-wide commit lock lives: next to the schemas, in
    /// `.iwe`. `None` when the schemas directory has no parent to lock in.
    fn lock_path(&self) -> Option<PathBuf> {
        let dir = self.schemas_dir.parent()?;
        dir.is_dir().then(|| dir.join(WRITE_LOCK_FILE))
    }

    /// The state that would result from applying every pending write on
    /// top of what is currently on disk at `base_path`.
    fn final_state(&self) -> State {
        let mut state = new_for_path(&self.base_path, self.format);
        for write in &self.pending {
            match write {
                Write::Put(key, content) => {
                    state.insert(key.as_str().to_string(), content.clone());
                }
                Write::Remove(key) => {
                    state.remove(key.as_str());
                }
            }
        }
        state
    }

    /// The distinct keys this transaction's pending writes name.
    fn touched_keys(&self) -> Vec<Key> {
        let mut keys: Vec<Key> = Vec::new();
        for write in &self.pending {
            let key = match write {
                Write::Put(key, _) => key.clone(),
                Write::Remove(key) => key.clone(),
            };
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
        keys
    }

    /// The staged keys whose on-disk content no longer matches what this
    /// transaction saw when it first wrote them.
    fn conflicts(&self) -> Vec<Key> {
        self.baseline
            .iter()
            .filter(|(key, seen)| self.on_disk(key).as_deref().map(digest) != **seen)
            .map(|(key, _)| key.clone())
            .collect()
    }

    fn apply_pending(&self) -> std::io::Result<()> {
        for write in &self.pending {
            match write {
                Write::Put(key, content) => {
                    let file_path = self.file_path(key);
                    if let Some(parent) = file_path.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    write_file(
                        &key.as_str().to_string(),
                        content,
                        &self.base_path,
                        self.format,
                    )?;
                }
                Write::Remove(key) => {
                    let file_path = self.file_path(key);
                    if file_path.exists() {
                        fs::remove_file(&file_path)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Puts every touched key back to `prior` — the compensating move
    /// when a checker rejects a state that was already applied.
    fn restore(&self, prior: &BTreeMap<Key, Option<String>>) -> std::io::Result<()> {
        for (key, content) in prior {
            let file_path = self.file_path(key);
            match content {
                Some(content) => {
                    if let Some(parent) = file_path.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::write(&file_path, content)?;
                }
                None => {
                    if file_path.exists() {
                        fs::remove_file(&file_path)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// The reports validation produces for `state`, to this transaction's
    /// scope.
    fn reports_for(&self, state: &State, touched: &[Key]) -> Result<Vec<KeyReport>, Vec<String>> {
        let graph = Graph::from_state(
            state,
            false,
            self.config.format_options(),
            self.config.library.frontmatter_document_title.clone(),
        );
        let run = match self.scope {
            ValidationScope::Full => validate_store_at(&self.schemas_dir, &self.config, &graph)?,
            ValidationScope::AffectedSet | ValidationScope::None => {
                validate_affected_set(&self.schemas_dir, &self.config, &graph, touched)
                    .map(|(run, _affected)| run)?
            }
        };
        Ok(run.reports)
    }

    /// Refuses the final state if it violates anything the current state
    /// does not. A violation that already stands on disk — someone else's
    /// debt, or a rule that tightened after the document was written — is
    /// not this transaction's to pay, and must not turn every unrelated
    /// write into a refusal; a violation this transaction would introduce
    /// is. The pre-state is validated only when the final state has
    /// reports at all, so a clean write costs one validation.
    fn validate_final_state(&self, touched: &[Key]) -> Result<(), ValidationFailure> {
        let after = self
            .reports_for(&self.final_state(), touched)
            .map_err(ValidationFailure::Config)?;
        if after.is_empty() {
            return Ok(());
        }
        let before = self
            .reports_for(&new_for_path(&self.base_path, self.format), touched)
            .map_err(ValidationFailure::Config)?;
        let standing: HashSet<(String, String, String)> = before
            .iter()
            .flat_map(|report| {
                report.violations.iter().map(move |violation| {
                    (
                        report.key.to_string(),
                        report.schema.clone(),
                        violation.message.clone(),
                    )
                })
            })
            .collect();
        let introduced: Vec<KeyReport> = after
            .into_iter()
            .filter_map(|report| {
                let violations: Vec<Violation> = report
                    .violations
                    .into_iter()
                    .filter(|violation| {
                        !standing.contains(&(
                            report.key.to_string(),
                            report.schema.clone(),
                            violation.message.clone(),
                        ))
                    })
                    .collect();
                (!violations.is_empty()).then(|| KeyReport {
                    key: report.key,
                    schema: report.schema,
                    violations,
                })
            })
            .collect();
        if introduced.is_empty() {
            Ok(())
        } else {
            Err(ValidationFailure::Violations(ValidationRun {
                documents: introduced.len(),
                schemas: 0,
                reports: introduced,
            }))
        }
    }

    /// The `always` checkers over the touched keys, against the applied
    /// state. Only the reports configured to fail count; warnings are the
    /// CLI's to print.
    fn failing_checker_reports(&self, touched: &[Key]) -> Option<ValidationRun> {
        if self.scope != ValidationScope::Full || self.config.checkers.is_empty() {
            return None;
        }
        let checked = run_checkers(&self.config, &self.checker_root, touched, false);
        if checked.failing.is_empty() {
            return None;
        }
        Some(ValidationRun {
            documents: touched.len(),
            schemas: 0,
            reports: checked.failing,
        })
    }

    fn commit_locked(&mut self) -> Result<(), ValidationFailure> {
        let conflicts = self.conflicts();
        if !conflicts.is_empty() {
            return Err(ValidationFailure::Conflict(conflicts));
        }

        let touched = self.touched_keys();
        self.validate_final_state(&touched)?;

        let prior: BTreeMap<Key, Option<String>> = touched
            .iter()
            .map(|key| (key.clone(), self.on_disk(key)))
            .collect();
        self.apply_pending().map_err(ValidationFailure::Io)?;

        if let Some(run) = self.failing_checker_reports(&touched) {
            self.restore(&prior).map_err(ValidationFailure::Io)?;
            return Err(ValidationFailure::Violations(run));
        }
        Ok(())
    }
}

impl Transaction for ValidatingTransaction {
    type Error = ValidationFailure;

    fn begin(&mut self) -> Result<(), Self::Error> {
        self.pending.clear();
        self.baseline.clear();
        self.failed = false;
        Ok(())
    }

    fn write(&mut self, write: Write) -> Result<(), WriteRejected<Self::Error>> {
        let key = match &write {
            Write::Put(key, _) | Write::Remove(key) => key.clone(),
        };
        if !self.baseline.contains_key(&key) {
            let seen = self.on_disk(&key).as_deref().map(digest);
            self.baseline.insert(key, seen);
        }
        self.pending.push(write);
        Ok(())
    }

    fn commit(&mut self) -> Result<(), CommitError<Self::Error>> {
        if self.failed {
            return Err(CommitError::Failed);
        }
        if self.pending.is_empty() {
            self.baseline.clear();
            return Ok(());
        }

        let lock = match self.lock_path() {
            Some(path) => match StoreLock::acquire(&path) {
                Ok(lock) => Some(lock),
                Err(error) => return Err(CommitError::Other(ValidationFailure::Io(error))),
            },
            None => None,
        };
        let result = self.commit_locked();
        drop(lock);

        match result {
            Ok(()) => {
                self.pending.clear();
                self.baseline.clear();
                Ok(())
            }
            Err(failure) => Err(CommitError::Other(failure)),
        }
    }

    fn abort(&mut self) -> Result<(), Self::Error> {
        self.pending.clear();
        self.baseline.clear();
        self.failed = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs::{create_dir_all, read_to_string, write};

    use tempfile::TempDir;

    use crate::config::{Patterns, SchemaBinding};

    fn write_schema(dir: &std::path::Path, name: &str, source: &str) {
        let schemas = dir.join(".iwe").join("schemas");
        create_dir_all(&schemas).unwrap();
        write(schemas.join(format!("{name}.yaml")), source).unwrap();
    }

    fn config_with(entries: &[(&str, &str)]) -> Configuration {
        Configuration {
            schemas: entries
                .iter()
                .map(|(name, pattern)| {
                    (
                        name.to_string(),
                        SchemaBinding {
                            r#match: Patterns::One(pattern.to_string()),
                        },
                    )
                })
                .collect(),
            ..Default::default()
        }
    }

    fn transaction_for(temp: &TempDir, config: Configuration) -> AffectedSetTransaction {
        AffectedSetTransaction::new(
            temp.path().to_path_buf(),
            Format::Markdown,
            config,
            temp.path().join(".iwe").join("schemas"),
        )
    }

    /// Test A: a two-write transaction whose intermediate state (after
    /// write 1) violates a schema rule, and whose final state (after write
    /// 2, same key) resolves it. `commit()` on the new backend succeeds
    /// because only the final state is checked. Then the SAME first write,
    /// committed alone as its own single-write transaction, is rejected by
    /// the same backend — the per-write-vs-at-commit difference made
    /// empirically visible, not just asserted in a comment.
    #[test]
    fn test_a_intermediate_state_permitted_same_write_alone_rejected() {
        let temp = TempDir::new().unwrap();
        write_schema(temp.path(), "note", "links:\n  - min: 1\n");
        create_dir_all(temp.path().join("notes")).unwrap();
        write(temp.path().join("notes/b.md"), "# B\n").unwrap();

        let config = config_with(&[("note", "notes/**")]);
        let mut tx = transaction_for(&temp, config);

        let invalid_content = "# A\n\nno links here\n".to_string();
        let valid_content = "# A\n\nSee [B](b).\n".to_string();

        // --- Multi-write transaction: write 1 is individually invalid,
        // write 2 (same key) resolves it. Only the final state is checked.
        tx.begin().unwrap();
        tx.write(Write::Put(Key::name("notes/a"), invalid_content.clone()))
            .unwrap();
        tx.write(Write::Put(Key::name("notes/a"), valid_content.clone()))
            .unwrap();
        let multi_write_result = tx.commit();
        println!("Test A, multi-write transaction commit result: {multi_write_result:?}");
        assert!(
            multi_write_result.is_ok(),
            "commit should succeed: the final state (write 2) is valid, even though the \
             intermediate state (write 1 alone) was not"
        );
        assert_eq!(
            read_to_string(temp.path().join("notes/a.md")).unwrap(),
            valid_content,
            "the committed file should hold the final (write 2) content"
        );

        // --- Same first write, alone, as its own single-write transaction.
        tx.begin().unwrap();
        tx.write(Write::Put(Key::name("notes/a"), invalid_content.clone()))
            .unwrap();
        let single_write_result = tx.commit();
        println!("Test A, single-write transaction (write 1 alone) commit result: {single_write_result:?}");
        assert!(
            matches!(
                single_write_result,
                Err(CommitError::Other(AffectedSetError::Violations(_)))
            ),
            "committing write 1 alone should be rejected: for THIS transaction, write 1's \
             content is the final state, and it violates the min-links rule"
        );
        assert_eq!(
            read_to_string(temp.path().join("notes/a.md")).unwrap(),
            valid_content,
            "the rejected commit must not have overwritten the file on disk"
        );
    }

    /// Test B: a transaction whose final state (after all its writes) is
    /// invalid is rejected at `commit()`, and none of its writes land on
    /// disk — verified by reading the filesystem, not just the return
    /// value.
    #[test]
    fn test_b_invalid_final_state_rejected_nothing_lands_on_disk() {
        let temp = TempDir::new().unwrap();
        write_schema(temp.path(), "note", "links:\n  - min: 1\n");
        create_dir_all(temp.path().join("logs")).unwrap();

        let config = config_with(&[("note", "logs/**")]);
        let mut tx = transaction_for(&temp, config);

        tx.begin().unwrap();
        tx.write(Write::Put(
            Key::name("logs/x"),
            "# X\n\nno links\n".to_string(),
        ))
        .unwrap();
        tx.write(Write::Put(
            Key::name("logs/y"),
            "# Y\n\nalso no links\n".to_string(),
        ))
        .unwrap();

        let result = tx.commit();
        println!("Test B, commit result: {result:?}");
        assert!(matches!(
            result,
            Err(CommitError::Other(AffectedSetError::Violations(_)))
        ));
        if let Err(CommitError::Other(AffectedSetError::Violations(run))) = &result {
            println!("Test B, violation reports: {:?}", run.reports);
            let keys: Vec<_> = run.reports.iter().map(|r| r.key.to_string()).collect();
            assert!(keys.contains(&"logs/x".to_string()));
            assert!(keys.contains(&"logs/y".to_string()));
        }

        assert!(
            !temp.path().join("logs/x.md").exists(),
            "logs/x.md must not have been written to disk"
        );
        assert!(
            !temp.path().join("logs/y.md").exists(),
            "logs/y.md must not have been written to disk"
        );
    }

    /// The direct-link (one-hop, RefIndex-backed) affected-set closure is
    /// real: removing a document that another document links to is
    /// rejected because the *referrer* (never itself touched) is pulled
    /// into the affected set and its "no such document" check fails.
    #[test]
    fn direct_link_closure_pulls_in_referrer_and_rejects_removal() {
        let temp = TempDir::new().unwrap();
        write_schema(temp.path(), "note", "links:\n  - min: 1\n");
        create_dir_all(temp.path().join("hubs")).unwrap();
        create_dir_all(temp.path().join("assets")).unwrap();
        write(
            temp.path().join("hubs/hub.md"),
            "# Hub\n\nSee [Target](../assets/target).\n",
        )
        .unwrap();
        write(temp.path().join("assets/target.md"), "# Target\n").unwrap();

        // "assets/**" is deliberately not schema-bound: only "hubs/**" is.
        let config = config_with(&[("note", "hubs/**")]);
        let mut tx = transaction_for(&temp, config);

        tx.begin().unwrap();
        tx.write(Write::Remove(Key::name("assets/target"))).unwrap();
        let result = tx.commit();
        println!("Direct-link closure test, commit result: {result:?}");

        match &result {
            Err(CommitError::Other(AffectedSetError::Violations(run))) => {
                println!(
                    "Direct-link closure test, violation reports: {:?}",
                    run.reports
                );
                assert!(
                    run.reports.iter().any(|r| r.key == Key::name("hubs/hub")),
                    "hubs/hub was never touched by this transaction, but it links to the \
                     removed key — it should have been pulled into the affected set via \
                     RefIndex and rejected for a dangling link"
                );
            }
            other => panic!("expected the removal to be rejected, got: {other:?}"),
        }
        assert!(
            temp.path().join("assets/target.md").exists(),
            "the rejected commit must not have removed the file on disk"
        );
    }

    /// The `reach`-rule (transitive-to-fixpoint, ViaWalk::inbound-backed)
    /// affected-set closure is real: breaking a link in the middle of a
    /// genus chain is rejected because a document further up the chain
    /// (never itself touched) is pulled into the affected set and its
    /// `reach` check fails.
    #[test]
    fn reach_closure_pulls_in_transitive_referrer_and_rejects_broken_chain() {
        let temp = TempDir::new().unwrap();
        write_schema(
            temp.path(),
            "concept",
            "links:\n  - within: Is a\n    min: 1\n    max: 1\n    reach: root/entity\n",
        );
        create_dir_all(temp.path().join("concepts")).unwrap();
        create_dir_all(temp.path().join("root")).unwrap();
        write(temp.path().join("root/entity.md"), "# Entity\n").unwrap();
        write(
            temp.path().join("concepts/mid.md"),
            "# Mid\n\n## Is a\n\n- [Entity](../root/entity)\n",
        )
        .unwrap();
        write(
            temp.path().join("concepts/leaf.md"),
            "# Leaf\n\n## Is a\n\n- [Mid](mid)\n",
        )
        .unwrap();

        let config = config_with(&[("concept", "concepts/**")]);
        let mut tx = transaction_for(&temp, config);

        // Redirect mid's genus link away from entity, breaking the chain
        // both for mid itself and for leaf, which reaches entity only
        // through mid.
        tx.begin().unwrap();
        tx.write(Write::Put(
            Key::name("concepts/mid"),
            "# Mid\n\n## Is a\n\n- [Stub](stub)\n".to_string(),
        ))
        .unwrap();
        let result = tx.commit();
        println!("Reach closure test, commit result: {result:?}");

        match &result {
            Err(CommitError::Other(AffectedSetError::Violations(run))) => {
                println!("Reach closure test, violation reports: {:?}", run.reports);
                assert!(
                    run.reports
                        .iter()
                        .any(|r| r.key == Key::name("concepts/leaf")),
                    "concepts/leaf was never touched by this transaction, but it reaches the \
                     touched key through the 'Is a' scope — it should have been pulled into \
                     the affected set via ViaWalk::inbound and rejected for a broken chain"
                );
            }
            other => panic!("expected the broken chain to be rejected, got: {other:?}"),
        }
        assert_eq!(
            read_to_string(temp.path().join("concepts/mid.md")).unwrap(),
            "# Mid\n\n## Is a\n\n- [Entity](../root/entity)\n",
            "the rejected commit must not have overwritten the file on disk"
        );
    }

    /// Full scope: a shape rule (`properties` on the frontmatter) that the
    /// affected-set scope never looks at is enforced, and a store-wide
    /// `[invariants]` count is enforced too — a write that is fine on its
    /// own but pushes the store over an invariant is refused.
    #[test]
    fn full_scope_enforces_shape_rules_and_invariants() {
        use crate::config::Invariant;

        let temp = TempDir::new().unwrap();
        write_schema(
            temp.path(),
            "note",
            "frontmatter:\n  type: object\n  required: [type]\n  properties:\n    type: { const: note }\n",
        );
        create_dir_all(temp.path().join("notes")).unwrap();

        let mut config = config_with(&[("note", "notes/**")]);
        config.invariants.insert(
            "one-hub".to_string(),
            Invariant {
                filter: "role: hub".to_string(),
                expect: toml::Value::Integer(1),
                description: None,
            },
        );

        // Shape: missing `type` is rejected under full scope ...
        let mut full = transaction_for(&temp, config.clone()).with_scope(ValidationScope::Full);
        full.begin().unwrap();
        full.write(Write::Put(Key::name("notes/a"), "# A\n".to_string()))
            .unwrap();
        let result = full.commit();
        println!("full scope, shape violation: {result:?}");
        assert!(matches!(
            result,
            Err(CommitError::Other(ValidationFailure::Violations(_)))
        ));
        assert!(!temp.path().join("notes/a.md").exists());

        // ... and accepted by the affected-set scope, which only knows
        // link rules.
        let mut bounded = transaction_for(&temp, config.clone());
        bounded.begin().unwrap();
        bounded
            .write(Write::Put(Key::name("notes/a"), "# A\n".to_string()))
            .unwrap();
        assert!(bounded.commit().is_ok());
        fs::remove_file(temp.path().join("notes/a.md")).unwrap();

        // Invariant: with the hub present the count is 1 and a valid
        // note commits; removing the hub breaks the invariant and is
        // refused even though the removed document itself is bound to no
        // rule that fails.
        write(
            temp.path().join("notes/hub.md"),
            "---\ntype: note\nrole: hub\n---\n# Hub\n",
        )
        .unwrap();
        full.begin().unwrap();
        full.write(Write::Put(
            Key::name("notes/a"),
            "---\ntype: note\n---\n# A\n".to_string(),
        ))
        .unwrap();
        let result = full.commit();
        println!("full scope, valid note with invariant satisfied: {result:?}");
        assert!(result.is_ok());

        full.begin().unwrap();
        full.write(Write::Remove(Key::name("notes/hub"))).unwrap();
        let result = full.commit();
        println!("full scope, invariant broken by removal: {result:?}");
        match &result {
            Err(CommitError::Other(ValidationFailure::Violations(run))) => {
                assert!(run
                    .reports
                    .iter()
                    .any(|r| r.key == Key::name("invariants/one-hub")));
            }
            other => panic!("expected the invariant to refuse the removal, got {other:?}"),
        }
        assert!(temp.path().join("notes/hub.md").exists());
    }

    /// A violation already standing on disk is not this transaction's to
    /// pay: an unrelated write commits over it, while a write that
    /// introduces a violation of its own is still refused — and the
    /// refusal names only the introduced one.
    #[test]
    fn standing_violations_do_not_block_unrelated_writes() {
        let temp = TempDir::new().unwrap();
        write_schema(temp.path(), "note", "links:\n  - min: 1\n");
        create_dir_all(temp.path().join("notes")).unwrap();
        // Someone else's debt: a note with no links.
        write(temp.path().join("notes/debt.md"), "# Debt\n").unwrap();
        write(temp.path().join("notes/b.md"), "# B\n\nSee [Debt](debt).\n").unwrap();

        let mut tx = transaction_for(&temp, config_with(&[("note", "notes/**")]))
            .with_scope(ValidationScope::Full);
        tx.begin().unwrap();
        tx.write(Write::Put(
            Key::name("notes/a"),
            "# A\n\nSee [B](b).\n".to_string(),
        ))
        .unwrap();
        let result = tx.commit();
        println!("standing violation, unrelated write: {result:?}");
        assert!(
            result.is_ok(),
            "the standing violation on notes/debt is not this write's"
        );
        assert!(temp.path().join("notes/a.md").exists());

        tx.begin().unwrap();
        tx.write(Write::Put(Key::name("notes/c"), "# C\n".to_string()))
            .unwrap();
        let result = tx.commit();
        println!("standing violation plus an introduced one: {result:?}");
        match &result {
            Err(CommitError::Other(ValidationFailure::Violations(run))) => {
                let keys: Vec<String> = run.reports.iter().map(|r| r.key.to_string()).collect();
                assert_eq!(
                    keys,
                    vec!["notes/c".to_string()],
                    "only the introduced violation is reported"
                );
            }
            other => panic!("expected the introduced violation to be refused, got {other:?}"),
        }
        assert!(!temp.path().join("notes/c.md").exists());
    }

    /// Conflict detection: a key staged by this transaction and then
    /// changed on disk by someone else before commit refuses the commit,
    /// names the key, and leaves the other writer's content in place.
    #[test]
    fn commit_refuses_when_a_staged_key_changed_underneath() {
        let temp = TempDir::new().unwrap();
        write_schema(temp.path(), "note", "links:\n  - min: 0\n");
        create_dir_all(temp.path().join("notes")).unwrap();
        write(temp.path().join("notes/a.md"), "# A\n").unwrap();

        let mut tx = transaction_for(&temp, config_with(&[("note", "notes/**")]));
        tx.begin().unwrap();
        tx.write(Write::Put(Key::name("notes/a"), "# A, mine\n".to_string()))
            .unwrap();

        // Another writer lands first.
        write(temp.path().join("notes/a.md"), "# A, theirs\n").unwrap();

        let result = tx.commit();
        println!("conflict test, commit result: {result:?}");
        match &result {
            Err(CommitError::Other(ValidationFailure::Conflict(keys))) => {
                assert_eq!(keys, &vec![Key::name("notes/a")]);
            }
            other => panic!("expected a conflict, got {other:?}"),
        }
        assert_eq!(
            read_to_string(temp.path().join("notes/a.md")).unwrap(),
            "# A, theirs\n"
        );

        // A fresh transaction that reads the new content commits fine.
        tx.begin().unwrap();
        tx.write(Write::Put(
            Key::name("notes/a"),
            "# A, merged\n".to_string(),
        ))
        .unwrap();
        assert!(tx.commit().is_ok());
        assert_eq!(
            read_to_string(temp.path().join("notes/a.md")).unwrap(),
            "# A, merged\n"
        );
    }

    /// A failing `always` checker is enforced under full scope, and its
    /// rejection is compensating: the write was applied for the checker to
    /// read, then put back.
    #[cfg(unix)]
    #[test]
    fn full_scope_runs_always_checkers_and_reverts_on_failure() {
        use crate::config::Checker;

        let temp = TempDir::new().unwrap();
        write_schema(temp.path(), "note", "links:\n  - min: 0\n");
        create_dir_all(temp.path().join("notes")).unwrap();
        write(temp.path().join("notes/a.md"), "# A\n").unwrap();

        let mut config = config_with(&[("note", "notes/**")]);
        // Fails any key whose file contains the word "forbidden".
        config.checkers.insert(
            "no-forbidden".to_string(),
            Checker {
                command: r#"python3 -c '
import json,sys,os
inp=json.load(sys.stdin)
out=[]
for k in inp["keys"]:
    p=os.path.join(inp["root"],k+".md")
    if os.path.exists(p) and "forbidden" in open(p).read():
        out.append({"key":k,"violations":[{"message":"forbidden word"}]})
print(json.dumps(out))'"#
                    .to_string(),
                warn: false,
                always: true,
                description: None,
            },
        );

        let mut tx = transaction_for(&temp, config).with_scope(ValidationScope::Full);
        tx.begin().unwrap();
        tx.write(Write::Put(
            Key::name("notes/a"),
            "# A\n\nforbidden\n".to_string(),
        ))
        .unwrap();
        tx.write(Write::Put(Key::name("notes/b"), "# B\n".to_string()))
            .unwrap();
        let result = tx.commit();
        println!("checker test, commit result: {result:?}");
        assert!(matches!(
            result,
            Err(CommitError::Other(ValidationFailure::Violations(_)))
        ));
        assert_eq!(
            read_to_string(temp.path().join("notes/a.md")).unwrap(),
            "# A\n",
            "the rejected write must have been reverted"
        );
        assert!(
            !temp.path().join("notes/b.md").exists(),
            "the sibling write of the rejected transaction must have been reverted too"
        );

        tx.begin().unwrap();
        tx.write(Write::Put(
            Key::name("notes/a"),
            "# A\n\nfine\n".to_string(),
        ))
        .unwrap();
        assert!(tx.commit().is_ok());
    }
}
