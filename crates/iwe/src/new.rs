use std::collections::BTreeMap;
use std::io::{self, Read};
use std::path::PathBuf;

use chrono::Local;
use minijinja::value::Value as TemplateValue;
use minijinja::Environment;
use rand::distr::Alphanumeric;
use rand::Rng;

use diwe::config::{Configuration, NoteTemplate, DEFAULT_KEY_DATE_FORMAT};
use liwe::graph::Graph;
use liwe::locale::get_locale;
use liwe::markdown::MarkdownReader;
use liwe::model::{
    prepend_frontmatter, split_raw_frontmatter, strip_doc_extension, Frontmatter, Key,
};
use liwe::transaction::{NoopTransaction, Transaction, Write as TxWrite};

pub const BODY_VARIABLE: &str = "body";
pub const LEGACY_BODY_VARIABLE: &str = "content";
pub const TITLE_VARIABLE: &str = "title";
pub const RESERVED_VARIABLES: [&str; 4] = ["slug", "today", "now", "id"];

pub type Variables = BTreeMap<String, serde_yaml::Value>;

#[derive(Debug, Clone, Default, clap::ValueEnum)]
pub enum IfExists {
    #[default]
    Suffix,
    Override,
    Skip,
    Fail,
}

fn get_default_template() -> NoteTemplate {
    NoteTemplate {
        key_template: "{{slug}}".to_string(),
        document_template: "# {{title}}\n\n{{content}}".to_string(),
    }
}

pub struct DocumentCreator<'a> {
    config: &'a Configuration,
    library_path: PathBuf,
}

pub struct CreateOptions {
    pub template_name: Option<String>,
    pub variables: Variables,
    pub key: Option<String>,
    pub if_exists: IfExists,
    pub frontmatter: Option<Frontmatter>,
    pub empty_key_error: String,
}

pub struct ContentOptions {
    pub key: String,
    pub content: String,
    pub if_exists: IfExists,
}

pub struct PreparedDocument {
    pub key: Key,
    pub path: PathBuf,
    pub content: String,
}

pub struct CreatedDocument {
    pub path: PathBuf,
}

impl<'a> DocumentCreator<'a> {
    pub fn new(config: &'a Configuration, library_path: PathBuf) -> Self {
        Self {
            config,
            library_path,
        }
    }

    fn find_available_key(&self, base_key: &Key) -> Key {
        let mut candidate_key = base_key.clone();
        let mut counter = 1;

        while self
            .library_path
            .join(candidate_key.to_path(self.config.format))
            .exists()
        {
            let suffixed_name = format!("{}-{}", base_key, counter);
            candidate_key = Key::name(&suffixed_name);
            counter += 1;
        }

        candidate_key
    }

    fn locate(
        &self,
        relative_key: &str,
        empty_key_error: &str,
        if_exists: &IfExists,
    ) -> Result<Option<(Key, PathBuf)>, String> {
        let base_key = Key::name(relative_key);
        if base_key.as_str().is_empty() {
            return Err(empty_key_error.to_string());
        }

        let path_str = base_key.to_path(self.config.format);
        let filename_len = std::path::Path::new(&path_str)
            .file_name()
            .map(|name| name.len())
            .unwrap_or(path_str.len());
        if filename_len > 255 {
            return Err(format!(
                "Generated filename is too long ({} bytes, max 255). Use a shorter title.",
                filename_len
            ));
        }

        let file_exists = self.library_path.join(&path_str).exists();
        let final_key = match if_exists {
            IfExists::Skip if file_exists => return Ok(None),
            IfExists::Fail if file_exists => {
                return Err(format!("Document '{}' already exists", base_key))
            }
            IfExists::Suffix => self.find_available_key(&base_key),
            IfExists::Override | IfExists::Skip | IfExists::Fail => base_key,
        };

        let file_path = self
            .library_path
            .join(final_key.to_path(self.config.format));
        Ok(Some((final_key, file_path)))
    }

    pub fn prepare_content(
        &self,
        options: ContentOptions,
    ) -> Result<Option<PreparedDocument>, String> {
        if strip_doc_extension(&options.key) != options.key.as_str() {
            return Err(format!(
                "Key '{}' must not include a file extension",
                options.key
            ));
        }

        match self.locate(&options.key, "Provided key is empty.", &options.if_exists)? {
            Some((key, path)) => Ok(Some(PreparedDocument {
                key,
                path,
                content: options.content,
            })),
            None => Ok(None),
        }
    }

    pub fn prepare(&self, options: CreateOptions) -> Result<Option<PreparedDocument>, String> {
        let template_name = options
            .template_name
            .or_else(|| self.config.library.default_template.clone())
            .unwrap_or_else(|| "default".to_string());

        let fallback_template = get_default_template();
        let template = self
            .config
            .templates
            .get(&template_name)
            .or_else(|| {
                if template_name == "default" {
                    Some(&fallback_template)
                } else {
                    None
                }
            })
            .ok_or_else(|| format!("Template '{}' not found in configuration", template_name))?;

        let key_date_format = self
            .config
            .library
            .date_format
            .clone()
            .unwrap_or_else(|| DEFAULT_KEY_DATE_FORMAT.to_string());

        let format_options = self.config.format_options();

        let content_date_format = format_options
            .date_format()
            .unwrap_or("%b %d, %Y")
            .to_string();

        let key_time_format = self
            .config
            .library
            .time_format
            .clone()
            .unwrap_or_else(|| key_date_format.clone());

        let content_time_format = format_options
            .time_format()
            .map(|format| format.to_string())
            .unwrap_or_else(|| content_date_format.clone());

        let key_locale = get_locale(self.config.library.locale.as_deref());
        let content_locale = get_locale(format_options.locale());

        let now = Local::now();
        let key_today = now
            .format_localized(&key_date_format, key_locale)
            .to_string();
        let content_today = now
            .format_localized(&content_date_format, content_locale)
            .to_string();
        let key_now = now
            .format_localized(&key_time_format, key_locale)
            .to_string();
        let content_now = now
            .format_localized(&content_time_format, content_locale)
            .to_string();

        let slug = string_to_slug(&scalar_text(options.variables.get(TITLE_VARIABLE)));
        let id = generate_random_id();

        let relative_key = match &options.key {
            Some(key) => {
                if strip_doc_extension(key) != key.as_str() {
                    return Err(format!("Key '{}' must not include a file extension", key));
                }
                key.clone()
            }
            None => render_template(
                &template.key_template,
                &template_context(&options.variables, &slug, &key_today, &key_now, &id),
            )?,
        };

        let document_content = render_template(
            &template.document_template,
            &template_context(&options.variables, &slug, &content_today, &content_now, &id),
        )?;

        let document_content = prepend_frontmatter(options.frontmatter, &document_content)?;

        let empty_key_error = if options.key.is_some() {
            "Provided key is empty."
        } else {
            &options.empty_key_error
        };

        match self.locate(&relative_key, empty_key_error, &options.if_exists)? {
            Some((key, path)) => Ok(Some(PreparedDocument {
                key,
                path,
                content: document_content,
            })),
            None => Ok(None),
        }
    }
}

pub fn normalize_content(config: &Configuration, key: &Key, content: &str) -> String {
    let (front, body) = split_raw_frontmatter(content);
    if body.trim().is_empty() {
        return content.to_string();
    }

    let mut graph = Graph::new_with_options(config.format_options());
    graph.from_markdown(key.clone(), body, MarkdownReader::new());
    let normalized = graph.to_markdown_skip_frontmatter(key);
    if normalized.trim().is_empty() {
        return content.to_string();
    }

    match front {
        Some(front) => format!("{}\n\n{}", front.trim_end_matches('\n'), normalized),
        None => normalized,
    }
}

// WP-02 (create_command) / WP-03 (new_command): both CLI commands funnel their
// durable write through this single call site, so wrapping it here covers
// both. The write is routed through the storage-agnostic `Transaction`
// interface (begin -> write(Put) -> commit) using the no-op default backend;
// the actual persistence still happens via `std::fs::write` below, since
// `NoopTransaction` performs no storage of its own (see transaction.rs).
//
// The write-permission check (`diwe::permissions::
// check_write_permission_for_content`, which resolves `prepared.key`'s
// real schema binding via `configuration.schemas` rather than a placeholder)
// runs inside this transaction bracket — after `begin()`, before the actual
// filesystem write — rather than at the `new_command`/`create_command` call
// sites before `write_document` is even invoked. Per `m2/design-transactions`,
// a write-permission rejection must be able to drive the transaction into
// its failed/aborted state, which requires the transaction to already be
// open when the check runs.
pub fn write_document(
    configuration: &Configuration,
    prepared: &PreparedDocument,
) -> Result<CreatedDocument, String> {
    write_document_with(configuration, prepared, NoopTransaction::new)
}

/// Generic core of [`write_document`], parameterized over the transaction
/// backend via a factory (`new_tx`) called once to build the transaction
/// used for this write. `write_document` always calls this with
/// `NoopTransaction::new`; T6's tests call it with a factory that builds a
/// call-recording stub instead (`liwe::transaction::RecordingTransaction`),
/// to prove this call site actually drives `begin`/`write`/`commit`/
/// `abort` on whatever `Transaction` it is given, rather than merely
/// compiling against the trait.
///
/// `commit` is attempted before the real filesystem write, not after: a
/// real backend makes writes durable in `commit`, so a commit refusal
/// must prevent the write from landing rather than merely being noticed
/// once it already has. This is a no-op change in observable behavior
/// under `NoopTransaction` (whose `commit` never fails), and matters only
/// once a real backend is wired in.
pub fn write_document_with<TX: Transaction>(
    configuration: &Configuration,
    prepared: &PreparedDocument,
    mut new_tx: impl FnMut() -> TX,
) -> Result<CreatedDocument, String> {
    if let Some(parent) = prepared.path.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create parent directories: {}", e))?;
        }
    }

    let mut tx = new_tx();
    tx.begin()
        .map_err(|_| "transaction backend failed to begin".to_string())?;

    if tx
        .write(TxWrite::Put(prepared.key.clone(), prepared.content.clone()))
        .is_err()
    {
        // The transaction backend itself rejected this write (T10/T11's
        // eventual real freeze/mutability logic, driven through
        // `Transaction::write` — not the placeholder
        // `check_write_permission_for_content` call below). Per the
        // failed-state contract on `Transaction::write`, `commit` must
        // now refuse: attempt it anyway (rather than skipping straight to
        // `abort`) so that refusal is what this call site actually
        // observes, not merely assumes.
        let commit_result = tx.commit();
        debug_assert!(
            commit_result.is_err(),
            "a transaction with a rejected write must refuse commit"
        );
        let _ = tx.abort();
        return Err("write rejected by transaction backend".to_string());
    }

    if diwe::permissions::check_write_permission_for_content(
        configuration,
        &prepared.key,
        &prepared.content,
    )
    .is_err()
    {
        // T10/T11/T12: surface the rejection once WP-02..WP-13 are
        // implemented. The placeholder check never returns Err today, so
        // this arm is unreachable in practice.
        let _ = tx.abort();
        return Err("write rejected by write-permission check".to_string());
    }

    if tx.commit().is_err() {
        let _ = tx.abort();
        return Err("write rejected: transaction backend refused to commit".to_string());
    }

    if let Err(e) = std::fs::write(&prepared.path, &prepared.content) {
        return Err(format!("Failed to write file: {}", e));
    }

    Ok(CreatedDocument {
        path: prepared
            .path
            .canonicalize()
            .unwrap_or_else(|_| prepared.path.clone()),
    })
}

pub fn read_stdin_if_available() -> String {
    use std::io::IsTerminal;

    if std::io::stdin().is_terminal() {
        return String::new();
    }

    read_stdin()
}

pub fn read_stdin() -> String {
    let mut buffer = String::new();
    io::stdin().read_to_string(&mut buffer).unwrap_or_default();
    buffer
}

fn template_context(
    variables: &Variables,
    slug: &str,
    today: &str,
    now: &str,
    id: &str,
) -> BTreeMap<String, TemplateValue> {
    let mut context: BTreeMap<String, TemplateValue> = variables
        .iter()
        .map(|(name, value)| (name.clone(), TemplateValue::from_serialize(value)))
        .collect();

    if let Some(body) = context.get(BODY_VARIABLE).cloned() {
        context.insert(LEGACY_BODY_VARIABLE.to_string(), body);
    }

    context.insert("slug".to_string(), TemplateValue::from(slug));
    context.insert("today".to_string(), TemplateValue::from(today));
    context.insert("now".to_string(), TemplateValue::from(now));
    context.insert("id".to_string(), TemplateValue::from(id));
    context
}

fn render_template(
    template_str: &str,
    context: &BTreeMap<String, TemplateValue>,
) -> Result<String, String> {
    Environment::new()
        .template_from_str(template_str)
        .map_err(|e| format!("Invalid template syntax: {}", e))?
        .render(context)
        .map_err(|e| format!("Template rendering failed: {}", e))
}

fn generate_random_id() -> String {
    rand::rng()
        .sample_iter(&Alphanumeric)
        .take(8)
        .map(char::from)
        .collect::<String>()
        .to_lowercase()
}

fn scalar_text(value: Option<&serde_yaml::Value>) -> String {
    match value {
        Some(serde_yaml::Value::String(text)) => text.clone(),
        Some(serde_yaml::Value::Number(number)) => number.to_string(),
        Some(serde_yaml::Value::Bool(flag)) => flag.to_string(),
        _ => String::new(),
    }
}

fn string_to_slug(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

// T6: `write_document_with` is the generic core behind WP-02 (CLI
// `create`) and WP-03 (CLI `new`) — both `create_command` and
// `new_command` funnel their durable write through `write_document`,
// which is a thin wrapper over `write_document_with(.., NoopTransaction::
// new)`. These tests drive `write_document_with` directly with a
// `liwe::transaction::RecordingTransaction` in place of `NoopTransaction`
// to prove the wiring is real.
#[cfg(test)]
mod transaction_tests {
    use super::*;
    use liwe::transaction::{RecordingTransaction, TransactionLog, TxEvent};

    fn prepared_document(dir: &std::path::Path, name: &str, content: &str) -> PreparedDocument {
        PreparedDocument {
            key: Key::name(name),
            path: dir.join(format!("{}.md", name)),
            content: content.to_string(),
        }
    }

    /// WP-02/WP-03 (CLI create/new): an ordinary write drives exactly one
    /// `begin` and one `commit` on the transaction it's given, and the
    /// content actually lands on disk.
    #[test]
    fn ordinary_write_drives_begin_and_commit() {
        let dir = tempfile::tempdir().unwrap();
        let config = Configuration::default();
        let prepared = prepared_document(dir.path(), "note", "# Note\n");
        let log = TransactionLog::new();

        let result =
            write_document_with(&config, &prepared, || RecordingTransaction::new(log.clone()));

        assert!(result.is_ok(), "{:?}", result.err());
        assert_eq!(log.begin_count(), 1);
        assert_eq!(log.commit_count(), 1);
        assert_eq!(log.abort_count(), 0);
        assert_eq!(
            std::fs::read_to_string(&prepared.path).unwrap(),
            "# Note\n"
        );
    }

    /// A commit refusal from the backend surfaces as an `Err` (not
    /// silently swallowed) and the write never lands on disk.
    #[test]
    fn commit_refusal_prevents_the_write_and_surfaces_as_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let config = Configuration::default();
        let prepared = prepared_document(dir.path(), "note", "# Note\n");
        let log = TransactionLog::new();

        let result = write_document_with(&config, &prepared, || {
            RecordingTransaction::refusing_commit(log.clone())
        });

        assert!(result.is_err());
        assert_eq!(log.commit_count(), 1, "commit must actually be attempted");
        assert_eq!(log.abort_count(), 1, "a refused commit must be aborted");
        assert!(
            !prepared.path.exists(),
            "the write must not land on disk when commit is refused"
        );
    }

    /// A write-permission rejection mid-transaction (standing in for
    /// T10/T11's real freeze/mutability logic, not yet landed as of T6):
    /// `commit` is attempted and refuses per the failed-state contract on
    /// `Transaction::write`, `abort` succeeds, and no partial state
    /// persists. NOTE for whoever integrates T10/T11: re-run this test's
    /// intent against the real freeze/mutability construct once it lands,
    /// in place of `RecordingTransaction::rejecting_next_write`.
    #[test]
    fn write_rejection_mid_transaction_refuses_commit_and_aborts_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let config = Configuration::default();
        let prepared = prepared_document(dir.path(), "note", "# Note\n");
        let log = TransactionLog::new();

        let result = write_document_with(&config, &prepared, || {
            RecordingTransaction::rejecting_next_write(log.clone())
        });

        assert!(result.is_err());
        let events = log.events();
        assert_eq!(events[0], TxEvent::Begin);
        assert!(matches!(events[1], TxEvent::Write(_)));
        assert_eq!(events[2], TxEvent::Commit);
        assert_eq!(events[3], TxEvent::Abort);
        assert_eq!(log.commit_count(), 1, "commit must actually be attempted");
        assert!(
            !prepared.path.exists(),
            "no partial state must persist after a mid-transaction rejection"
        );
    }
}
