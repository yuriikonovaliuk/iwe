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

use crate::permissions::WritePermissionError;

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
pub fn write_store_at_path(
    store: &State,
    to: &Path,
    format: Format,
    check: impl Fn(&Key, &str) -> Result<(), WritePermissionError>,
) -> std::io::Result<()> {
    for (key, content) in store.iter() {
        let doc_key = Key::name(key);
        let mut tx = NoopTransaction::new();
        tx.begin().expect("no-op transaction backend never fails");
        tx.write(TxWrite::Put(doc_key.clone(), content.clone()))
            .expect("no-op transaction backend never fails");
        if let Err(rejected) = check(&doc_key, content) {
            let _ = tx.abort();
            return Err(permission_denied(&doc_key, rejected));
        }
        if let Err(e) = write_file(key, content, to, format) {
            let _ = tx.abort();
            return Err(e);
        }
        tx.commit().expect("no-op transaction backend never fails");
    }
    Ok(())
}

/// Turns a write-permission rejection into the `std::io::Result` this
/// function already returns for every other kind of write failure, so a
/// rejection halts `apply_changes` (and is reported to the caller) exactly
/// the way an I/O error already does.
fn permission_denied(key: &Key, _rejected: WritePermissionError) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        format!("write rejected by write-permission check for '{}'", key),
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
// it exists immediately before removal (there is no "new" content for a
// removal). If that content can't be read, the removal proceeds without a
// check rather than blocking on an unrelated I/O failure.
pub fn apply_changes(
    changes: &Changes,
    base_path: &Path,
    format: Format,
    check: impl Fn(&Key, &str) -> Result<(), WritePermissionError>,
) -> std::io::Result<()> {
    let extension = format.extension();

    for key in &changes.removes {
        let file_path = base_path.join(format!("{}.{}", key, extension));
        let mut tx = NoopTransaction::new();
        tx.begin().expect("no-op transaction backend never fails");
        tx.write(TxWrite::Remove(key.clone()))
            .expect("no-op transaction backend never fails");
        if file_path.exists() {
            if let Ok(existing) = fs::read_to_string(&file_path) {
                if let Err(rejected) = check(key, &existing) {
                    let _ = tx.abort();
                    return Err(permission_denied(key, rejected));
                }
            }
            if let Err(e) = fs::remove_file(&file_path) {
                let _ = tx.abort();
                return Err(e);
            }
        }
        prune_empty_dirs(file_path.parent(), base_path);
        tx.commit().expect("no-op transaction backend never fails");
    }

    for (key, markdown) in &changes.creates {
        let file_path = base_path.join(format!("{}.{}", key, extension));
        if let Some(parent) = file_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut tx = NoopTransaction::new();
        tx.begin().expect("no-op transaction backend never fails");
        tx.write(TxWrite::Put(key.clone(), markdown.clone()))
            .expect("no-op transaction backend never fails");
        if let Err(rejected) = check(key, markdown) {
            let _ = tx.abort();
            return Err(permission_denied(key, rejected));
        }
        if let Err(e) = fs::write(&file_path, markdown) {
            let _ = tx.abort();
            return Err(e);
        }
        tx.commit().expect("no-op transaction backend never fails");
    }

    for (key, markdown) in &changes.updates {
        let file_path = base_path.join(format!("{}.{}", key, extension));
        let mut tx = NoopTransaction::new();
        tx.begin().expect("no-op transaction backend never fails");
        tx.write(TxWrite::Put(key.clone(), markdown.clone()))
            .expect("no-op transaction backend never fails");
        if let Err(rejected) = check(key, markdown) {
            let _ = tx.abort();
            return Err(permission_denied(key, rejected));
        }
        if let Err(e) = fs::write(&file_path, markdown) {
            let _ = tx.abort();
            return Err(e);
        }
        tx.commit().expect("no-op transaction backend never fails");
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
