use indoc::indoc;
use log::debug;
use std::{
    collections::HashMap,
    env,
    fs::read_to_string,
    path::{Path, PathBuf},
};
use toml_edit::{value, DocumentMut, Item};

use serde::{Deserialize, Serialize};

use crate::schema::patterns_match_raw;
use crate::search::{parse_language, Language};
use liwe::model::Key;
pub use liwe::model::config::{
    DjotOptions, Format, FormatOptions, FormattingOptions, InlineType, LineBreakStyle, LinkType,
    MarkdownOptions, Operation, RefsPath, RefsText, TargetType, WikiLinkPath,
    DEFAULT_KEY_DATE_FORMAT,
};

const CONFIG_FILE_NAME: &str = "config.toml";
pub const IWE_MARKER: &str = ".iwe";

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LibraryOptions {
    #[serde(default)]
    pub path: String,
    pub date_format: Option<String>,
    pub time_format: Option<String>,
    pub default_template: Option<String>,
    pub frontmatter_document_title: Option<String>,
    pub locale: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionOptions {
    pub link_format: Option<LinkType>,
    pub min_prefix_length: Option<usize>,
    pub trigger_characters: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SearchOptions {
    #[serde(default = "default_search_language")]
    pub language: String,
}

fn default_search_language() -> String {
    "english".to_string()
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            language: default_search_language(),
        }
    }
}

/// A transaction journal: on a successful commit, IWE appends one record
/// noting which keys were affected and how (see [`crate::journal`]) to
/// `path`. Left unset (the default), IWE writes nothing and behaves
/// exactly as it does without this option — a tool watching for changes
/// (an audit trail, an undo/backup mechanism, an external indexer) has to
/// opt in explicitly by setting this.
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct JournalOptions {
    #[serde(default)]
    pub path: Option<String>,
}

/// `[commit]`: an out-of-process command IWE runs after every successful
/// commit whose journal append produced a record (see [`crate::journal`]
/// and [`crate::commit_trigger`]). The command runs through the shell
/// (`sh -c`), with the store root as its working directory and two extra
/// environment variables: `IWE_STORE_ROOT` (the absolute store path) and
/// `IWE_COMMIT_LOCK_GENERATION` (the decimal generation of the commit-lock
/// hold the commit ran under). Best-effort and fail-open: a trigger that
/// cannot start, exits non-zero, or outlives its timeout never touches
/// the write or its journal record, never changes the commit's result or
/// exit code, and prints at most one line to stderr. Absent entirely, IWE
/// behaves exactly as it does without this section.
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CommitOptions {
    /// The shell command to run after a journal-recorded commit, or absent
    /// ([`None`], the default) to disable the trigger entirely.
    #[serde(default)]
    pub command: Option<String>,
    /// How long IWE waits for the command before giving up (failure is
    /// ignored either way). Defaults to `30` seconds when absent.
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
}

/// How far a write's transaction looks before it lets the write land.
/// `none` (the default) keeps AB9's no-op passthrough: the pending
/// documents are shape-checked on their own and nothing else stands
/// between a tool call and the disk. `affected-set` checks the
/// index-bounded link rules over the documents the write can reach
/// (`crate::validating_transaction`). `affected-set-with-checkers` is the
/// same affected-set schema/links check, plus the `always` external
/// `[checkers.*]` (and the compiled-in always-checkers) over the touched
/// keys -- the same checker call `full` makes, just without `full`'s own
/// whole-store schema/links re-validation: a store whose checkers (e.g. a
/// spaCy-backed term-closure pass) are the enforcement that actually
/// matters per write, and whose whole-store schema shape is cheap enough
/// to trust between full runs (`kc sync`, `iwe schema validate`), gets
/// checker coverage on every write without `full`'s per-write cost of
/// re-validating every other document too. `full` validates the whole
/// store's final state the way `iwe schema validate` does — every schema,
/// the `[invariants]`, and the `always` checkers over the touched keys —
/// and refuses the commit on any failing report, so a store that is clean
/// before a write is clean after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ValidationScope {
    #[default]
    None,
    AffectedSet,
    AffectedSetWithCheckers,
    Full,
}

/// `[transactions]`: what a write's transaction backend does at commit.
/// Left at its default, IWE behaves exactly as without the section.
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TransactionOptions {
    #[serde(default)]
    pub validate: ValidationScope,
    /// Keys a write may not touch, in [`SchemaBinding::r#match`] glob
    /// syntax. Left empty (the default), nothing is denied on this basis.
    #[serde(default)]
    pub deny: Vec<String>,
    /// Keys a write may touch, in the same glob syntax. Non-empty, this is
    /// an allowlist: only matching keys are permitted and `deny` is not
    /// consulted. Left empty (the default), nothing is restricted on this
    /// basis.
    #[serde(default)]
    pub allow: Vec<String>,
}

/// How strictly `[integrity]` holds one structural property of the store
/// (links that resolve, pages reachable from the root). `off` (the
/// default) enforces nothing. `no-new` refuses a commit that adds a
/// violation the pre-commit state did not have: existing debt may stay or
/// shrink, never grow. `strict` refuses a commit whose resulting store has
/// any violation at all; there is no exceptions list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum IntegrityMode {
    #[default]
    Off,
    NoNew,
    Strict,
}

impl IntegrityMode {
    pub fn is_off(self) -> bool {
        self == IntegrityMode::Off
    }

    pub fn label(self) -> &'static str {
        match self {
            IntegrityMode::Off => "off",
            IntegrityMode::NoNew => "no-new",
            IntegrityMode::Strict => "strict",
        }
    }
}

fn default_integrity_root() -> String {
    "index".to_string()
}

/// `[integrity]`: structural link integrity, enforced at the commit every
/// write goes through (see [`crate::integrity`]) and reported by
/// `iwe schema validate`. `links` governs broken links (an internal link or
/// inclusion to a key that does not exist); `orphans` governs documents not
/// reachable from `root` by following outgoing links and inclusions. Left
/// at its default (both `off`), IWE behaves exactly as without the section.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IntegrityOptions {
    #[serde(default)]
    pub links: IntegrityMode,
    #[serde(default)]
    pub orphans: IntegrityMode,
    /// The key every document must be reachable from. Default `index`.
    #[serde(default = "default_integrity_root")]
    pub root: String,
}

impl Default for IntegrityOptions {
    fn default() -> Self {
        Self {
            links: IntegrityMode::Off,
            orphans: IntegrityMode::Off,
            root: default_integrity_root(),
        }
    }
}

impl IntegrityOptions {
    /// Whether either property is enforced.
    pub fn is_enabled(&self) -> bool {
        !self.links.is_off() || !self.orphans.is_off()
    }

    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

impl Default for LibraryOptions {
    fn default() -> Self {
        Self {
            path: String::new(),
            date_format: Some(DEFAULT_KEY_DATE_FORMAT.into()),
            time_format: None,
            default_template: None,
            frontmatter_document_title: None,
            locale: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Configuration {
    pub version: Option<u32>,
    #[serde(default)]
    pub format: Format,
    #[serde(default)]
    pub markdown: MarkdownOptions,
    #[serde(default)]
    pub djot: DjotOptions,
    #[serde(default)]
    pub library: LibraryOptions,
    #[serde(default)]
    pub completion: CompletionOptions,
    #[serde(default)]
    pub search: SearchOptions,
    #[serde(default)]
    pub journal: JournalOptions,
    #[serde(default)]
    pub transactions: TransactionOptions,
    #[serde(default, skip_serializing_if = "IntegrityOptions::is_default")]
    pub integrity: IntegrityOptions,
    #[serde(default)]
    pub commit: CommitOptions,
    #[serde(default)]
    pub commands: HashMap<String, Command>,
    #[serde(default)]
    pub actions: HashMap<String, ActionDefinition>,
    #[serde(default)]
    pub templates: HashMap<String, NoteTemplate>,
    #[serde(default)]
    pub schemas: HashMap<String, SchemaBinding>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub invariants: HashMap<String, Invariant>,
    /// External checkers, run by `iwe schema validate` on a whole-store
    /// validation: any program that reads the selected keys as JSON on stdin
    /// and writes reports as JSON on stdout. IWE assumes nothing about what
    /// the program is.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub checkers: HashMap<String, Checker>,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    pub run: String,
    pub args: Option<Vec<String>>,
    pub cwd: Option<String>,
    pub env: Option<HashMap<String, String>>,
    pub shell: Option<bool>,
    pub timeout_seconds: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type")]
pub enum ActionDefinition {
    #[serde(rename = "transform")]
    Transform(Transform),
    #[serde(rename = "attach")]
    Attach(Attach),
    #[serde(rename = "sort")]
    Sort(Sort),
    #[serde(rename = "inline")]
    Inline(Inline),
    #[serde(rename = "extract")]
    Extract(Extract),
    #[serde(rename = "extract_all")]
    ExtractAll(ExtractAll),
    #[serde(rename = "link")]
    Link(Link),
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Transform {
    pub title: String,
    pub command: String,
    pub input_template: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Attach {
    pub title: String,
    pub key_template: String,
    pub document_template: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Sort {
    pub title: String,
    pub reverse: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Inline {
    pub title: String,
    pub inline_type: InlineType,
    pub keep_target: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Extract {
    pub title: String,
    pub link_type: Option<LinkType>,
    pub key_template: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractAll {
    pub title: String,
    pub link_type: Option<LinkType>,
    pub key_template: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Link {
    pub title: String,
    pub link_type: Option<LinkType>,
    pub key_template: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NoteTemplate {
    pub key_template: String,
    pub document_template: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Patterns {
    One(String),
    Many(Vec<String>),
}

impl Patterns {
    pub fn as_slice(&self) -> &[String] {
        match self {
            Patterns::One(pattern) => std::slice::from_ref(pattern),
            Patterns::Many(patterns) => patterns,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaBinding {
    pub r#match: Patterns,
}

/// A graph-wide standing check: the documents matching `filter` must number
/// what `expect` says — an integer, or a count predicate such as
/// `"{ $lte: 3 }"`. The filter may use `$today`, `$today-Nd`, `$today+Nd`,
/// replaced by ISO dates before parsing. Checked by `iwe schema validate`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Invariant {
    pub filter: String,
    pub expect: toml::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// An external checker: `command` is run through the shell with
/// `{ "root": "<store dir>", "keys": [...] }` on stdin and must print a JSON
/// array of `{ "key": "...", "violations": [ { "message": "...", "hint":
/// "...", "pointer": "..." } ] }`. A non-zero exit is itself a violation.
/// `warn` reports without failing the run; `always` runs it on every
/// whole-store validation rather than only with `--checkers`. `paths`, in
/// [`SchemaBinding::r#match`] glob syntax, limits it to runs whose keys
/// include at least one matching key: a commit touching none of them does
/// not start the checker at all.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Checker {
    pub command: String,
    #[serde(default)]
    pub warn: bool,
    #[serde(default)]
    pub always: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl Default for Configuration {
    fn default() -> Self {
        Self {
            version: Some(3),
            format: Default::default(),
            markdown: Default::default(),
            djot: Default::default(),
            library: Default::default(),
            completion: Default::default(),
            search: Default::default(),
            journal: Default::default(),
            transactions: Default::default(),
            integrity: Default::default(),
            commit: Default::default(),
            commands: Default::default(),
            actions: Default::default(),
            templates: Default::default(),
            schemas: Default::default(),
            invariants: Default::default(),
            checkers: Default::default(),
        }
    }
}

impl Configuration {
    pub fn format_options(&self) -> FormatOptions {
        match self.format {
            Format::Markdown => FormatOptions::Markdown(self.markdown.clone()),
            Format::Djot => FormatOptions::Djot(self.djot.clone()),
        }
    }

    pub fn search_language(&self) -> Language {
        parse_language(&self.search.language)
    }

    pub fn template() -> Self {
        let mut template = Self {
            version: Some(3),
            ..Default::default()
        };

        template.commands.insert(
            "default".into(),
            Command {
                run: "claude -p".to_string(),
                timeout_seconds: Some(120),
                ..Default::default()
            },
        );

        template.actions.insert(
            "today".into(),
            ActionDefinition::Attach(Attach {
                title: "Add Date".into(),
                key_template: "{{today}}".into(),
                document_template: "# {{today}}\n\n{{content}}\n".into(),
            }),
        );

        template.actions.insert(
            "rewrite".into(),
            ActionDefinition::Transform(
                Transform {
                    title: "Rewrite".into(),
                    command: "default".into(),
                    input_template: indoc! {r##"
                        Here's a text that I'm going to ask you to edit. The text is marked with {{context_start}}{{context_end}} tag.

                        The part you'll need to update is marked with {{update_start}}{{update_end}}.

                        {{context_start}}

                        {{context}}

                        {{context_end}}

                        - You can't replace entire text, your answer will be inserted in place of the {{update_start}}{{update_end}}. Don't include the {{context_start}}{{context_end}} and {{context_start}}{{context_end}} tags in your output.
                        - Preserve the links in the text. Do not return list item "-" or header "#" prefix

                        Your goal is to rewrite a given text to improve its clarity and readability. Ensure the language remains personable and not overly formal. Focus on simplifying language, organizing sentences logically, and removing ambiguity while maintaining a conversational tone.
                        "##}.to_string(),
                }
            ),
        );

        template.actions.insert (
            "expand".to_string(),
            ActionDefinition::Transform(
                Transform {
                    title: "Expand".to_string(),
                    command: "default".to_string(),
                    input_template: indoc! {r##"
                        Here's a text that I'm going to ask you to edit. The text is marked with {{context_start}}{{context_end}} tag.

                        The part you'll need to update is marked with {{update_start}}{{update_end}}.

                        {{context_start}}

                        {{context}}

                        {{context_end}}

                        - You can't replace entire text, your answer will be inserted in place of the {{update_start}}{{update_end}}. Don't include the {{context_start}}{{context_end}} and {{context_start}}{{context_end}} tags in your output.
                        - Preserve the links in the text. Do not return list item "-" or header "#" prefix

                        Expand the text you need to update, generate a couple paragraphs.
                        "##}.to_string(),
                }
            ),
        );

        template.actions.insert (
            "keywords".into(),
            ActionDefinition::Transform(
                Transform {
                    title: "Keywords".to_string(),
                    command: "default".to_string(),
                    input_template: indoc! {r##"
                        Here's a text that I'm going to ask you to edit. The text is marked with {{context_start}}{{context_end}} tag.

                        The part you'll need to update is marked with {{update_start}}{{update_end}}.

                        {{context_start}}

                        {{context}}

                        {{context_end}}

                        - You can't replace entire text, your answer will be inserted in place of the {{update_start}}{{update_end}}. Don't include the {{context_start}}{{context_end}} and {{context_start}}{{context_end}} tags in your output.

                        Mark most important keywords with bold using ** markdown syntax. Keep the text unchanged!
                        "##}.to_string(),
                }
            ),
        );

        template.actions.insert(
            "emoji".into(),
            ActionDefinition::Transform(
                Transform {
                    title: "Emojify".to_string(),
                    command: "default".to_string(),
                    input_template: indoc! {r##"
                        Here's a text that I'm going to ask you to edit. The text is marked with {{context_start}} {{context_end}} tags.

                        - The part you'll need to update is marked with {{update_start}} {{update_end}} tags.
                        - You can't replace entire text, your answer will be inserted in between {{update_start}} {{update_end}} tags.
                        - Add a relevant emoji one per list item (prior to list item text), header (prior to header text) or paragraph. Keep the text otherwise unchanged.
                        - Don't include the {{update_start}} {{update_end}} tags in your answer.

                        {{context_start}}

                        {{context}}

                        {{context_end}}
                        "##}.to_string(),
                }
            )
        );

        template.actions.insert(
            "sort".into(),
            ActionDefinition::Sort(Sort {
                title: "Sort A-Z".into(),
                reverse: Some(false),
            }),
        );

        template.actions.insert(
            "sort_desc".into(),
            ActionDefinition::Sort(Sort {
                title: "Sort Z-A".into(),
                reverse: Some(true),
            }),
        );

        template.actions.insert(
            "inline_section".into(),
            ActionDefinition::Inline(Inline {
                title: "Inline section".into(),
                inline_type: InlineType::Section,
                keep_target: Some(false),
            }),
        );

        template.actions.insert(
            "inline_quote".into(),
            ActionDefinition::Inline(Inline {
                title: "Inline quote".into(),
                inline_type: InlineType::Quote,
                keep_target: Some(false),
            }),
        );

        template.actions.insert(
            "extract".into(),
            ActionDefinition::Extract(Extract {
                title: "Extract".into(),
                link_type: Some(LinkType::Markdown),
                key_template: "{{id}}".into(),
            }),
        );

        template.actions.insert(
            "extract_all".into(),
            ActionDefinition::ExtractAll(ExtractAll {
                title: "Extract all subsections".into(),
                link_type: Some(LinkType::Markdown),
                key_template: "{{id}}".into(),
            }),
        );

        template.actions.insert(
            "link".into(),
            ActionDefinition::Link(Link {
                title: "Link".into(),
                link_type: Some(LinkType::Markdown),
                key_template: "{{id}}".into(),
            }),
        );

        template.templates.insert(
            "default".into(),
            NoteTemplate {
                key_template: "{{slug}}".into(),
                document_template: "# {{title}}\n\n{{content}}".into(),
            },
        );

        template
    }
}

pub fn schemas_dir() -> Result<PathBuf, String> {
    let base = env::current_dir().map_err(|e| format!("Failed to get current directory: {}", e))?;
    Ok(schemas_dir_in(&base))
}

pub fn schemas_dir_in(base: &Path) -> PathBuf {
    base.join(IWE_MARKER).join("schemas")
}

pub fn library_path_in(project_root: &Path, configuration: &Configuration) -> PathBuf {
    if configuration.library.path.is_empty() {
        project_root.to_path_buf()
    } else {
        project_root.join(&configuration.library.path)
    }
}

/// Where the transaction journal (`journal.path`) resolves to, given a
/// project root — `None` when unconfigured, the default. A relative
/// `journal.path` is resolved against `project_root` (mirroring
/// [`library_path_in`] and [`schemas_dir_in`]); an absolute one is used
/// as-is.
pub fn journal_path_in(project_root: &Path, configuration: &Configuration) -> Option<PathBuf> {
    configuration.journal.path.as_ref().map(|path| {
        let path = Path::new(path);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            project_root.join(path)
        }
    })
}

/// Whether a write to `key` is permitted under `[transactions]`'s
/// `deny`/`allow` lists. `allow`, if non-empty, is an allowlist: the write
/// is permitted iff `key` matches at least one `allow` pattern, and `deny`
/// is not consulted. Otherwise, `deny`, if non-empty, is a denylist: the
/// write is permitted iff `key` matches none of its patterns. With both
/// empty, every write is permitted — today's unrestricted behavior.
/// Pattern syntax matches [`SchemaBinding::r#match`]'s glob semantics.
pub fn write_permitted(deny: &[String], allow: &[String], key: &Key) -> bool {
    let key = key.as_str();
    if !allow.is_empty() {
        return patterns_match_raw(allow, key);
    }
    if !deny.is_empty() {
        return !patterns_match_raw(deny, key);
    }
    true
}

/// Env var name for [`TransactionOptions::deny`], read by [`load_config`]
/// after the TOML file is parsed. See [`apply_transactions_env_overlay`]
/// for the override-entirely semantics.
pub const ENV_TRANSACTIONS_DENY: &str = "IWE_TRANSACTIONS_DENY";
/// Env var name for [`TransactionOptions::allow`]. See
/// [`ENV_TRANSACTIONS_DENY`].
pub const ENV_TRANSACTIONS_ALLOW: &str = "IWE_TRANSACTIONS_ALLOW";

pub fn load_config() -> Result<Configuration, String> {
    let current_dir =
        env::current_dir().map_err(|e| format!("Failed to get current directory: {}", e))?;
    load_config_in(&current_dir)
}

/// Loads a configuration rooted at `project_root`, rather than the process
/// current directory. Services with an explicitly selected store must not
/// change the process-wide cwd just to read that store's configuration.
pub fn load_config_in(project_root: &Path) -> Result<Configuration, String> {
    let mut config_path = project_root.to_path_buf();
    config_path.push(IWE_MARKER);
    config_path.push(CONFIG_FILE_NAME);

    let mut config = if config_path.exists() {
        debug!("reading config from path: {:?}", config_path);

        let raw = read_to_string(&config_path).map_err(|e| {
            format!(
                "Failed to read config file '{}': {}",
                config_path.display(),
                e
            )
        })?;
        let configuration = migrate(&raw, project_root)?;

        let mut config = toml::from_str::<Configuration>(&configuration).map_err(|e| {
            format!(
                "Failed to parse config file '{}': {}",
                config_path.display(),
                e
            )
        })?;
        config.markdown.formatting = config.markdown.formatting.validated();
        config.djot.formatting = config.djot.formatting.validated();
        config
    } else {
        debug!("using default configuration");
        Configuration::template()
    };

    apply_transactions_env_overlay(&mut config.transactions)?;

    Ok(config)
}

/// Splits a raw `IWE_TRANSACTIONS_DENY`/`IWE_TRANSACTIONS_ALLOW` value on
/// `,`, trimming whitespace around each entry and dropping any entry that
/// is empty after trimming (e.g. from a trailing comma).
fn parse_env_pattern_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|entry| entry.trim().to_string())
        .filter(|entry| !entry.is_empty())
        .collect()
}

/// Applies the `IWE_TRANSACTIONS_DENY`/`IWE_TRANSACTIONS_ALLOW` process-env
/// overlay to a parsed [`TransactionOptions`], then fails fast if the
/// *final resolved* deny and allow are both non-empty.
///
/// Override-entirely semantics: if either env var is set to a non-empty
/// string, the resolved deny/allow pair comes entirely from the
/// environment — the file's `deny`/`allow` are both discarded (not
/// merged), and whichever of the two env vars is absent resolves to an
/// empty list. If neither env var is set, the file's parsed deny/allow
/// apply unchanged.
///
/// Fail-fast: this task's fail-fast is on the *final resolved* deny/allow
/// pair, evaluated unconditionally after the overlay is applied — not
/// gated on whether an env override actually happened. A config file that
/// already carries non-empty `deny` and `allow` with neither env var set
/// fails fast exactly the same as an env override that resolves to both
/// non-empty; the source of the conflicting values does not matter, only
/// the final state. `load_config` returns `Err` — this is the existing
/// `Result<Configuration, String>` error type, not a new error type.
/// There is no error enum on this path: the error is a `String` message
/// beginning with `"conflicting transactions override:"`, which
/// callers/tests can match on with `starts_with`.
fn apply_transactions_env_overlay(transactions: &mut TransactionOptions) -> Result<(), String> {
    let deny_env = env::var(ENV_TRANSACTIONS_DENY)
        .ok()
        .filter(|v| !v.is_empty());
    let allow_env = env::var(ENV_TRANSACTIONS_ALLOW)
        .ok()
        .filter(|v| !v.is_empty());

    let override_applied = deny_env.is_some() || allow_env.is_some();
    if override_applied {
        transactions.deny = deny_env
            .as_deref()
            .map(parse_env_pattern_list)
            .unwrap_or_default();
        transactions.allow = allow_env
            .as_deref()
            .map(parse_env_pattern_list)
            .unwrap_or_default();
    }

    if !transactions.deny.is_empty() && !transactions.allow.is_empty() {
        return Err(format!(
            "conflicting transactions override: resolved deny ({:?}) and allow ({:?}) are both non-empty; set only one of {} / {}",
            transactions.deny, transactions.allow, ENV_TRANSACTIONS_DENY, ENV_TRANSACTIONS_ALLOW
        ));
    }

    Ok(())
}

fn migrate(config: &str, project_root: &Path) -> Result<String, String> {
    let doc = config
        .parse::<DocumentMut>()
        .map_err(|e| format!("Config file is not valid TOML: {}", e))?;
    let current_version = doc
        .get("version")
        .and_then(|v| v.as_value())
        .and_then(|v| v.as_integer())
        .unwrap_or(0);

    let mut updated = config.to_string();
    let mut needs_update = false;

    // Migrate from version 0 to version 1
    if current_version < 1 {
        debug!("applying migrations from version 0 to 1");
        updated = add_default_type_to_actions(&updated);
        updated = add_default_code_actions(&updated);
        updated = set_config_version(&updated, 1);
        needs_update = true;
    }

    // Migrate from version 1 to version 2
    if current_version < 2 {
        debug!("applying migrations from version 1 to 2");
        updated = add_refs_extension_field(&updated);
        updated = add_link_action(&updated);
        updated = set_config_version(&updated, 2);
        needs_update = true;
    }

    // Migrate from version 2 to version 3
    if current_version < 3 {
        debug!("applying migrations from version 2 to 3");
        updated = migrate_v2_to_v3(&updated);
        updated = set_config_version(&updated, 3);
        needs_update = true;
    }

    if needs_update {
        debug!("configuration file migration applied");
        let mut config_path = project_root.to_path_buf();
        config_path.push(IWE_MARKER);
        config_path.push(CONFIG_FILE_NAME);

        debug!("updating configuration file");
        std::fs::write(&config_path, &updated).map_err(|e| {
            format!(
                "Failed to write config file '{}': {}",
                config_path.display(),
                e
            )
        })?;
    }

    Ok(updated)
}

fn add_default_type_to_actions(input: &str) -> String {
    let mut doc = input.parse::<DocumentMut>().expect("valid TOML");

    if let Some(Item::Table(actions)) = doc.get_mut("actions") {
        for (_, action) in actions.iter_mut() {
            if let Item::Table(action_table) = action {
                action_table.entry("type").or_insert(value("transform"));
            }
        }
    }

    doc.to_string()
}

fn add_refs_extension_field(input: &str) -> String {
    let mut doc = input.parse::<DocumentMut>().expect("valid TOML");

    if doc.get("markdown").is_none() {
        doc["markdown"] = Item::Table(toml_edit::Table::new());
    }

    if let Some(Item::Table(markdown)) = doc.get_mut("markdown") {
        if markdown.get("refs_extension").is_none() {
            markdown.insert("refs_extension", value(""));
        }
    }

    doc.to_string()
}

fn add_link_action(input: &str) -> String {
    let mut doc = input.parse::<DocumentMut>().expect("valid TOML");

    if doc.get("actions").is_none() {
        doc["actions"] = Item::Table(toml_edit::Table::new());
    }

    if let Some(Item::Table(actions)) = doc.get_mut("actions") {
        // Check if link action already exists
        let has_link = actions.iter().any(|(_, action)| {
            if let Item::Table(action_table) = action {
                if let Some(Item::Value(action_type)) = action_table.get("type") {
                    if let Some(type_str) = action_type.as_str() {
                        return type_str == "link";
                    }
                }
            }
            false
        });

        if !has_link {
            let mut link_table = toml_edit::Table::new();
            link_table.insert("type", value("link"));
            link_table.insert("title", value("Link word"));
            link_table.insert("link_type", value("markdown"));
            link_table.insert("key_template", value("{{id}}"));
            actions.insert("link", Item::Table(link_table));
        }
    }

    doc.to_string()
}

fn set_config_version(input: &str, version: i64) -> String {
    let mut doc = input.parse::<DocumentMut>().expect("valid TOML");

    doc.insert("version", value(version));

    doc.to_string()
}

fn add_default_code_actions(input: &str) -> String {
    let mut doc = input.parse::<DocumentMut>().expect("valid TOML");

    if doc.get("actions").is_none() {
        doc["actions"] = Item::Table(toml_edit::Table::new());
    }

    if let Some(Item::Table(actions)) = doc.get_mut("actions") {
        let mut has_extract = false;
        let mut has_extract_all = false;
        let mut has_inline = false;

        for (_, action) in actions.iter() {
            if let Item::Table(action_table) = action {
                if let Some(Item::Value(action_type)) = action_table.get("type") {
                    if let Some(type_str) = action_type.as_str() {
                        match type_str {
                            "extract" => has_extract = true,
                            "extract_all" => has_extract_all = true,
                            "inline" => has_inline = true,
                            _ => {}
                        }
                    }
                }
            }
        }

        if !has_extract {
            let mut extract_table = toml_edit::Table::new();
            extract_table.insert("type", value("extract"));
            extract_table.insert("title", value("Extract"));
            extract_table.insert("link_type", value("markdown"));
            extract_table.insert("key_template", value("{{id}}"));
            actions.insert("extract", Item::Table(extract_table));
        }

        if !has_extract_all {
            let mut extract_all_table = toml_edit::Table::new();
            extract_all_table.insert("type", value("extract_all"));
            extract_all_table.insert("title", value("Extract all subsections"));
            extract_all_table.insert("link_type", value("markdown"));
            extract_all_table.insert("key_template", value("{{id}}"));
            actions.insert("extract_all", Item::Table(extract_all_table));
        }

        if !has_inline {
            let mut inline_section_table = toml_edit::Table::new();
            inline_section_table.insert("type", value("inline"));
            inline_section_table.insert("title", value("Inline section"));
            inline_section_table.insert("inline_type", value("section"));
            inline_section_table.insert("keep_target", value(false));
            actions.insert("inline_section", Item::Table(inline_section_table));

            let mut inline_quote_table = toml_edit::Table::new();
            inline_quote_table.insert("type", value("inline"));
            inline_quote_table.insert("title", value("Inline quote"));
            inline_quote_table.insert("inline_type", value("quote"));
            inline_quote_table.insert("keep_target", value(false));
            actions.insert("inline_quote", Item::Table(inline_quote_table));
        }
    }

    doc.to_string()
}

pub fn migrate_v2_to_v3(input: &str) -> String {
    let mut doc = input.parse::<DocumentMut>().expect("valid TOML");

    if let Some(Item::Table(models)) = doc.remove("models") {
        let mut commands = toml_edit::Table::new();
        for (name, model) in models.iter() {
            if let Item::Table(_) = model {
                let mut cmd = toml_edit::Table::new();
                cmd.insert("run", value(""));
                commands.insert(name, Item::Table(cmd));
            }
        }
        doc.insert("commands", Item::Table(commands));
    }

    if let Some(Item::Table(actions)) = doc.get_mut("actions") {
        for (_, action) in actions.iter_mut() {
            if let Item::Table(action_table) = action {
                let is_transform = action_table
                    .get("type")
                    .and_then(|v| v.as_value())
                    .and_then(|v| v.as_str())
                    .map(|s| s == "transform")
                    .unwrap_or(false);

                if is_transform {
                    if let Some(model_val) = action_table.remove("model") {
                        action_table.insert("command", model_val);
                    }

                    if let Some(prompt_val) = action_table.remove("prompt_template") {
                        action_table.insert("input_template", prompt_val);
                    }

                    action_table.remove("context");
                }
            }
        }
    }

    doc.to_string()
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use super::*;

    #[test]
    fn unknown_top_level_table_is_rejected() {
        let source = "version = 3\n\n[schema.note]\nmatch = \"**\"\n";
        let error = toml::from_str::<Configuration>(source).unwrap_err();
        assert_eq!(
            error.to_string(),
            indoc! {r#"
                TOML parse error at line 3, column 2
                  |
                3 | [schema.note]
                  |  ^^^^^^
                unknown field `schema`, expected one of `version`, `format`, `markdown`, `djot`, `library`, `completion`, `search`, `journal`, `transactions`, `integrity`, `commit`, `commands`, `actions`, `templates`, `schemas`, `invariants`, `checkers`
            "#}
        );
    }

    #[test]
    fn unknown_schema_binding_key_is_rejected() {
        let source = "version = 3\n\n[schemas.note]\nmach = \"**\"\n";
        let error = toml::from_str::<Configuration>(source).unwrap_err();
        assert_eq!(
            error.to_string(),
            indoc! {r#"
                TOML parse error at line 4, column 1
                  |
                4 | mach = "**"
                  | ^^^^
                unknown field `mach`, expected `match`
            "#}
        );
    }

    #[test]
    fn unknown_markdown_key_is_rejected() {
        let source = "version = 3\n\n[markdown]\nrefs_extention = \".md\"\n";
        let error = toml::from_str::<Configuration>(source).unwrap_err();
        assert_eq!(
            error.to_string(),
            indoc! {r#"
                TOML parse error at line 4, column 1
                  |
                4 | refs_extention = ".md"
                  | ^^^^^^^^^^^^^^
                unknown field `refs_extention`, expected one of `refs_extension`, `refs_path`, `refs_text`, `date_format`, `time_format`, `locale`, `wiki_link_path`, `formatting`
            "#}
        );
    }

    #[test]
    fn template_configuration_round_trips() {
        let rendered = toml::to_string(&Configuration::template()).expect("serializes");
        toml::from_str::<Configuration>(&rendered).expect("parses");
    }

    #[test]
    fn write_permitted_with_no_deny_or_allow_is_unrestricted() {
        let key = Key::name("mind/anything");
        assert!(write_permitted(&[], &[], &key));
    }

    #[test]
    fn write_permitted_denies_a_matching_deny_pattern() {
        let deny = vec!["mind/**".to_string()];
        let key = Key::name("mind/notes");
        assert!(!write_permitted(&deny, &[], &key));
    }

    #[test]
    fn write_permitted_allows_a_non_matching_deny_pattern() {
        let deny = vec!["mind/**".to_string()];
        let key = Key::name("world/notes");
        assert!(write_permitted(&deny, &[], &key));
    }

    #[test]
    fn write_permitted_allows_a_matching_allow_pattern() {
        let allow = vec!["mind/**".to_string()];
        let key = Key::name("mind/notes");
        assert!(write_permitted(&[], &allow, &key));
    }

    #[test]
    fn write_permitted_denies_a_non_matching_allow_pattern() {
        let allow = vec!["mind/**".to_string()];
        let key = Key::name("world/notes");
        assert!(!write_permitted(&[], &allow, &key));
    }
}
