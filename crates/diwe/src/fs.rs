use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::{collections::HashMap, fs};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::{Match, WalkBuilder};
use log::error;
use rayon::prelude::*;

use liwe::model::config::Format;
use liwe::model::{Content, Key, State};
use liwe::operations::Changes;
use liwe::transaction::{NoopTransaction, Transaction, Write as TxWrite};

use crate::journal::{self, Effect, KeyEffect};
use crate::permissions::{WriteOperation, WritePermissionError};

pub fn write_file(
    key: &String,
    content: &Content,
    to: &Path,
    format: Format,
) -> std::io::Result<()> {
    fs::write(
        to.join(format!("{}.{}", key, format.extension())),
        content.as_str(),
    )
}

pub fn new_for_path(base_path: &PathBuf, format: Format) -> State {
    if !base_path.exists() {
        error!("path doesn't exist");
        return State::new();
    }

    walk_md_paths(base_path, format)
        .into_par_iter()
        .filter_map(|(key, path)| {
            fs::read_to_string(&path)
                .ok()
                .map(|content| (key, sanitize_content(content)))
        })
        .collect()
}

pub fn walk_md_paths(base_path: &Path, format: Format) -> Vec<(String, PathBuf)> {
    if !base_path.exists() {
        error!("path doesn't exist");
        return Vec::new();
    }

    let extension = format.extension();

    WalkBuilder::new(base_path)
        .follow_links(false)
        .hidden(true)
        .require_git(false)
        .build()
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();

            if !path.is_file() || path.extension().is_none_or(|ext| ext != extension) {
                return None;
            }

            let relative_path = path.strip_prefix(base_path).ok()?;
            let key = relative_path
                .with_extension("")
                .components()
                .filter_map(|c| match c {
                    std::path::Component::Normal(os) => Some(os.to_string_lossy().to_string()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("/");

            Some((key, path.to_path_buf()))
        })
        .collect()
}

pub fn read_md_file(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(sanitize_content)
}

pub struct PathFilter {
    base_path: PathBuf,
    global: Gitignore,
    per_directory: Mutex<HashMap<PathBuf, Gitignore>>,
}

impl PathFilter {
    pub fn new(base_path: &Path) -> PathFilter {
        let (global, _) = Gitignore::global();
        PathFilter {
            base_path: base_path.to_path_buf(),
            global,
            per_directory: Mutex::new(HashMap::new()),
        }
    }

    pub fn includes(&self, path: &Path) -> bool {
        let Ok(relative) = path.strip_prefix(&self.base_path) else {
            return false;
        };

        let components: Vec<_> = relative.components().collect();

        if components.iter().any(|component| match component {
            std::path::Component::Normal(name) => name.to_string_lossy().starts_with('.'),
            _ => true,
        }) {
            return false;
        }

        let mut directory = self.base_path.clone();
        for component in &components[..components.len().saturating_sub(1)] {
            directory = directory.join(component);
            if self.is_ignored(&directory, true) {
                return false;
            }
        }

        !self.is_ignored(path, false)
    }

    fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        let mut directory = path.parent();
        while let Some(current) = directory {
            if !current.starts_with(&self.base_path) {
                break;
            }
            match self.matched_in(current, path, is_dir) {
                Match::Ignore(_) => return true,
                Match::Whitelist(_) => return false,
                Match::None => {}
            }
            if current == self.base_path {
                break;
            }
            directory = current.parent();
        }

        matches!(
            matched_under_root(&self.global, path, is_dir),
            Match::Ignore(_)
        )
    }

    fn matched_in(&self, directory: &Path, path: &Path, is_dir: bool) -> Match<()> {
        let mut cache = self.per_directory.lock().expect("filter cache to lock");
        let matcher = cache
            .entry(directory.to_path_buf())
            .or_insert_with(|| build_directory_gitignore(directory));
        matched_under_root(matcher, path, is_dir)
    }
}

fn matched_under_root(matcher: &Gitignore, path: &Path, is_dir: bool) -> Match<()> {
    if matcher.is_empty() || !path.starts_with(matcher.path()) {
        return Match::None;
    }
    matcher.matched(path, is_dir).map(|_| ())
}

fn build_directory_gitignore(directory: &Path) -> Gitignore {
    let mut builder = GitignoreBuilder::new(directory);
    builder.add(directory.join(".gitignore"));
    builder.add(directory.join(".ignore"));
    builder.add(directory.join(".git/info/exclude"));
    builder.build().unwrap_or_else(|_| Gitignore::empty())
}

pub fn new_from_hashmap(map: HashMap<String, String>) -> State {
    map.into_iter().collect()
}

// WP-11 (empty-key/normalize-all branch): each document rewritten by a
// whole-graph normalize is routed through the no-op Transaction interface
// as its own implicit single-write transaction, before the actual
// filesystem write. `check` is the same permission-check hook `apply_
// changes` takes (see its doc comment) — run inside this transaction
// bracket, after `begin()`, before the actual filesystem write, aborting
// the transaction and halting on rejection.
///
/// `journal_path`, if configured (see [`crate::journal`]), gets exactly one
/// journal record after every document in `store` has been written
/// successfully — one record for the whole call, carrying every key it
/// wrote, not one record per document. Nothing is appended if this
/// function returns an error partway through, or if `journal_path` is
/// `None` (the default).
pub fn write_store_at_path(
    store: &State,
    to: &Path,
    format: Format,
    check: impl Fn(&Key, &str, Option<&str>) -> Result<(), WritePermissionError>,
    journal_path: Option<&Path>,
) -> std::io::Result<()> {
    // Snapshotted before the writes land, since every key in `store` will
    // exist on disk by the time `write_store_at_path_with` returns —
    // needed only to tell a journal record's create effects from its
    // update effects; `write_store_at_path_with` itself is unaware of the
    // journal entirely.
    let existed: HashMap<&String, bool> = store
        .iter()
        .map(|(key, _)| (key, to.join(format!("{}.{}", key, format.extension())).exists()))
        .collect();

    write_store_at_path_with(store, to, format, check, NoopTransaction::new)?;

    let effects = store
        .iter()
        .map(|(key, _)| {
            let doc_key = Key::name(key);
            let effect = if existed.get(key).copied().unwrap_or(false) {
                Effect::Update
            } else {
                Effect::Create
            };
            KeyEffect::new(&doc_key, effect)
        })
        .collect();
    journal::record_commit(journal_path, effects);

    Ok(())
}

/// Generic core of [`write_store_at_path`], parameterized over the
/// transaction backend via a factory (`new_tx`) called once per document
/// to build the transaction used for that document's write.
/// `write_store_at_path` always calls this with `NoopTransaction::new`;
/// T6's tests call it with a factory that builds a call-recording stub
/// instead (`liwe::transaction::RecordingTransaction`), to prove this call
/// site actually drives `begin`/`write`/`commit`/`abort`, rather than
/// merely compiling against the trait.
///
/// `commit` is attempted before the real filesystem write, not after: see
/// the note on [`apply_changes_with`], which this function mirrors.
///
/// `check` is given both `content` (the outgoing write) and the document's
/// prior on-disk content (`None` if it doesn't exist yet), read from `to`
/// immediately before this document's write — the fix for M2's freeze-
/// bypass defect (`m2/design-freeze-semantics`): a write-permission
/// predicate fed only the outgoing content can't enforce a rule about a
/// transition (e.g. "frozen, unless this write's sole effect is lifting
/// freeze"), since it never sees what the document looked like before.
pub fn write_store_at_path_with<TX: Transaction>(
    store: &State,
    to: &Path,
    format: Format,
    check: impl Fn(&Key, &str, Option<&str>) -> Result<(), WritePermissionError>,
    mut new_tx: impl FnMut() -> TX,
) -> std::io::Result<()> {
    for (key, content) in store.iter() {
        let doc_key = Key::name(key);
        let file_path = to.join(format!("{}.{}", key, format.extension()));
        let prior_content = fs::read_to_string(&file_path).ok();
        let mut tx = new_tx();
        tx.begin()
            .map_err(|_| transaction_backend_failed(&doc_key))?;

        if tx
            .write(TxWrite::Put(doc_key.clone(), content.clone()))
            .is_err()
        {
            let _ = tx.commit();
            let _ = tx.abort();
            return Err(transaction_backend_failed(&doc_key));
        }
        if let Err(rejected) = check(&doc_key, content, prior_content.as_deref()) {
            let _ = tx.abort();
            return Err(permission_denied(&doc_key, rejected));
        }
        if tx.commit().is_err() {
            let _ = tx.abort();
            return Err(transaction_backend_failed(&doc_key));
        }
        if let Err(e) = write_file(key, content, to, format) {
            return Err(e);
        }
    }
    Ok(())
}

/// the way an I/O error already does. Uses `rejected`'s own `Display` (both
/// `WritePermissionError::Frozen` and `WritePermissionError::
/// PropertyImmutable` already carry their own document key) rather than a
/// generic message, so callers surfacing this error to a user get the
/// specific reason, not just "which key".
fn permission_denied(_key: &Key, rejected: WritePermissionError) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::PermissionDenied, rejected.to_string())
}

/// Turns a transaction backend's refusal (a rejected write, a refused
/// commit) into the `std::io::Result` `apply_changes`/`write_store_at_
/// path` already return for every other kind of write failure, so a
/// refusal halts the caller (and is reported to it) exactly the way an
/// I/O error already does, instead of being silently swallowed.
fn transaction_backend_failed(key: &Key) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::Other,
        format!("write rejected by transaction backend for '{}'", key),
    )
}

// WP-06..WP-09 (CLI delete/rename/extract/inline, via main.rs's
// `apply_changes` wrapper) and WP-12 (MCP write tools that go through
// `write_changes` -> this function): every remove/create/update below is
// routed through the no-op Transaction interface as its own implicit
// single-write transaction, before the actual filesystem operation.
//
// `check` lets a caller opt in to running `diwe::permissions::
// check_write_permission`-shaped logic (e.g. via `diwe::permissions::
// check_write_permission_for_content`) inside each per-write transaction
// bracket, after `tx.begin()` and before the real write, aborting the
// transaction and halting `apply_changes` on rejection — the same
// composition already used at `iwe::new::write_document` / `iwec`'s
// `write_file`. This is the one hook every caller of `apply_changes`
// (CLI delete/rename/extract/inline, MCP `write_changes`) wires
// identically, per `m2/design-enforcement-modes`'s "one mechanism, not
// two": the check is implemented once, here, not re-implemented at each
// caller.
//
// For `changes.removes`, `check` is given the document's on-disk content as
// it exists immediately before removal as the prior-content argument, and
// an empty string as `content` — the outgoing/"resulting" state of a
// removal is that the document (every frontmatter property and the body)
// no longer exists, not that it is unchanged. (D4 fix, M4-extension
// defect: this used to pass the same existing content as both arguments,
// on the reasoning that "a removal's resulting content is, in effect, its
// own unchanged prior content" — true under the write-permission predicate
// as it existed through D1's own fix, which checked every write
// unconditionally against `PropertyRef::Body` regardless of what the write
// actually touched, so passing identical content still triggered rejection
// via that unconditional check. Once D1 replaced that with a touched/
// untouched diff between prior and outgoing content
// [`crate::permissions::property_touched`], identical-content input made
// every property look untouched — silently permitting deletion of a
// document with an immutable body or any other `mutable: false` property,
// which is exactly the rule a deletion should trip: removing a document
// changes every property in it, including the immutable ones, from
// present to gone.) If the on-disk content can't be read, the removal
// proceeds without a check rather than blocking on an unrelated I/O
// failure. `check` is also given [`WriteOperation::Delete`] here — M4/R1's
// explicit operation signal (`m2/design-deletion-carrier`; see
// `diwe::permissions::WriteOperation`'s own doc comment), so a
// delete-specific rule (LAW-16's `deletable: false`) can be evaluated only
// for a genuine removal, never inferred from `content` happening to be
// `""`. For `changes.creates` and
// `changes.updates`, `check` additionally receives the document's prior
// on-disk content (`None` if it doesn't exist yet), read from `base_path`
// immediately before that document's write — the fix for M2's freeze-
// bypass defect (`m2/design-freeze-semantics`): a write-permission
// predicate fed only the outgoing content can't enforce a rule about a
// transition (e.g. "frozen, unless this write's sole effect is lifting
// freeze") — and [`WriteOperation::Write`], since neither ever removes a
// document.
///
/// `journal_path`, if configured (see [`crate::journal`]), gets exactly one
/// journal record after every one of `changes`'s removes/creates/updates
/// has landed successfully — one record for the whole call, carrying every
/// key it affected (a rename's remove *and* create both land in the same
/// record, for example), not one record per document. Nothing is appended
/// if this function returns an error partway through, or if `journal_path`
/// is `None` (the default).
pub fn apply_changes(
    changes: &Changes,
    base_path: &Path,
    format: Format,
    check: impl Fn(&Key, &str, Option<&str>, WriteOperation) -> Result<(), WritePermissionError>,
    journal_path: Option<&Path>,
) -> std::io::Result<()> {
    apply_changes_with(changes, base_path, format, check, NoopTransaction::new)?;
    journal::record_commit(journal_path, effects_for(changes));
    Ok(())
}

/// The journal effects `changes` represents: every removed key as a
/// [`Effect::Delete`], every created key as an [`Effect::Create`], every
/// updated key as an [`Effect::Update`] — the same three-way split
/// `Changes` itself already carries, read straight off it rather than
/// re-derived from the filesystem.
fn effects_for(changes: &Changes) -> Vec<KeyEffect> {
    changes
        .removes
        .iter()
        .map(|key| KeyEffect::new(key, Effect::Delete))
        .chain(
            changes
                .creates
                .iter()
                .map(|(key, _)| KeyEffect::new(key, Effect::Create)),
        )
        .chain(
            changes
                .updates
                .iter()
                .map(|(key, _)| KeyEffect::new(key, Effect::Update)),
        )
        .collect()
}

/// Generic core of [`apply_changes`], parameterized over the transaction
/// backend via a factory (`new_tx`) called once per document to build the
/// transaction used for that document's write or removal. `apply_changes`
/// always calls this with `NoopTransaction::new`; T6's tests call it with
/// a factory that builds a call-recording stub instead
/// (`liwe::transaction::RecordingTransaction`), to prove every one of
/// these call sites (WP-06..WP-09 CLI, WP-12 MCP) actually drives
/// `begin`/`write`/`commit`/`abort`, rather than merely compiling against
/// the trait.
///
/// `commit` is attempted before the real filesystem operation, not after:
/// a real backend makes writes durable in `commit`, so a commit refusal
/// must prevent the write (or removal) from landing rather than merely
/// being noticed once it already has. This is a no-op change in
/// observable behavior under `NoopTransaction` (whose `commit` never
/// fails), and matters only once a real backend is wired in.
///
/// If the transaction backend itself rejects a `write` call (T10/T11's
/// eventual real freeze/mutability logic, not yet landed as of T6),
/// `commit` is attempted anyway — rather than skipping straight to
/// `abort` — so that the failed-state contract on `Transaction::write`
/// (a rejected write must make `commit` refuse) is what this call site
/// actually observes, not merely assumes.
pub fn apply_changes_with<TX: Transaction>(
    changes: &Changes,
    base_path: &Path,
    format: Format,
    check: impl Fn(&Key, &str, Option<&str>, WriteOperation) -> Result<(), WritePermissionError>,
    mut new_tx: impl FnMut() -> TX,
) -> std::io::Result<()> {
    let extension = format.extension();

    for key in &changes.removes {
        let file_path = base_path.join(format!("{}.{}", key, extension));
        let mut tx = new_tx();
        tx.begin().map_err(|_| transaction_backend_failed(key))?;

        if tx.write(TxWrite::Remove(key.clone())).is_err() {
            let _ = tx.commit();
            let _ = tx.abort();
            return Err(transaction_backend_failed(key));
        }
        if file_path.exists() {
            if let Ok(existing) = fs::read_to_string(&file_path) {
                // D4 fix: pass "" as `content` (see this loop's own doc
                // comment above `apply_changes_with` for why) rather than
                // `&existing` — a removal's outgoing state is "nothing",
                // not "unchanged", so a mutability/freeze rule can actually
                // see the removal as the change it is. M4/R1: also pass
                // `WriteOperation::Delete` explicitly — this is the one
                // call site that actually knows a removal is happening, so
                // it is the one call site responsible for saying so, rather
                // than leaving `check` to infer it from `content == ""`.
                if let Err(rejected) = check(key, "", Some(&existing), WriteOperation::Delete) {
                    let _ = tx.abort();
                    return Err(permission_denied(key, rejected));
                }
            }
            if tx.commit().is_err() {
                let _ = tx.abort();
                return Err(transaction_backend_failed(key));
            }
            if let Err(e) = fs::remove_file(&file_path) {
                return Err(e);
            }
        } else {
            let _ = tx.commit();
        }
        prune_empty_dirs(file_path.parent(), base_path);
    }

    for (key, markdown) in &changes.creates {
        let file_path = base_path.join(format!("{}.{}", key, extension));
        let prior_content = fs::read_to_string(&file_path).ok();
        if let Some(parent) = file_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut tx = new_tx();
        tx.begin().map_err(|_| transaction_backend_failed(key))?;

        if tx
            .write(TxWrite::Put(key.clone(), markdown.clone()))
            .is_err()
        {
            let _ = tx.commit();
            let _ = tx.abort();
            return Err(transaction_backend_failed(key));
        }
        if let Err(rejected) = check(key, markdown, prior_content.as_deref(), WriteOperation::Write) {
            let _ = tx.abort();
            return Err(permission_denied(key, rejected));
        }
        if tx.commit().is_err() {
            let _ = tx.abort();
            return Err(transaction_backend_failed(key));
        }
        if let Err(e) = fs::write(&file_path, markdown) {
            return Err(e);
        }
    }

    for (key, markdown) in &changes.updates {
        let file_path = base_path.join(format!("{}.{}", key, extension));
        let prior_content = fs::read_to_string(&file_path).ok();
        let mut tx = new_tx();
        tx.begin().map_err(|_| transaction_backend_failed(key))?;

        if tx
            .write(TxWrite::Put(key.clone(), markdown.clone()))
            .is_err()
        {
            let _ = tx.commit();
            let _ = tx.abort();
            return Err(transaction_backend_failed(key));
        }
        if let Err(rejected) = check(key, markdown, prior_content.as_deref(), WriteOperation::Write) {
            let _ = tx.abort();
            return Err(permission_denied(key, rejected));
        }
        if tx.commit().is_err() {
            let _ = tx.abort();
            return Err(transaction_backend_failed(key));
        }
        if let Err(e) = fs::write(&file_path, markdown) {
            return Err(e);
        }
    }

    Ok(())
}

fn prune_empty_dirs(start: Option<&Path>, base_path: &Path) {
    let mut dir = start.map(|p| p.to_path_buf());
    while let Some(parent) = dir {
        if parent == base_path || !parent.starts_with(base_path) {
            break;
        }
        if parent.read_dir().map_or(false, |mut d| d.next().is_none()) {
            let _ = fs::remove_dir(&parent);
            dir = parent.parent().map(|p| p.to_path_buf());
        } else {
            break;
        }
    }
}

fn sanitize_content(content: String) -> String {
    let content = content
        .strip_prefix('\u{FEFF}')
        .map(|s| s.to_string())
        .unwrap_or(content);
    content.replace("\r\n", "\n").replace('\r', "\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_content_strips_crlf() {
        assert_eq!("a\nb\nc\n", sanitize_content("a\r\nb\r\nc\r\n".into()));
    }

    fn workspace_with_gitignore(ignore_body: &str) -> tempfile::TempDir {
        let base = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(base.path().join("node_modules/pkg")).unwrap();
        std::fs::create_dir_all(base.path().join("docs/drafts")).unwrap();
        std::fs::write(base.path().join(".gitignore"), ignore_body).unwrap();
        std::fs::write(base.path().join("note.md"), "# Note\n").unwrap();
        std::fs::write(base.path().join("docs/guide.md"), "# Guide\n").unwrap();
        std::fs::write(base.path().join("docs/drafts/wip.md"), "# Wip\n").unwrap();
        std::fs::write(base.path().join("node_modules/pkg/README.md"), "# Pkg\n").unwrap();
        base
    }

    fn walked_keys(base: &Path) -> Vec<String> {
        let mut keys: Vec<String> = walk_md_paths(base, Format::Markdown)
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        keys.sort();
        keys
    }

    fn filtered_keys(base: &Path) -> Vec<String> {
        let filter = PathFilter::new(base);
        let mut keys: Vec<String> = walk_all_md_paths(base)
            .into_iter()
            .filter(|path| filter.includes(path))
            .map(|path| {
                path.strip_prefix(base)
                    .unwrap()
                    .with_extension("")
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        keys.sort();
        keys
    }

    fn walk_all_md_paths(base: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let mut stack = vec![base.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|ext| ext == "md") {
                    found.push(path);
                }
            }
        }
        found
    }

    #[test]
    fn path_filter_agrees_with_walk_on_gitignored_directory() {
        let base = workspace_with_gitignore("node_modules\n");

        assert_eq!(
            walked_keys(base.path()),
            vec![
                "docs/drafts/wip".to_string(),
                "docs/guide".into(),
                "note".into()
            ]
        );
        assert_eq!(filtered_keys(base.path()), walked_keys(base.path()));
    }

    #[test]
    fn path_filter_agrees_with_walk_on_nested_gitignore() {
        let base = workspace_with_gitignore("node_modules\n");
        std::fs::write(base.path().join("docs/.gitignore"), "drafts\n").unwrap();

        assert_eq!(
            walked_keys(base.path()),
            vec!["docs/guide".to_string(), "note".into()]
        );
        assert_eq!(filtered_keys(base.path()), walked_keys(base.path()));
    }

    #[test]
    fn path_filter_agrees_with_walk_on_whitelisted_path() {
        let base = workspace_with_gitignore("docs\n!docs/guide.md\n");

        assert_eq!(
            walked_keys(base.path()),
            vec!["node_modules/pkg/README".to_string(), "note".into()]
        );
        assert_eq!(filtered_keys(base.path()), walked_keys(base.path()));
    }

    #[test]
    fn path_filter_rejects_hidden_and_outside_paths() {
        let base = workspace_with_gitignore("node_modules\n");
        let filter = PathFilter::new(base.path());

        assert!(!filter.includes(&base.path().join(".iwe/config.md")));
        assert!(!filter.includes(Path::new("/elsewhere/note.md")));
        assert!(filter.includes(&base.path().join("note.md")));
    }

    // T6: `apply_changes_with` is the generic core behind WP-06 (CLI
    // `delete`), WP-07 (CLI `rename`), WP-08 (CLI `extract`), WP-09 (CLI
    // `inline`), and WP-12 (every MCP write tool that removes/creates/
    // updates a document, via `iwec`'s `write_changes` — which calls
    // `apply_changes` directly, with no additional logic of its own, per
    // its own doc comment). `write_store_at_path_with` is the generic
    // core behind WP-11's whole-graph normalize (`write_graph` ->
    // `write_store_at_path`). These tests drive both generic cores
    // directly with a `liwe::transaction::RecordingTransaction` in place
    // of `NoopTransaction` to prove the wiring at every one of those call
    // sites is real.
    mod transaction_tests {
        use super::*;
        use liwe::transaction::{RecordingTransaction, TransactionLog};

        fn allow(_key: &Key, _content: &str, _prior_content: Option<&str>) -> Result<(), WritePermissionError> {
            Ok(())
        }

        /// Same as [`allow`], but shaped for `apply_changes_with`'s `check`
        /// closure (M4/R1: it additionally takes a [`WriteOperation`]).
        fn allow4(
            _key: &Key,
            _content: &str,
            _prior_content: Option<&str>,
            _operation: WriteOperation,
        ) -> Result<(), WritePermissionError> {
            Ok(())
        }

        /// WP-06 (delete): removing a document drives exactly one `begin`
        /// and one `commit`, and the file is actually gone afterward.
        #[test]
        fn delete_drives_begin_and_commit() {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("a.md"), "# A\n").unwrap();
            let changes = Changes::new().remove(Key::name("a"));
            let log = TransactionLog::new();

            let result = apply_changes_with(&changes, dir.path(), Format::Markdown, allow4, {
                let log = log.clone();
                move || RecordingTransaction::new(log.clone())
            });

            assert!(result.is_ok(), "{:?}", result.err());
            assert_eq!(log.begin_count(), 1);
            assert_eq!(log.commit_count(), 1);
            assert!(!dir.path().join("a.md").exists());
        }

        /// D4 fix (M4-extension defect): the write-permission `check` for a
        /// removal must see the document's on-disk content as
        /// `prior_content`, and an *empty* string as `content` — not the
        /// same existing content passed for both, which (once D1's
        /// touched/untouched diff landed) made every removal look like a
        /// no-op write and silently bypassed every mutability/freeze rule.
        /// This proves the exact values `apply_changes_with`'s removal
        /// branch now passes to `check`, independent of what any real
        /// `check_write_permission_for_content`-shaped predicate does with
        /// them.
        #[test]
        fn delete_checks_permission_with_empty_content_and_existing_prior_content() {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("a.md"), "# A\n\nbody\n").unwrap();
            let changes = Changes::new().remove(Key::name("a"));
            let seen: std::rc::Rc<std::cell::RefCell<Option<(String, Option<String>, WriteOperation)>>> =
                std::rc::Rc::new(std::cell::RefCell::new(None));

            let result = apply_changes_with(
                &changes,
                dir.path(),
                Format::Markdown,
                {
                    let seen = seen.clone();
                    move |_key, content, prior_content, operation| {
                        *seen.borrow_mut() = Some((
                            content.to_string(),
                            prior_content.map(str::to_string),
                            operation,
                        ));
                        Ok(())
                    }
                },
                NoopTransaction::new,
            );

            assert!(result.is_ok(), "{:?}", result.err());
            let (content, prior_content, operation) = seen.borrow().clone().expect("check was called");
            assert_eq!(content, "", "a removal's outgoing content must be empty");
            // M4/R1: a removal must be explicitly signaled as
            // `WriteOperation::Delete`, not left for `check` to infer from
            // `content == ""` (`m2/design-deletion-carrier`'s "the predicate
            // ... must therefore distinguish which operation it is judging").
            assert_eq!(operation, WriteOperation::Delete);
            assert_eq!(
                prior_content,
                Some("# A\n\nbody\n".to_string()),
                "a removal's prior content must be the document's on-disk content"
            );
        }

        /// WP-07 (rename): a remove + create pair each drive their own
        /// `begin`/`commit`, per the "one transaction per write" wiring
        /// documented on `apply_changes_with`.
        #[test]
        fn rename_drives_begin_and_commit_for_each_of_its_writes() {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("old.md"), "# Old\n").unwrap();
            let changes = Changes::new()
                .remove(Key::name("old"))
                .create(Key::name("new"), "# Old\n".to_string());
            let log = TransactionLog::new();

            let result = apply_changes_with(&changes, dir.path(), Format::Markdown, allow4, {
                let log = log.clone();
                move || RecordingTransaction::new(log.clone())
            });

            assert!(result.is_ok(), "{:?}", result.err());
            assert_eq!(log.begin_count(), 2);
            assert_eq!(log.commit_count(), 2);
            assert!(!dir.path().join("old.md").exists());
            assert_eq!(
                std::fs::read_to_string(dir.path().join("new.md")).unwrap(),
                "# Old\n"
            );
        }

        /// WP-08 (extract): the new document lands via `creates`, driving
        /// begin/commit for that write.
        #[test]
        fn extract_create_drives_begin_and_commit() {
            let dir = tempfile::tempdir().unwrap();
            let changes = Changes::new().create(Key::name("extracted"), "# Extracted\n".into());
            let log = TransactionLog::new();

            let result = apply_changes_with(&changes, dir.path(), Format::Markdown, allow4, {
                let log = log.clone();
                move || RecordingTransaction::new(log.clone())
            });

            assert!(result.is_ok(), "{:?}", result.err());
            assert_eq!(log.begin_count(), 1);
            assert_eq!(log.commit_count(), 1);
            assert_eq!(
                std::fs::read_to_string(dir.path().join("extracted.md")).unwrap(),
                "# Extracted\n"
            );
        }

        /// WP-09 (inline): the target document is rewritten via
        /// `updates`, driving begin/commit for that write.
        #[test]
        fn inline_update_drives_begin_and_commit() {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("host.md"), "# Host\n").unwrap();
            let changes = Changes::new().update(Key::name("host"), "# Host\n\ninlined\n".into());
            let log = TransactionLog::new();

            let result = apply_changes_with(&changes, dir.path(), Format::Markdown, allow4, {
                let log = log.clone();
                move || RecordingTransaction::new(log.clone())
            });

            assert!(result.is_ok(), "{:?}", result.err());
            assert_eq!(log.begin_count(), 1);
            assert_eq!(log.commit_count(), 1);
            assert_eq!(
                std::fs::read_to_string(dir.path().join("host.md")).unwrap(),
                "# Host\n\ninlined\n"
            );
        }

        /// WP-11 (normalize, whole-graph branch): each rewritten document
        /// drives its own begin/commit.
        #[test]
        fn write_store_at_path_drives_begin_and_commit() {
            let dir = tempfile::tempdir().unwrap();
            let mut store = State::new();
            store.insert("a".to_string(), "# A\n".to_string());
            let log = TransactionLog::new();

            let result = write_store_at_path_with(&store, dir.path(), Format::Markdown, allow, {
                let log = log.clone();
                move || RecordingTransaction::new(log.clone())
            });

            assert!(result.is_ok(), "{:?}", result.err());
            assert_eq!(log.begin_count(), 1);
            assert_eq!(log.commit_count(), 1);
            assert_eq!(
                std::fs::read_to_string(dir.path().join("a.md")).unwrap(),
                "# A\n"
            );
        }

        /// A commit refusal from the backend surfaces as an `Err` (not
        /// silently swallowed) and the write never lands on disk.
        #[test]
        fn commit_refusal_prevents_the_write_and_surfaces_as_an_error() {
            let dir = tempfile::tempdir().unwrap();
            let changes = Changes::new().create(Key::name("a"), "# A\n".to_string());
            let log = TransactionLog::new();

            let result = apply_changes_with(&changes, dir.path(), Format::Markdown, allow4, {
                let log = log.clone();
                move || RecordingTransaction::refusing_commit(log.clone())
            });

            assert!(result.is_err());
            assert_eq!(log.commit_count(), 1, "commit must actually be attempted");
            assert_eq!(log.abort_count(), 1, "a refused commit must be aborted");
            assert!(
                !dir.path().join("a.md").exists(),
                "the write must not land on disk when commit is refused"
            );
        }

        /// A write-permission rejection mid-transaction (standing in for
        /// T10/T11's real freeze/mutability logic, not yet landed as of
        /// T6): `commit` is attempted and refuses per the failed-state
        /// contract on `Transaction::write`, `abort` succeeds, and no
        /// partial state persists. NOTE for whoever integrates T10/T11:
        /// re-run this test's intent against the real freeze/mutability
        /// construct once it lands, in place of
        /// `RecordingTransaction::rejecting_next_write`.
        #[test]
        fn write_rejection_mid_transaction_refuses_commit_and_aborts_cleanly() {
            let dir = tempfile::tempdir().unwrap();
            let changes = Changes::new().create(Key::name("a"), "# A\n".to_string());
            let log = TransactionLog::new();

            let result = apply_changes_with(&changes, dir.path(), Format::Markdown, allow4, {
                let log = log.clone();
                move || RecordingTransaction::rejecting_next_write(log.clone())
            });

            assert!(result.is_err());
            assert_eq!(log.begin_count(), 1);
            assert_eq!(log.write_count(), 1);
            assert_eq!(log.commit_count(), 1, "commit must actually be attempted");
            assert_eq!(log.abort_count(), 1);
            assert!(
                !dir.path().join("a.md").exists(),
                "no partial state must persist after a mid-transaction rejection"
            );
        }
    }

    #[test]
    fn walk_md_paths_uses_forward_slash_separators_for_nested_files() {
        let base = tempfile::tempdir().unwrap();
        let nested = base.path().join("sub").join("dir");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("note.md"), "# note\n").unwrap();

        let keys = walk_md_paths(base.path(), Format::Markdown)
            .into_iter()
            .map(|(key, _)| key)
            .collect::<Vec<_>>();

        assert_eq!(keys, vec!["sub/dir/note".to_string()]);
    }
}
