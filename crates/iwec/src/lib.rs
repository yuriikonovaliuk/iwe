pub mod watcher;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use chrono::Local;
use diwe::config::{
    library_path_in, schemas_dir_in, ActionDefinition, CompletionOptions, Configuration,
    MarkdownOptions, NoteTemplate, ValidationScope, DEFAULT_KEY_DATE_FORMAT,
};
use diwe::find::{DocumentFinder, FindOptions, FindOutput};
use diwe::fs::{new_for_path, new_from_hashmap};
use diwe::retrieve::{DocumentReader, RetrieveOptions, RetrieveOutput};
use diwe::schema::{
    pending_from_changes, render_reports_text, validate_documents_in, validate_pending_documents,
    validate_pending_documents_in, KeyReport,
};
use diwe::search::Bm25Index;
use diwe::search_query::{build_index, corpus_text};
use diwe::stats::{
    mutation_findings, Finding, GraphStatistics, KeyStatistics, KeyStatisticsReport,
    SimilarityIndex,
};
use diwe::tokens::Truncation;
use diwe::validating_transaction::ValidatingTransaction;
use liwe::graph::{Graph, GraphContext};
use liwe::model::node::NodePointer;
use liwe::model::tree::{Tree, TreeIter};
use liwe::model::{strip_doc_extension, Key};
use liwe::operations::{
    attach_reference, delete as op_delete, extract as op_extract, inline as op_inline, references,
    rename as op_rename, sections, select_reference, select_section, AttachTarget, Changes,
    ExtractConfig, InlineConfig, OperationError, SelectError,
};
use liwe::query::cli::parse_projection;
use liwe::query::{
    self, execute, parse_operation, strict_guard_violations, Filter, InclusionAnchor, Operation,
    OperationKind, Outcome, ProjectionBase,
};
use liwe::transaction::{NoopTransaction, Transaction, Write as TxWrite};
use liwe::write_lock::CommitLockGuard;
use minijinja::{context, Environment};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::schemars::JsonSchema;
use rmcp::service::RequestContext;
use rmcp::{prompt, prompt_handler, prompt_router, tool, tool_router, RoleServer};
use rmcp::{tool_handler, ErrorData as McpError, ServerHandler};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

fn to_json_result<T: Serialize>(output: &T) -> Result<CallToolResult, McpError> {
    let json =
        serde_json::to_string(output).map_err(|e| McpError::internal_error(e.to_string(), None))?;
    Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
}

#[derive(Serialize)]
struct TruncationNote<'a> {
    truncated: bool,
    emitted: usize,
    matched: usize,
    clipped: &'a [String],
    tokens: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    budget: Option<usize>,
    hint: &'static str,
}

fn to_json_result_with_truncation<T: Serialize>(
    output: &T,
    truncation: &Truncation,
) -> Result<CallToolResult, McpError> {
    let json =
        serde_json::to_string(output).map_err(|e| McpError::internal_error(e.to_string(), None))?;
    let mut blocks = vec![ContentBlock::text(json)];
    if truncation.is_truncated() {
        blocks.push(ContentBlock::text(truncation_note(truncation)));
    }
    Ok(CallToolResult::success(blocks))
}

fn truncation_note(truncation: &Truncation) -> String {
    let note = TruncationNote {
        truncated: true,
        emitted: truncation.emitted,
        matched: truncation.matched,
        clipped: &truncation.clipped,
        tokens: truncation.tokens,
        budget: truncation.budget,
        hint: "Output was bounded. To see more, narrow the query, raise max_tokens/limit/max_document_tokens, or re-run excluding the returned keys.",
    };
    serde_json::to_string(&note).unwrap_or_else(|_| "{\"truncated\":true}".to_string())
}

fn to_json_result_with_warnings<T: Serialize>(
    output: &T,
    warnings: &[String],
) -> Result<CallToolResult, McpError> {
    let json =
        serde_json::to_string(output).map_err(|e| McpError::internal_error(e.to_string(), None))?;
    let mut blocks = vec![ContentBlock::text(json)];
    for warning in warnings {
        blocks.push(ContentBlock::text(format!("warning: {}", warning)));
    }
    Ok(CallToolResult::success(blocks))
}

fn to_text_result(text: String) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
}

#[derive(Serialize)]
struct CreateResult {
    key: String,
    created: bool,
}

fn schema_violation_error(reports: &[KeyReport]) -> McpError {
    let mut message = String::from("schema validation failed; change rejected:\n");
    message.push_str(&render_reports_text(reports));
    McpError::invalid_params(message, Some(serde_json::json!({ "violations": reports })))
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum KeyDepthParam {
    Bare(String),
    Qualified { key: String, depth: Option<u8> },
}

impl KeyDepthParam {
    fn anchor(&self, default_depth: Option<u8>) -> InclusionAnchor {
        let (key, depth) = match self {
            KeyDepthParam::Bare(s) => (s.clone(), None),
            KeyDepthParam::Qualified { key, depth } => (key.clone(), *depth),
        };
        let raw = depth.or(default_depth);
        let max = match raw {
            None => u32::MAX,
            Some(0) => u32::MAX,
            Some(n) => u32::from(n),
        };
        InclusionAnchor::with_max(key, max)
    }
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct SelectorParams {
    #[schemars(
        description = "Restrict to candidates that are sub-documents of EVERY listed key (AND). Each entry is either a bare KEY or {key, depth}."
    )]
    #[serde(rename = "in", default)]
    pub in_: Vec<KeyDepthParam>,
    #[schemars(
        description = "Restrict to candidates that are sub-documents of AT LEAST ONE listed key (OR)."
    )]
    #[serde(default)]
    pub in_any: Vec<KeyDepthParam>,
    #[schemars(description = "Exclude candidates that are sub-documents of ANY listed key (NOT).")]
    #[serde(default)]
    pub not_in: Vec<KeyDepthParam>,
    #[schemars(
        description = "Default depth for in / in_any / not_in entries that don't specify their own depth. Omit for unbounded."
    )]
    #[serde(default)]
    pub max_depth: Option<u8>,
}

impl SelectorParams {
    pub fn is_empty(&self) -> bool {
        self.in_.is_empty()
            && self.in_any.is_empty()
            && self.not_in.is_empty()
            && self.max_depth.is_none()
    }

    pub fn to_filter(&self) -> Option<Filter> {
        if self.is_empty() {
            return None;
        }
        let mut conjuncts: Vec<Filter> = Vec::new();
        for kd in &self.in_ {
            conjuncts.push(Filter::IncludedBy(Box::new(kd.anchor(self.max_depth))));
        }
        if !self.in_any.is_empty() {
            conjuncts.push(Filter::Or(
                self.in_any
                    .iter()
                    .map(|kd| Filter::IncludedBy(Box::new(kd.anchor(self.max_depth))))
                    .collect(),
            ));
        }
        for kd in &self.not_in {
            conjuncts.push(Filter::Nor(vec![Filter::IncludedBy(Box::new(
                kd.anchor(self.max_depth),
            ))]));
        }
        Some(Filter::And(conjuncts))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindParams {
    #[schemars(description = "Fuzzy match on document title and key")]
    pub fuzzy: Option<String>,
    #[schemars(description = "Lexical (BM25) full-text match on title and body")]
    pub lexical: Option<String>,
    #[schemars(description = "Only return documents that reference this key")]
    pub refs_to: Option<String>,
    #[schemars(description = "Only return documents referenced by this key")]
    pub refs_from: Option<String>,
    #[schemars(
        description = "Maximum number of results to return. Unlimited if omitted (0 also = unlimited)."
    )]
    pub limit: Option<usize>,
    #[schemars(
        description = "Cap total projected `$content` tokens across all results. Unlimited if omitted (0 also = unlimited)."
    )]
    pub max_tokens: Option<usize>,
    #[schemars(
        description = "Cap projected `$content` tokens per result. Unlimited if omitted (0 also = unlimited)."
    )]
    pub max_document_tokens: Option<usize>,
    #[schemars(
        description = "Replacement projection (e.g. '$title,priority' or 'body=$content,parents=$includedBy'). Bare names are frontmatter fields, and project null when the document has no such field; document fields are `$`-selectors (`$key`, `$title`, `$content`, `$includedBy`, ...). Mutually exclusive with add_fields."
    )]
    pub project: Option<String>,
    #[schemars(
        description = "Additive projection: same grammar as project (bare names are frontmatter fields, `$`-selectors are the document's own), extends defaults rather than replacing. Mutually exclusive with project."
    )]
    pub add_fields: Option<String>,
    #[serde(flatten)]
    pub selector: SelectorParams,
}

impl TryFrom<FindParams> for FindOptions {
    type Error = McpError;

    fn try_from(p: FindParams) -> Result<Self, Self::Error> {
        let project = match (p.project.as_deref(), p.add_fields.as_deref()) {
            (Some(_), Some(_)) => {
                return Err(McpError::invalid_params(
                    "project and add_fields are mutually exclusive".to_string(),
                    None,
                ))
            }
            (Some(s), None) => Some(
                parse_projection(s, ProjectionBase::Empty)
                    .map_err(|e| McpError::invalid_params(e, None))?,
            ),
            (None, Some(s)) => Some(
                parse_projection(s, ProjectionBase::Document)
                    .map_err(|e| McpError::invalid_params(e, None))?,
            ),
            (None, None) => None,
        };
        Ok(FindOptions {
            fuzzy: p.fuzzy,
            lexical: p.lexical,
            refs_to: p.refs_to.map(|k| Key::name(&k)),
            refs_from: p.refs_from.map(|k| Key::name(&k)),
            filter: p.selector.to_filter(),
            limit: p.limit,
            sort: None,
            project,
            max_tokens: p.max_tokens,
            max_document_tokens: p.max_document_tokens,
        })
    }
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct ExpandParams {
    #[schemars(description = "Levels of inclusion descendants to pull in (0 = unbounded).")]
    pub includes: Option<u64>,
    #[schemars(description = "Levels of inclusion ancestors to pull in (0 = unbounded).")]
    #[serde(rename = "includedBy")]
    pub included_by: Option<u64>,
    #[schemars(description = "Hops of outbound reference links to follow (0 = unbounded).")]
    pub references: Option<u64>,
    #[schemars(description = "Hops of inbound reference links to follow (0 = unbounded).")]
    #[serde(rename = "referencedBy")]
    pub referenced_by: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RetrieveParams {
    #[schemars(
        description = "Document keys to retrieve, or the candidate set searched within when `search`/`fuzzy` is present. Can be empty when a structural selector is provided."
    )]
    #[serde(default)]
    pub keys: Vec<String>,
    #[schemars(
        description = "Seed search: BM25 full-text query over title and body. Present → the tool searches the candidate set and reads the ordered seeds."
    )]
    pub search: Option<String>,
    #[schemars(
        description = "Seed search: fuzzy query over title and key. Fuses with `search` (RRF)."
    )]
    pub fuzzy: Option<String>,
    #[schemars(
        description = "Expansion directions to follow out from each seed: object over includes / includedBy / references / referencedBy → integer depths (0 = unbounded, omitted = not followed). Expansion is doc-only when omitted."
    )]
    pub expand: Option<ExpandParams>,
    #[schemars(description = "DEPRECATED: use `expand: { includes: N }`.")]
    pub depth: Option<u8>,
    #[schemars(description = "DEPRECATED: use `expand: { includedBy: N }`.")]
    pub context: Option<u8>,
    #[schemars(description = "DEPRECATED: use `expand: { references: 1 }`.")]
    pub links: Option<bool>,
    #[schemars(description = "Include incoming inline references. Default: true")]
    pub backlinks: Option<bool>,
    #[schemars(description = "Document keys to exclude from results")]
    pub exclude: Option<Vec<String>>,
    #[schemars(
        description = "Populate the `includes` array with child document edges. Default: false"
    )]
    pub children: Option<bool>,
    #[schemars(
        description = "Cap the number of seed documents kept before expansion — top-N by relevance when searching, the first N of the selection otherwise. Unlimited if omitted (0 also = unlimited)."
    )]
    pub limit: Option<usize>,
    #[schemars(
        description = "Cap the number of documents returned after expansion, trimming periphery documents first. Unlimited if omitted (0 also = unlimited)."
    )]
    pub max_documents: Option<usize>,
    #[schemars(
        description = "Cap total content tokens across all documents. Unlimited if omitted (0 also = unlimited)."
    )]
    pub max_tokens: Option<usize>,
    #[schemars(
        description = "Cap content tokens per document. Unlimited if omitted (0 also = unlimited)."
    )]
    pub max_document_tokens: Option<usize>,
    #[serde(flatten)]
    pub selector: SelectorParams,
}

impl RetrieveParams {
    fn searching(&self) -> bool {
        self.search.is_some() || self.fuzzy.is_some()
    }

    fn validate_expand(&self) -> Result<(), String> {
        if self.expand.is_some()
            && (self.depth.is_some() || self.context.is_some() || self.links.unwrap_or(false))
        {
            return Err(
                "`expand` cannot be combined with the deprecated `depth` / `context` / `links` aliases"
                    .to_string(),
            );
        }
        Ok(())
    }

    fn expansion(&self) -> (u32, u32, u32, u32) {
        use diwe::retrieve::expand_depth;
        if let Some(e) = &self.expand {
            return (
                e.includes.map(expand_depth).unwrap_or(0),
                e.included_by.map(expand_depth).unwrap_or(0),
                e.references.map(expand_depth).unwrap_or(0),
                e.referenced_by.map(expand_depth).unwrap_or(0),
            );
        }
        (
            self.depth.map(u32::from).unwrap_or(0),
            self.context.map(u32::from).unwrap_or(0),
            if self.links.unwrap_or(false) { 1 } else { 0 },
            0,
        )
    }

    fn base_options(&self) -> RetrieveOptions {
        let (includes, included_by, references, referenced_by) = self.expansion();
        RetrieveOptions {
            includes,
            included_by,
            references,
            referenced_by,
            backlinks: self.backlinks.unwrap_or(true),
            exclude: self
                .exclude
                .clone()
                .unwrap_or_default()
                .into_iter()
                .map(|k| Key::name(&k))
                .collect::<HashSet<_>>(),
            children: self.children.unwrap_or(false),
            filter: None,
            limit: self.limit,
            max_documents: self.max_documents,
            max_tokens: self.max_tokens,
            max_document_tokens: self.max_document_tokens,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TreeParams {
    #[schemars(
        description = "Starting document keys. If empty and no selector, shows all root documents."
    )]
    pub keys: Option<Vec<String>>,
    #[schemars(description = "Maximum traversal depth. Default: 4")]
    pub depth: Option<u8>,
    #[serde(flatten)]
    pub selector: SelectorParams,
}

#[derive(Debug, Serialize)]
struct TreeNode {
    key: String,
    title: String,
    children: Vec<TreeNode>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StatsParams {
    #[schemars(
        description = "Document key for per-document stats. Omit for aggregate graph statistics"
    )]
    pub key: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ArgueParams {
    #[schemars(
        description = "Restrict the printed nodes to this document key. Standing is always computed over the whole graph"
    )]
    pub key: Option<String>,
    #[schemars(
        description = "Restrict the printed nodes to documents matching this query-language filter (inline YAML, e.g. 'type: objection, state: open')"
    )]
    pub filter: Option<String>,
    #[schemars(
        description = "Diagnose instead of list: the root cycles behind every undecided node with the moves that break them, the claims downstream of each root, the defeated claims with their reinstatement moves, and the hypotheses whose dispute waits on an observation"
    )]
    pub explain: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CheckParams {
    #[schemars(
        description = "One or more document keys to validate against their configured schema. Scoped to exactly these documents — no whole-store checkers, no cross-document invariants, no output about anything else in the graph."
    )]
    pub keys: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SquashParams {
    #[schemars(description = "Root document key to expand")]
    pub key: String,
    #[schemars(description = "Levels of references to expand. Default: 2")]
    pub depth: Option<u8>,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum CreateIfExists {
    Fail,
    Skip,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateParams {
    #[schemars(
        required,
        description = "Document key — the created document's stable identity. Derive it from stable metadata (entity name, session date), not the title wording. Subdirectory keys allowed (e.g. people/ada); do not include a file extension."
    )]
    pub key: Option<String>,
    #[schemars(
        required,
        description = "The complete document, written verbatim: the YAML frontmatter block first (when there is one), then the markdown, normally starting with a `# Title` heading. Nothing is added or moved."
    )]
    pub content: Option<String>,
    #[schemars(
        description = "Behavior when the key already exists: \"fail\" (default) reports an error, \"skip\" leaves the existing document untouched and returns created: false, which makes retries idempotent."
    )]
    pub if_exists: Option<CreateIfExists>,
    #[schemars(skip)]
    pub template: Option<String>,
    #[schemars(skip)]
    pub variables: Option<serde_json::Map<String, serde_json::Value>>,
    #[schemars(skip)]
    pub frontmatter: Option<serde_json::Map<String, serde_json::Value>>,
    #[schemars(
        description = "Transaction handle from iwe_tx_begin to stage this write into. Omit to use the implicit default transaction (today's behavior)."
    )]
    pub handle: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateParams {
    #[schemars(description = "Document key to update")]
    pub key: String,
    #[schemars(description = "New full markdown content")]
    pub content: String,
    #[schemars(
        description = "Transaction handle from iwe_tx_begin to stage this write into. Omit to use the implicit default transaction (today's behavior)."
    )]
    pub handle: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteParams {
    #[schemars(description = "Document key to delete")]
    pub key: String,
    #[schemars(description = "Preview changes without applying. Default: false")]
    pub dry_run: Option<bool>,
    #[schemars(
        description = "Transaction handle from iwe_tx_begin to stage this write into. Omit to use the implicit default transaction (today's behavior)."
    )]
    pub handle: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum QueryKind {
    Find,
    Count,
    Update,
    Delete,
}

impl From<QueryKind> for OperationKind {
    fn from(kind: QueryKind) -> Self {
        match kind {
            QueryKind::Find => OperationKind::Find,
            QueryKind::Count => OperationKind::Count,
            QueryKind::Update => OperationKind::Update,
            QueryKind::Delete => OperationKind::Delete,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct QueryParams {
    #[schemars(
        description = "Operation kind: find (read documents), count (count documents), update (mutate frontmatter and/or blocks), or delete (remove documents)."
    )]
    pub operation: QueryKind,
    #[schemars(
        description = "The operation document as YAML. Uses the IWE query + block-selection language: `filter` (with $content block membership), `project`/`addFields` ($content narrowing, $blocks, $matches), `sort`, `limit` for reads; `filter` + `update` (with block operators $replace, $replaceText, $insertBefore, $insertAfter, $append, $delete) for update; `filter` + `expect` for delete. This surface is always strict: every mutating application must carry an `expect` guard (document-level `expect`, and one per block operator)."
    )]
    pub document: String,
    #[schemars(
        description = "Preview mutations without writing to disk (update/delete only). Default: false."
    )]
    #[serde(default)]
    pub dry_run: Option<bool>,
    #[schemars(
        description = "Transaction handle from iwe_tx_begin to stage this write into (update/delete only). Omit to use the implicit default transaction (today's behavior)."
    )]
    pub handle: Option<String>,
}

#[derive(Debug, Serialize)]
struct QueryUpdateOutput {
    dry_run: bool,
    changed: Vec<ChangeEntry>,
}

#[derive(Debug, Serialize)]
struct QueryCountOutput {
    count: usize,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RenameParams {
    #[schemars(description = "Current document key")]
    pub old_key: String,
    #[schemars(description = "New document key")]
    pub new_key: String,
    #[schemars(description = "Preview changes without applying. Default: false")]
    pub dry_run: Option<bool>,
    #[schemars(
        description = "Transaction handle from iwe_tx_begin to stage this write into. Omit to use the implicit default transaction (today's behavior)."
    )]
    pub handle: Option<String>,
}

#[derive(Debug, Serialize)]
struct ChangesOutput {
    creates: Vec<ChangeEntry>,
    updates: Vec<ChangeEntry>,
    removes: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ChangeEntry {
    key: String,
    content: String,
}

impl From<&Changes> for ChangesOutput {
    fn from(c: &Changes) -> Self {
        ChangesOutput {
            creates: c
                .creates
                .iter()
                .map(|(k, v)| ChangeEntry {
                    key: k.to_string(),
                    content: v.clone(),
                })
                .collect(),
            updates: c
                .updates
                .iter()
                .map(|(k, v)| ChangeEntry {
                    key: k.to_string(),
                    content: v.clone(),
                })
                .collect(),
            removes: c.removes.iter().map(|k| k.to_string()).collect(),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExtractParams {
    #[schemars(description = "Source document key")]
    pub key: String,
    #[schemars(description = "Section title to extract (case-insensitive partial match)")]
    pub section: Option<String>,
    #[schemars(description = "Block number to extract (1-indexed, use list mode to discover)")]
    pub block: Option<usize>,
    #[schemars(
        description = "List all sections with block numbers instead of extracting. Default: false"
    )]
    pub list: Option<bool>,
    #[schemars(description = "Preview changes without applying. Default: false")]
    pub dry_run: Option<bool>,
    #[schemars(
        description = "Transaction handle from iwe_tx_begin to stage this write into. Omit to use the implicit default transaction (today's behavior)."
    )]
    pub handle: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InlineParams {
    #[schemars(description = "Document key containing the block reference")]
    pub key: String,
    #[schemars(description = "Reference key or title to inline (partial match)")]
    pub reference: Option<String>,
    #[schemars(description = "Block number to inline (1-indexed, use list mode to discover)")]
    pub block: Option<usize>,
    #[schemars(description = "List all block references instead of inlining. Default: false")]
    pub list: Option<bool>,
    #[schemars(description = "Inline as blockquote instead of section. Default: false")]
    pub as_quote: Option<bool>,
    #[schemars(description = "Keep the target document after inlining. Default: false")]
    pub keep_target: Option<bool>,
    #[schemars(description = "Preview changes without applying. Default: false")]
    pub dry_run: Option<bool>,
    #[schemars(
        description = "Transaction handle from iwe_tx_begin to stage this write into. Omit to use the implicit default transaction (today's behavior)."
    )]
    pub handle: Option<String>,
}

#[derive(Debug, Serialize)]
struct SectionEntry {
    block_number: usize,
    title: String,
}

#[derive(Debug, Serialize)]
struct ReferenceEntry {
    block_number: usize,
    key: String,
    title: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NormalizeParams {
    #[schemars(
        description = "Transaction handle from iwe_tx_begin to stage these writes into. Omit to use the implicit default transaction (today's behavior)."
    )]
    pub handle: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AttachParams {
    #[schemars(
        description = "Configured attach action(s) to attach to (e.g. 'today'). Pass one or more action names; the source is attached under each resolved target."
    )]
    #[serde(default)]
    pub to: Vec<String>,
    #[schemars(description = "Document key to attach as a block reference in the target(s)")]
    pub key: Option<String>,
    #[schemars(description = "List available attach actions instead of executing. Default: false")]
    pub list: Option<bool>,
    #[schemars(description = "Preview changes without applying. Default: false")]
    pub dry_run: Option<bool>,
    #[schemars(
        description = "Transaction handle from iwe_tx_begin to stage this write into. Omit to use the implicit default transaction (today's behavior)."
    )]
    pub handle: Option<String>,
}

#[derive(Debug, Serialize)]
struct AttachActionEntry {
    name: String,
    title: String,
    target_key: String,
}

#[derive(Debug, Serialize)]
struct ConfigResource {
    markdown: MarkdownOptions,
    library: LibraryResourceView,
    completion: CompletionOptions,
    templates: HashMap<String, NoteTemplate>,
    actions: Vec<ActionResourceView>,
}

#[derive(Debug, Serialize)]
struct LibraryResourceView {
    date_format: Option<String>,
    default_template: Option<String>,
    frontmatter_document_title: Option<String>,
    locale: Option<String>,
}

#[derive(Debug, Serialize)]
struct ActionResourceView {
    name: String,
    action_type: String,
    title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_key: Option<String>,
}

impl ConfigResource {
    fn from_config(config: &Configuration, server: &IweServer) -> Result<Self, String> {
        let actions = config
            .actions
            .iter()
            .map(|(name, action)| {
                let (action_type, title, target_key) = match action {
                    ActionDefinition::Transform(a) => ("transform", a.title.clone(), None),
                    ActionDefinition::Attach(a) => (
                        "attach",
                        a.title.clone(),
                        Some(server.render_key_template(&a.key_template)?),
                    ),
                    ActionDefinition::Sort(a) => ("sort", a.title.clone(), None),
                    ActionDefinition::Inline(a) => ("inline", a.title.clone(), None),
                    ActionDefinition::Extract(a) => ("extract", a.title.clone(), None),
                    ActionDefinition::ExtractAll(a) => ("extract_all", a.title.clone(), None),
                    ActionDefinition::Link(a) => ("link", a.title.clone(), None),
                };
                Ok(ActionResourceView {
                    name: name.clone(),
                    action_type: action_type.to_string(),
                    title,
                    target_key,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;

        Ok(Self {
            markdown: config.markdown.clone(),
            library: LibraryResourceView {
                date_format: config.library.date_format.clone(),
                default_template: config.library.default_template.clone(),
                frontmatter_document_title: config.library.frontmatter_document_title.clone(),
                locale: config.library.locale.clone(),
            },
            completion: config.completion.clone(),
            templates: config.templates.clone(),
            actions,
        })
    }
}

fn op_error_to_mcp(e: OperationError) -> McpError {
    McpError::invalid_params(e.to_string(), None)
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReviewPromptArgs {
    #[schemars(description = "Document key to review")]
    pub key: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RefactorPromptArgs {
    #[schemars(description = "Root document key to analyze for restructuring")]
    pub key: String,
}

#[derive(Clone)]
pub struct IweServer {
    graph: Arc<Mutex<Graph>>,
    base_path: Option<PathBuf>,
    project_path: Option<PathBuf>,
    config: Configuration,
    index: Arc<Mutex<Option<Bm25Index>>>,
    seen: Arc<Mutex<HashSet<Finding>>>,
    /// Every agent transaction open on this server, keyed by the opaque
    /// handle `iwe_tx_begin` returned for it — see [`OpenTransaction`]. A
    /// `std` mutex, not tokio's: it is taken from the synchronous write
    /// paths and never held across an await.
    open_txs: Arc<std::sync::Mutex<HashMap<String, OpenTransaction>>>,
}

/// The map key `open_txs` uses for a transaction begun without an
/// explicit `handle` — today's single-implicit-transaction slot,
/// preserved unchanged so no-handle callers see identical behavior
/// (including "already open" refusing a second implicit begin).
const DEFAULT_TX_HANDLE: &str = "default";

/// Every write-tool/`tx_commit`/`tx_abort` parameter struct carries an
/// optional `handle`; this resolves it to the `open_txs` map key —
/// [`DEFAULT_TX_HANDLE`] when omitted.
fn resolve_tx_handle(handle: &Option<String>) -> String {
    handle
        .clone()
        .unwrap_or_else(|| DEFAULT_TX_HANDLE.to_string())
}

/// An agent transaction (`iwe_tx_begin` … `iwe_tx_commit`/`iwe_tx_abort`).
/// While one is open every write tool stages its writes here instead of
/// committing them one at a time: the in-memory graph takes each write
/// immediately (so the agent's later reads and operations see its own
/// staged state), disk takes nothing until `iwe_tx_commit`, which
/// validates the final state as one unit, refuses it whole if it is not
/// clean or a staged key changed underneath the transaction, and lands it
/// under the store lock with a single journal record. Abort — explicit,
/// or forced by a refused commit — reloads the graph from disk, dropping
/// the staged state.
///
/// `graph` is this transaction's own private snapshot, cloned from the
/// server's live graph at `iwe_tx_begin`, so its staged writes are
/// visible to its own later calls (same handle) but invisible to every
/// other handle and to no-handle callers until this transaction commits —
/// except for the default handle, where `graph` *is* the server's shared
/// `IweServer::graph` (an `Arc` clone, not a snapshot), preserving
/// today's exact behavior: the agent's own no-handle reads see the
/// implicit transaction's staged state, as they always have.
struct OpenTransaction {
    backend: ValidatingTransaction,
    /// Every staged key's effect, in staging order; collapsed per key
    /// into the one journal record at commit ([`collapse_effects`]).
    effects: Vec<diwe::journal::KeyEffect>,
    graph: Arc<Mutex<Graph>>,
}

impl OpenTransaction {
    fn staged_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = Vec::new();
        for effect in &self.effects {
            if !keys.contains(&effect.key) {
                keys.push(effect.key.clone());
            }
        }
        keys
    }
}

/// One effect per key for a transaction's journal record, from the
/// sequence of effects its writes staged: a create later updated is a
/// create; a create later deleted never happened; anything deleted and
/// re-created is an update; otherwise the last effect stands.
fn collapse_effects(effects: &[diwe::journal::KeyEffect]) -> Vec<diwe::journal::KeyEffect> {
    use diwe::journal::Effect;
    let mut collapsed: Vec<(String, Option<Effect>)> = Vec::new();
    for effect in effects {
        match collapsed.iter_mut().find(|(key, _)| *key == effect.key) {
            None => collapsed.push((effect.key.clone(), Some(effect.effect.clone()))),
            Some((_, slot)) => {
                *slot = match (slot.take(), effect.effect.clone()) {
                    (Some(Effect::Create), Effect::Update) => Some(Effect::Create),
                    (Some(Effect::Create), Effect::Delete) => None,
                    (None, Effect::Create) => Some(Effect::Create),
                    (Some(Effect::Delete), Effect::Create) => Some(Effect::Update),
                    (_, later) => Some(later),
                };
            }
        }
    }
    collapsed
        .into_iter()
        .filter_map(|(key, effect)| {
            effect.map(|effect| diwe::journal::KeyEffect::new(&Key::name(&key), effect))
        })
        .collect()
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct TxBeginParams {
    #[schemars(
        description = "Explicit name for this transaction, so it can stay open alongside another one — pass this same value as `handle` to every write tool and to iwe_tx_commit/iwe_tx_abort that should target it. Omit to use the implicit default transaction slot (today's behavior): refused if a default transaction is already open."
    )]
    pub handle: Option<String>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct TxCommitParams {
    #[schemars(
        description = "Transaction handle to commit. Omit to commit the implicit default transaction (today's behavior)."
    )]
    pub handle: Option<String>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct TxAbortParams {
    #[schemars(
        description = "Transaction handle to abort. Omit to abort the implicit default transaction (today's behavior)."
    )]
    pub handle: Option<String>,
}

#[tool_router]
impl IweServer {
    #[tool(
        description = "Search and discover documents in the knowledge graph. Supports fuzzy text query (`query`), root filter (`roots`), direct-reference filters (`refs_to`, `refs_from`), and the structural set selector (`in` / `in_any` / `not_in` / `max_depth`) for transitive sub-document AND/OR/NOT queries with configurable depth."
    )]
    async fn iwe_find(
        &self,
        Parameters(params): Parameters<FindParams>,
    ) -> Result<CallToolResult, McpError> {
        let options: FindOptions = params.try_into()?;
        let graph = self.graph.lock().await;
        let index = (options.lexical.is_some() || options.fuzzy.is_some())
            .then(|| diwe::search_query::build_index(&graph, self.config.search_language()));
        let finder = match &index {
            Some(index) => DocumentFinder::with_index(&graph, index),
            None => DocumentFinder::new(&graph),
        };
        let output: FindOutput = finder.find(&options);
        to_json_result_with_truncation(&output.results, &output.truncation)
    }

    #[tool(
        description = "Retrieve documents from the knowledge graph. Reads the given `keys` (or search seeds) and expands the graph around them via `expand` (includes / includedBy / references / referencedBy). With `search`/`fuzzy`, seeds are found by relevance within the candidate set (keys + selector); `limit` caps the seeds before expansion and `max_documents` caps the documents returned after expansion."
    )]
    async fn iwe_retrieve(
        &self,
        Parameters(params): Parameters<RetrieveParams>,
    ) -> Result<CallToolResult, McpError> {
        params
            .validate_expand()
            .map_err(|e| McpError::invalid_params(e, None))?;
        let graph = self.graph.lock().await;
        let reader = DocumentReader::new(&graph);
        let mut options = params.base_options();

        let output: RetrieveOutput = if params.searching() {
            let key_filter = (!params.keys.is_empty()).then(|| {
                Filter::Key(query::KeyOp::In(
                    params.keys.iter().map(|k| Key::name(k)).collect(),
                ))
            });
            let selector_filter = params.selector.to_filter();
            let candidate_filter = match (selector_filter, key_filter) {
                (Some(a), Some(b)) => Some(Filter::And(vec![a, b])),
                (Some(f), None) | (None, Some(f)) => Some(f),
                (None, None) => None,
            };
            let candidates: Vec<Key> = match &candidate_filter {
                None => graph.keys(),
                Some(f) => query::evaluate(f, &graph),
            };
            let spec = query::SearchSpec::new(params.search.clone(), params.fuzzy.clone());
            let index = diwe::search_query::build_index(&graph, self.config.search_language());
            let seeds = diwe::search_query::ranked(&graph, &index, &candidates, &spec);
            reader.retrieve_many(&seeds, &options)
        } else {
            options.filter = params.selector.to_filter();
            let keys: Vec<Key> = params.keys.iter().map(|k| Key::name(k)).collect();
            reader.retrieve_many(&keys, &options)
        };

        // A requested key with no document answers with what is owed there —
        // a fill-in request — rather than nothing. Searching mode has no
        // explicit keys, so there is nothing to miss.
        let fill_ins: Vec<diwe::fill_in::FillInRequest> = if params.searching() {
            Vec::new()
        } else {
            params
                .keys
                .iter()
                .map(|k| Key::name(k))
                .filter(|key| graph.maybe_key(key).is_none())
                .filter_map(|key| diwe::fill_in::fill_in_request(&self.config, &graph, &key).ok())
                .collect()
        };
        let mut result = to_json_result_with_truncation(&output.documents, &output.truncation)?;
        if !fill_ins.is_empty() {
            let note = serde_json::json!({
                "fillIn": fill_ins,
                "hint": "These requested keys have no document. Each entry says what the store expects there: the bound schemas, the type, the required frontmatter and sections, and who already references it.",
            });
            result.content.push(ContentBlock::text(
                serde_json::to_string(&note)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?,
            ));
        }
        Ok(result)
    }

    #[tool(
        description = "View the hierarchical tree structure of the knowledge graph showing how documents are connected via block references. Supports the structural set selector (in / in_any / not_in / max_depth) — when provided, the tree roots are restricted to (or selected from) that set."
    )]
    async fn iwe_tree(
        &self,
        Parameters(params): Parameters<TreeParams>,
    ) -> Result<CallToolResult, McpError> {
        let graph = self.graph.lock().await;

        let filter = params.selector.to_filter();
        let explicit_keys: Vec<Key> = params
            .keys
            .filter(|k| !k.is_empty())
            .map(|ks| ks.iter().map(|k| Key::name(k)).collect())
            .unwrap_or_default();

        let root_keys: Vec<Key> = if let Some(f) = filter {
            let selector_set: HashSet<Key> = query::evaluate(&f, &graph).into_iter().collect();
            if explicit_keys.is_empty() {
                let mut v: Vec<Key> = selector_set.into_iter().collect();
                v.sort();
                v
            } else {
                explicit_keys
                    .into_iter()
                    .filter(|k| selector_set.contains(k))
                    .collect()
            }
        } else if !explicit_keys.is_empty() {
            explicit_keys
        } else {
            let paths = graph.paths();
            let mut keys: Vec<Key> = paths
                .iter()
                .filter(|n| n.ids().len() == 1)
                .filter_map(|n| n.first_id())
                .map(|id| (&*graph).node(id).node_key())
                .collect();
            keys.sort();
            keys.dedup();
            keys
        };

        let max_depth = params.depth.unwrap_or(4);
        let mut trees: Vec<TreeNode> = Vec::new();
        for root_key in &root_keys {
            let mut visited: HashSet<Key> = HashSet::new();
            if let Some(node) = build_tree_node(&graph, root_key, max_depth, &mut visited) {
                trees.push(node);
            }
        }
        to_json_result(&trees)
    }

    #[tool(
        description = "Get comprehensive statistics about the knowledge graph including document counts, reference patterns, broken links, and most connected documents"
    )]
    async fn iwe_stats(
        &self,
        Parameters(params): Parameters<StatsParams>,
    ) -> Result<CallToolResult, McpError> {
        let graph = self.graph.lock().await;
        if let Some(key) = params.key {
            let all_stats = KeyStatistics::from_graph(&graph);
            let stat = all_stats
                .into_iter()
                .find(|s| s.key == key)
                .ok_or_else(|| {
                    McpError::invalid_params(format!("Document '{}' not found", key), None)
                })?;
            let similar = SimilarityIndex::build(&graph, self.config.search_language())
                .similar(&Key::name(&key));
            to_json_result(&KeyStatisticsReport {
                stats: stat,
                similar_pages: similar,
            })
        } else {
            let stats = GraphStatistics::from_graph(&graph);
            to_json_result(&stats)
        }
    }

    #[tool(
        description = "Compute the dialectical standing (in, out, undecided) of every claim and objection from the objections against it and the premises it rests on — grounded semantics with deductive support. Returns nodes with attackers, premises and a 'because' chain, disputes with what decides them, and warnings (a conceded objection whose target still stands, an objection whose target is gone, an objection with a circular ground). With explain: true, returns instead the diagnosis — root cycles and the moves that break them, downstream claims, defeated claims and their reinstatement moves, hypotheses waiting on an observation. A reading, not a gate"
    )]
    async fn iwe_argue(
        &self,
        Parameters(params): Parameters<ArgueParams>,
    ) -> Result<CallToolResult, McpError> {
        let graph = self.graph.lock().await;
        let mut argument = liwe::query::argue(&graph);
        let mut selected: Option<HashSet<String>> = None;
        if let Some(key) = params.key {
            selected
                .get_or_insert_with(HashSet::new)
                .insert(Key::name(&key).to_string());
        }
        if let Some(filter) = params.filter {
            let filter = liwe::query::parse_filter_expression(&filter)
                .map_err(|e| McpError::invalid_params(format!("filter: {e}"), None))?;
            selected.get_or_insert_with(HashSet::new).extend(
                liwe::query::evaluate(&filter, &graph)
                    .into_iter()
                    .map(|k| k.to_string()),
            );
        }
        if params.explain.unwrap_or(false) {
            let mut diagnosis = liwe::query::diagnose(&argument);
            if let Some(selected) = &selected {
                diagnosis.select(selected);
            }
            return to_json_result(&diagnosis);
        }
        if let Some(selected) = selected {
            argument.nodes.retain(|n| selected.contains(&n.key));
            argument.disputes.retain(|d| selected.contains(&d.key));
            argument.warnings.retain(|w| selected.contains(&w.key));
        }
        to_json_result(&argument)
    }

    #[tool(
        description = "Validate one or more documents by key against their configured schema — per-document rules only (frontmatter shape, link-target types, token budget, required sections), the same as `iwe schema validate -k KEY -f json`, run only against exactly the keys given here. Fast, no whole-store scan. Does NOT run whole-store-only checkers or cross-document invariants (e.g. a parent stage's state relative to its children's) — those need the unscoped CLI `iwe schema validate` (which the pre-commit hook already runs before every commit). Use this after writing or editing a document to check just that write, instead of a whole-store validate pass. Returns one {key, ok, violations} object per requested key, in order; a key matching no document also reports ok: true — this checks 'no violations found', not 'the document exists'."
    )]
    async fn iwe_check(
        &self,
        Parameters(params): Parameters<CheckParams>,
    ) -> Result<CallToolResult, McpError> {
        let graph = self.graph.lock().await;
        let requested: Vec<Key> = params.keys.iter().map(|k| Key::name(k)).collect();
        let base_path = self.base_path.as_deref().ok_or_else(|| {
            McpError::internal_error("this server has no store on disk to validate against", None)
        })?;
        // A key that doesn't exist in the graph is skipped before it ever
        // reaches validate_documents_in: a schema's `match` glob (often
        // "**") can select it by name alone, and the document-building
        // path below that assumes an existing node hangs rather than
        // erroring on one that isn't there. Trivially "no violations" —
        // there's nothing to violate — and reported as such below.
        let schemas_dir = schemas_dir_in(base_path);
        let existing: Vec<Key> = requested.iter().filter(|k| graph.has_key(k)).cloned().collect();
        let run = validate_documents_in(&schemas_dir, &self.config, &graph, &existing, true)
            .map_err(|errors| McpError::internal_error(errors.join("; "), None))?;
        let results: Vec<serde_json::Value> = requested
            .iter()
            .map(|key| match run.reports.iter().find(|r| &r.key == key) {
                Some(report) => serde_json::json!({
                    "key": key.to_string(),
                    "ok": false,
                    "violations": report.violations,
                }),
                None => serde_json::json!({ "key": key.to_string(), "ok": true }),
            })
            .collect();
        to_json_result(&results)
    }

    #[tool(
        description = "Expand all block references into a single flat markdown document. Useful for export or generating a complete view of a document tree"
    )]
    async fn iwe_squash(
        &self,
        Parameters(params): Parameters<SquashParams>,
    ) -> Result<CallToolResult, McpError> {
        let graph = self.graph.lock().await;
        let key = Key::name(&params.key);
        let depth = params.depth.unwrap_or(2);

        if (&*graph).get_node_id(&key).is_none() {
            return Err(McpError::invalid_params(
                format!("Document '{}' not found", params.key),
                None,
            ));
        }

        let squashed: Tree = (&*graph).squash(&key, depth);
        let mut patch = Graph::new();
        patch.build_key_from_iter(&key, TreeIter::new(&squashed));
        let content = patch.export_key(&key).unwrap_or_default();
        to_text_result(content)
    }

    #[tool(
        description = "Create a document at an explicit `key` from the complete markdown in `content` — frontmatter block first (when there is one), then the title heading and the body. The content is written verbatim; the server adds nothing and moves nothing. Derive the key from stable metadata (entity name, session date), not the title wording. Pass if_exists: \"skip\" to make retries idempotent. Stats warnings (dangling links, orphans, similar pages) may ride the result; resolve them before ending the session."
    )]
    async fn iwe_create(
        &self,
        Parameters(params): Parameters<CreateParams>,
    ) -> Result<CallToolResult, McpError> {
        if params.content.is_some() && params.template.is_some() {
            return Err(McpError::invalid_params(
                "'content' and 'template' are mutually exclusive: content mode writes the document you pass, template mode composes it from a named template".to_string(),
                None,
            ));
        }
        if params.template.is_some() || params.variables.is_some() || params.frontmatter.is_some() {
            return Err(McpError::invalid_params(
                "template mode is not yet supported; pass the complete document in 'content'"
                    .to_string(),
                None,
            ));
        }

        let key_name = match &params.key {
            Some(k) => {
                if strip_doc_extension(k) != k.as_str() {
                    return Err(McpError::invalid_params(
                        format!("Key '{}' must not include a file extension", k),
                        None,
                    ));
                }
                let key = Key::name(k);
                if key.as_str().is_empty() {
                    return Err(McpError::invalid_params(
                        "Key must not be empty".to_string(),
                        None,
                    ));
                }
                key.to_string()
            }
            None => {
                return Err(McpError::invalid_params(
                    "'key' is required: it is the created document's stable identity".to_string(),
                    None,
                ))
            }
        };

        let markdown = match params.content {
            Some(content) if !content.trim().is_empty() => content,
            _ => {
                return Err(McpError::invalid_params(
                    "'content' is required: pass the complete document, frontmatter and title heading included".to_string(),
                    None,
                ))
            }
        };

        let key = Key::name(&key_name);
        let graph_arc = self.tx_graph(&params.handle)?;
        let mut graph = graph_arc.lock().await;

        if (&*graph).get_node_id(&key).is_some() || self.document_file_exists(&key) {
            return match params.if_exists {
                Some(CreateIfExists::Skip) => to_json_result_with_warnings(
                    &CreateResult {
                        key: key_name,
                        created: false,
                    },
                    &[],
                ),
                _ => Err(McpError::invalid_params(
                    format!("Document '{}' already exists", key_name),
                    None,
                )),
            };
        }

        self.ensure_schema_clean(&[(key.clone(), markdown.clone())])?;

        // Write-permission (e.g. EXT-FREEZE) is checked inside `write_file`;
        // run it before mutating the in-memory graph so a rejection leaves
        // both graph and disk untouched rather than just disk.
        self.write_file(&key, &markdown, &params.handle)
            .map_err(|message| McpError::invalid_params(message, None))?;
        graph.insert_document(key.clone(), markdown.clone());

        let warnings = self
            .stats_warnings(
                &graph,
                std::slice::from_ref(&key),
                &[],
                std::slice::from_ref(&key),
            )
            .await;

        to_json_result_with_warnings(
            &CreateResult {
                key: key_name,
                created: true,
            },
            &warnings,
        )
    }

    #[tool(
        description = "Update the full markdown content of an existing document. Stats warnings (dangling links, orphans, similar pages) may ride the result; resolve them before ending the session."
    )]
    async fn iwe_update(
        &self,
        Parameters(params): Parameters<UpdateParams>,
    ) -> Result<CallToolResult, McpError> {
        let key = Key::name(&params.key);
        let graph_arc = self.tx_graph(&params.handle)?;
        let mut graph = graph_arc.lock().await;

        if (&*graph).get_node_id(&key).is_none() {
            return Err(McpError::invalid_params(
                format!("Document '{}' not found", params.key),
                None,
            ));
        }

        let previous_title = (&*graph)
            .get_key_title(&key)
            .unwrap_or_else(|| params.key.clone());

        self.ensure_schema_clean(&[(key.clone(), params.content.clone())])?;

        // Write-permission (e.g. EXT-FREEZE) is checked inside `write_file`;
        // run it before mutating the in-memory graph so a rejection leaves
        // both graph and disk untouched rather than just disk.
        self.write_file(&key, &params.content, &params.handle)
            .map_err(|message| McpError::invalid_params(message, None))?;
        graph.update_document(key.clone(), params.content.clone());

        let new_title = (&*graph)
            .get_key_title(&key)
            .unwrap_or_else(|| params.key.clone());

        let warnings = self
            .stats_warnings(
                &graph,
                std::slice::from_ref(&key),
                &[],
                std::slice::from_ref(&key),
            )
            .await;

        #[derive(Serialize)]
        struct UpdateResult {
            key: String,
            previous_title: String,
            new_title: String,
        }
        to_json_result_with_warnings(
            &UpdateResult {
                key: params.key,
                previous_title,
                new_title,
            },
            &warnings,
        )
    }

    #[tool(
        description = "Delete a document from the knowledge graph. All block references and inline links to this document in other documents are cleaned up. Stats warnings (dangling links, orphans, similar pages) may ride the result; resolve them before ending the session."
    )]
    async fn iwe_delete(
        &self,
        Parameters(params): Parameters<DeleteParams>,
    ) -> Result<CallToolResult, McpError> {
        let key = Key::name(&params.key);
        let graph_arc = self.tx_graph(&params.handle)?;
        let mut graph = graph_arc.lock().await;
        let changes = op_delete(&graph, &key).map_err(op_error_to_mcp)?;

        let mut warnings = Vec::new();
        if !params.dry_run.unwrap_or(false) {
            self.ensure_schema_clean(&pending_from_changes(&changes))?;
            self.write_changes(&changes, &params.handle)
                .map_err(|message| McpError::invalid_params(message, None))?;
            Self::apply_changes(&mut graph, &changes);
            warnings = self.stats_after_delete(&graph, &changes).await;
        }

        to_json_result_with_warnings(&ChangesOutput::from(&changes), &warnings)
    }

    #[tool(
        description = "Run an IWE query/block-selection operation document. `find` and `count` read; `update` mutates frontmatter and blocks (operators $replace, $replaceText, $insertBefore, $insertAfter, $append, $delete); `delete` removes documents. Membership uses the `$content` filter operator; reads project `$content` narrowing, `$blocks`, and `$matches`. Always strict: every mutating application must carry an `expect` guard (document-level `expect` plus one per block operator). Use `find` with `$blocks`/`$matches` to locate targets and learn counts before mutating. Update/delete results may carry stats warnings (dangling links, orphans, similar pages); resolve them before ending the session."
    )]
    async fn iwe_query(
        &self,
        Parameters(params): Parameters<QueryParams>,
    ) -> Result<CallToolResult, McpError> {
        let kind: OperationKind = params.operation.into();
        let op = parse_operation(&params.document, kind)
            .map_err(|e| McpError::invalid_params(format!("invalid operation: {}", e), None))?;

        let violations = strict_guard_violations(&op);
        if !violations.is_empty() {
            return Err(McpError::invalid_params(
                format!(
                    "MCP block operations run strict: every mutating application must carry an `expect` guard; missing: {}. \
                     State the expected count — 1 for a precision edit, {{ min: 1 }} for a bulk edit that must match, {{ min: 0 }} when zero is acceptable.",
                    violations.join(", ")
                ),
                None,
            ));
        }

        let dry_run = params.dry_run.unwrap_or(false);
        let graph_arc = self.tx_graph(&params.handle)?;
        let mut graph = graph_arc.lock().await;

        let index = match &op {
            Operation::Find(find) if find.search.is_some() => Some(
                diwe::search_query::build_index(&graph, self.config.search_language()),
            ),
            _ => None,
        };

        match &op {
            Operation::Find(find) => {
                let outcome = diwe::search_query::execute(&op, &graph, index.as_ref())
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                let Outcome::Find { matches } = outcome else {
                    unreachable!("find operation yields a find outcome")
                };
                let documents: Vec<_> = matches.into_iter().map(|m| m.document).collect();
                let mut warnings = Vec::new();
                if let Some(spec) = &find.search {
                    if index
                        .as_ref()
                        .map(|idx| diwe::search_query::lexical_has_no_terms(idx, spec))
                        .unwrap_or(false)
                    {
                        warnings.push(diwe::search_query::no_terms_warning(spec));
                    }
                }
                to_json_result_with_warnings(&documents, &warnings)
            }
            Operation::Count(_) => {
                let outcome = execute(&op, &graph)
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                let Outcome::Count(count) = outcome else {
                    unreachable!("count operation yields a count outcome")
                };
                to_json_result(&QueryCountOutput { count })
            }
            Operation::Update(_) => {
                let outcome = execute(&op, &graph)
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                let Outcome::Update { changes } = outcome else {
                    unreachable!("update operation yields an update outcome")
                };
                let changed: Vec<ChangeEntry> = changes
                    .iter()
                    .map(|(key, content)| ChangeEntry {
                        key: key.to_string(),
                        content: content.clone(),
                    })
                    .collect();
                let mut warnings = Vec::new();
                if !dry_run {
                    self.ensure_schema_clean(&changes)?;
                    for (key, content) in &changes {
                        // Write-permission (e.g. EXT-FREEZE) is checked
                        // inside `write_file`; run it before mutating the
                        // in-memory graph so a rejection leaves both graph
                        // and disk untouched rather than just disk.
                        self.write_file(key, content, &params.handle)
                            .map_err(|message| McpError::invalid_params(message, None))?;
                        graph.update_document(key.clone(), content.clone());
                    }
                    let touched: Vec<Key> = changes.iter().map(|(key, _)| key.clone()).collect();
                    warnings = self.stats_warnings(&graph, &touched, &[], &touched).await;
                }
                to_json_result_with_warnings(&QueryUpdateOutput { dry_run, changed }, &warnings)
            }
            Operation::Delete(_) => {
                let outcome = execute(&op, &graph)
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                let Outcome::Delete { removed } = outcome else {
                    unreachable!("delete operation yields a delete outcome")
                };
                let mut combined = Changes::default();
                for key in &removed {
                    let changes = op_delete(&graph, key).map_err(op_error_to_mcp)?;
                    combined.merge(changes);
                }
                let mut warnings = Vec::new();
                if !dry_run {
                    self.ensure_schema_clean(&pending_from_changes(&combined))?;
                    self.write_changes(&combined, &params.handle)
                        .map_err(|message| McpError::invalid_params(message, None))?;
                    Self::apply_changes(&mut graph, &combined);
                    warnings = self.stats_after_delete(&graph, &combined).await;
                }
                to_json_result_with_warnings(&ChangesOutput::from(&combined), &warnings)
            }
        }
    }

    #[tool(
        description = "Rename a document key. All block references and inline links across the entire graph are updated to point to the new key"
    )]
    async fn iwe_rename(
        &self,
        Parameters(params): Parameters<RenameParams>,
    ) -> Result<CallToolResult, McpError> {
        let old_key = Key::name(&params.old_key);
        let new_key = Key::name(&params.new_key);
        let graph_arc = self.tx_graph(&params.handle)?;
        let mut graph = graph_arc.lock().await;
        let changes = op_rename(&graph, &old_key, &new_key).map_err(op_error_to_mcp)?;

        if !params.dry_run.unwrap_or(false) {
            self.ensure_schema_clean(&pending_from_changes(&changes))?;
            self.write_changes(&changes, &params.handle)
                .map_err(|message| McpError::invalid_params(message, None))?;
            Self::apply_changes(&mut graph, &changes);
        }

        to_json_result(&ChangesOutput::from(&changes))
    }

    #[tool(
        description = "Extract a section from a document into a new standalone document. The original section is replaced with a block reference. Use list mode to discover sections first"
    )]
    async fn iwe_extract(
        &self,
        Parameters(params): Parameters<ExtractParams>,
    ) -> Result<CallToolResult, McpError> {
        let source_key = Key::name(&params.key);
        let graph_arc = self.tx_graph(&params.handle)?;
        let mut graph = graph_arc.lock().await;

        if (&*graph).get_node_id(&source_key).is_none() {
            return Err(McpError::invalid_params(
                format!("Document '{}' not found", params.key),
                None,
            ));
        }

        let tree = (&*graph).collect(&source_key);

        if params.list.unwrap_or(false) {
            let sections: Vec<SectionEntry> = sections(&tree)
                .into_iter()
                .map(|section| SectionEntry {
                    block_number: section.number,
                    title: section.title,
                })
                .collect();
            return to_json_result(&sections);
        }

        let section = match select_section(&tree, params.section.as_deref(), params.block) {
            Ok(section) => section,
            Err(SelectError::NotFound(query)) => {
                return Err(McpError::invalid_params(
                    format!("No section matches '{}'", query),
                    None,
                ))
            }
            Err(SelectError::Ambiguous(query, matches)) => {
                return Err(McpError::invalid_params(
                    format!(
                        "Multiple sections match '{}': {}",
                        query,
                        matches
                            .iter()
                            .map(|section| section.title.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    None,
                ))
            }
            Err(SelectError::OutOfRange(block, len)) => {
                return Err(McpError::invalid_params(
                    format!("Block number {} out of range (1-{})", block, len),
                    None,
                ))
            }
            Err(SelectError::NoSelector) => {
                return Err(McpError::invalid_params(
                    "Must specify section, block, or list",
                    None,
                ))
            }
        };

        let section_id = section.id;

        let config = ExtractConfig::default();
        let changes = op_extract(
            &graph,
            &source_key,
            section_id,
            &config,
            std::time::SystemTime::now(),
        )
        .map_err(op_error_to_mcp)?;

        if !params.dry_run.unwrap_or(false) {
            self.ensure_schema_clean(&pending_from_changes(&changes))?;
            self.write_changes(&changes, &params.handle)
                .map_err(|message| McpError::invalid_params(message, None))?;
            Self::apply_changes(&mut graph, &changes);
        }

        to_json_result(&ChangesOutput::from(&changes))
    }

    #[tool(
        description = "Replace a block reference with the actual content of the referenced document. Use list mode to discover block references first"
    )]
    async fn iwe_inline(
        &self,
        Parameters(params): Parameters<InlineParams>,
    ) -> Result<CallToolResult, McpError> {
        let source_key = Key::name(&params.key);
        let graph_arc = self.tx_graph(&params.handle)?;
        let mut graph = graph_arc.lock().await;

        if (&*graph).get_node_id(&source_key).is_none() {
            return Err(McpError::invalid_params(
                format!("Document '{}' not found", params.key),
                None,
            ));
        }

        let tree = (&*graph).collect(&source_key);

        if params.list.unwrap_or(false) {
            let refs: Vec<ReferenceEntry> = references(&tree)
                .into_iter()
                .map(|reference| ReferenceEntry {
                    block_number: reference.number,
                    key: reference.key.to_string(),
                    title: reference.title,
                })
                .collect();
            return to_json_result(&refs);
        }

        let reference = match select_reference(&tree, params.reference.as_deref(), params.block) {
            Ok(reference) => reference,
            Err(SelectError::NotFound(query)) => {
                return Err(McpError::invalid_params(
                    format!("No reference matches '{}'", query),
                    None,
                ))
            }
            Err(SelectError::Ambiguous(query, matches)) => {
                return Err(McpError::invalid_params(
                    format!(
                        "Multiple references match '{}': {}",
                        query,
                        matches
                            .iter()
                            .map(|reference| reference.key.to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    None,
                ))
            }
            Err(SelectError::OutOfRange(block, len)) => {
                return Err(McpError::invalid_params(
                    format!("Block number {} out of range (1-{})", block, len),
                    None,
                ))
            }
            Err(SelectError::NoSelector) => {
                return Err(McpError::invalid_params(
                    "Must specify reference, block, or list",
                    None,
                ))
            }
        };

        let ref_id = reference.id;

        let inline_type = if params.as_quote.unwrap_or(false) {
            diwe::config::InlineType::Quote
        } else {
            diwe::config::InlineType::Section
        };

        let config = InlineConfig {
            inline_type,
            keep_target: params.keep_target.unwrap_or(false),
        };

        let changes = op_inline(&graph, &source_key, ref_id, &config).map_err(op_error_to_mcp)?;

        if !params.dry_run.unwrap_or(false) {
            self.ensure_schema_clean(&pending_from_changes(&changes))?;
            self.write_changes(&changes, &params.handle)
                .map_err(|message| McpError::invalid_params(message, None))?;
            Self::apply_changes(&mut graph, &changes);
        }

        to_json_result(&ChangesOutput::from(&changes))
    }

    #[tool(
        description = "Normalize all document formatting across the knowledge graph. Re-parses and re-writes all documents to ensure consistent formatting"
    )]
    async fn iwe_normalize(
        &self,
        Parameters(params): Parameters<NormalizeParams>,
    ) -> Result<CallToolResult, McpError> {
        let graph_arc = self.tx_graph(&params.handle)?;
        let graph = graph_arc.lock().await;
        let state = graph.export();
        let original_count = state.len();

        let mut changed = 0usize;
        if self.base_path.is_some() {
            for (key_str, normalized_content) in &state {
                let key = Key::name(key_str);
                if self.read_file(&key).as_deref() != Some(normalized_content.as_str()) {
                    self.write_file(&key, normalized_content, &params.handle)
                        .map_err(|message| McpError::invalid_params(message, None))?;
                    changed += 1;
                }
            }
        }

        #[derive(Serialize)]
        struct NormalizeResult {
            total: usize,
            normalized: usize,
        }
        to_json_result(&NormalizeResult {
            total: original_count,
            normalized: changed,
        })
    }

    #[tool(
        description = "Attach a document as a block reference in one or more target documents determined by configured attach actions. Each target key is derived from the action's key_template (e.g. daily/{{today}}). The `to` field accepts a list of action names; the source is attached under each resolved target. Targets that already contain the source are silently skipped. Use list mode to discover available attach actions."
    )]
    async fn iwe_attach(
        &self,
        Parameters(params): Parameters<AttachParams>,
    ) -> Result<CallToolResult, McpError> {
        if params.list.unwrap_or(false) {
            let mut entries: Vec<AttachActionEntry> = Vec::new();
            for (name, action) in &self.config.actions {
                if let ActionDefinition::Attach(attach) = action {
                    let target_key =
                        self.render_key_template(&attach.key_template)
                            .map_err(|e| {
                                McpError::invalid_params(format!("action '{}': {}", name, e), None)
                            })?;
                    entries.push(AttachActionEntry {
                        name: name.clone(),
                        title: attach.title.clone(),
                        target_key,
                    });
                }
            }
            return to_json_result(&entries);
        }

        if params.to.is_empty() {
            return Err(McpError::invalid_params(
                "'to' is required when not in list mode (pass one or more action names)"
                    .to_string(),
                None,
            ));
        }
        let source_key_str = params.key.as_deref().ok_or_else(|| {
            McpError::invalid_params("'key' is required when not in list mode".to_string(), None)
        })?;

        let graph_arc = self.tx_graph(&params.handle)?;
        let mut graph = graph_arc.lock().await;

        let source_key = Key::name(source_key_str);
        if (&*graph).get_node_id(&source_key).is_none() {
            return Err(McpError::invalid_params(
                format!("Document '{}' not found", source_key_str),
                None,
            ));
        }

        let reference_text = (&*graph)
            .get_key_title(&source_key)
            .unwrap_or_else(|| source_key_str.to_string());

        let mut combined = Changes::new();

        for action_name in &params.to {
            let attach = match self.config.actions.get(action_name) {
                Some(ActionDefinition::Attach(a)) => a,
                Some(_) => {
                    return Err(McpError::invalid_params(
                        format!("Action '{}' is not an attach action", action_name),
                        None,
                    ));
                }
                None => {
                    return Err(McpError::invalid_params(
                        format!("Action '{}' not found", action_name),
                        None,
                    ));
                }
            };

            let target_key = Key::name(&self.render_key_template(&attach.key_template).map_err(
                |e| McpError::invalid_params(format!("action '{}': {}", action_name, e), None),
            )?);

            match attach_reference(&graph, &target_key, &source_key, &reference_text) {
                AttachTarget::AlreadyAttached => continue,
                AttachTarget::Update(content) => {
                    combined.add_update(target_key.clone(), content);
                }
                AttachTarget::Create(body) => {
                    let document = self
                        .render_document_template(&attach.document_template, &body)
                        .map_err(|e| {
                            McpError::invalid_params(
                                format!("action '{}': {}", action_name, e),
                                None,
                            )
                        })?;
                    combined.add_create(target_key.clone(), document);
                }
            }
        }

        if !params.dry_run.unwrap_or(false) {
            self.ensure_schema_clean(&pending_from_changes(&combined))?;
            self.write_changes(&combined, &params.handle)
                .map_err(|message| McpError::invalid_params(message, None))?;
            Self::apply_changes(&mut graph, &combined);
        }

        to_json_result(&ChangesOutput::from(&combined))
    }

    #[tool(
        description = "Open a transaction: until iwe_tx_commit, every write tool (create, update, delete, rename, extract, inline, attach, query --set, normalize) stages its writes instead of committing them, while your own reads see the staged state. Commit validates the final state as one unit and lands it atomically with one journal record; abort discards it. Use it for multi-document changes that must land together or not at all. Pass an explicit `handle` to keep several transactions open at once (e.g. interleaving two changes); omit it for the implicit default transaction (today's single-transaction behavior). Needs `[transactions] validate` set for the store."
    )]
    async fn iwe_tx_begin(
        &self,
        Parameters(params): Parameters<TxBeginParams>,
    ) -> Result<CallToolResult, McpError> {
        // The graph lock serializes against in-flight writes so a begin
        // never lands between a tool's stage and its graph mutation. Only
        // taken for the default handle's sake (see `tx_graph`'s doc
        // comment) — an explicit handle's transaction gets its own
        // private graph snapshot below and never touches this lock.
        let key = resolve_tx_handle(&params.handle);
        let _graph = self.graph.lock().await;
        let mut open = self.open_txs.lock().expect("open transaction lock");
        if let Some(tx) = open.get(&key) {
            return Err(McpError::invalid_params(
                format!(
                    "a transaction is already open with {} staged write(s) ({}); commit or abort it first",
                    tx.effects.len(),
                    tx.staged_keys().join(", ")
                ),
                None,
            ));
        }
        let Some(mut backend) = self.validating_backend() else {
            return Err(McpError::invalid_params(
                "transactions are not enabled for this store: set `[transactions] validate` to \
                 \"affected-set\" or \"full\", or set `[transactions] deny`/`allow` \
                 (write-scope enforcement also constructs a validating backend)"
                    .to_string(),
                None,
            ));
        };
        backend
            .begin()
            .map_err(|e| McpError::internal_error(format!("transaction failed to begin: {e}"), None))?;
        let scope = backend.scope();
        // Default: alias the server's own shared graph, so this
        // transaction's writes land exactly where they always have and
        // no-handle reads see them, unchanged from today. Explicit
        // handle: a private snapshot, isolated from every other handle
        // (and from the default) until this transaction's own commit.
        let tx_graph = if key == DEFAULT_TX_HANDLE {
            self.graph.clone()
        } else {
            Arc::new(Mutex::new((*_graph).clone()))
        };
        open.insert(
            key.clone(),
            OpenTransaction {
                backend,
                effects: Vec::new(),
                graph: tx_graph,
            },
        );
        drop(open);

        #[derive(Serialize)]
        struct TxBegun {
            status: &'static str,
            validate: ValidationScope,
            handle: String,
        }
        to_json_result(&TxBegun {
            status: "open",
            validate: scope,
            handle: key,
        })
    }

    #[tool(
        description = "Commit the open transaction: validates the final state of every staged write as one unit (schemas, invariants, checkers, to the store's configured scope), refuses the whole transaction if it is not clean or a staged document changed on disk since it was staged — in which case nothing lands and the staged state is discarded — and otherwise lands every write atomically with a single journal record. Pass the `handle` returned by iwe_tx_begin to commit a specific transaction; omit it for the implicit default one."
    )]
    async fn iwe_tx_commit(
        &self,
        Parameters(params): Parameters<TxCommitParams>,
    ) -> Result<CallToolResult, McpError> {
        let key = resolve_tx_handle(&params.handle);
        let is_default = key == DEFAULT_TX_HANDLE;
        let mut graph = self.graph.lock().await;
        let taken = self.open_txs.lock().expect("open transaction lock").remove(&key);
        let Some(mut tx) = taken else {
            return Err(McpError::invalid_params(
                "no transaction is open; call iwe_tx_begin first".to_string(),
                None,
            ));
        };
        let keys = tx.staged_keys();
        match tx.backend.commit_or_abort() {
            Ok(()) => {
                // The default handle's writes already landed directly on
                // `self.graph` as each write tool ran (today's exact
                // behavior). An explicit handle staged into its own
                // private graph instead, so land its final per-key
                // content into the shared graph now that it is committed
                // to disk, using each key's collapsed effect to decide
                // create/update/delete — the same collapsing `tx_commit`
                // already does for the journal record.
                if !is_default {
                    let tx_graph = tx.graph.lock().await;
                    for effect in collapse_effects(&tx.effects) {
                        let effect_key = Key::name(&effect.key);
                        match effect.effect {
                            diwe::journal::Effect::Delete => {
                                graph.remove_document(effect_key);
                            }
                            diwe::journal::Effect::Create | diwe::journal::Effect::Update => {
                                if let Some(content) = tx_graph.export_key(&effect_key) {
                                    if (&*graph).get_node_id(&effect_key).is_some() {
                                        graph.update_document(effect_key, content);
                                    } else {
                                        graph.insert_document(effect_key, content);
                                    }
                                }
                            }
                        }
                    }
                }
                self.record_journal_commit(collapse_effects(&tx.effects), None);
                drop(graph);
                #[derive(Serialize)]
                struct TxCommitted {
                    status: &'static str,
                    keys: Vec<String>,
                }
                to_json_result(&TxCommitted {
                    status: "committed",
                    keys,
                })
            }
            Err(message) => {
                drop(graph);
                // The store-wide commit lock's whole acquire/fencing/apply
                // window is owned by `ValidatingTransaction::commit`
                // itself (5-iwe-t3) — the one backend construction both
                // the CLI and this server's agent transactions share — so
                // a lock timeout or a fencing failure surfaces here as
                // this `commit_or_abort()` call's own refusal message,
                // not as a separate acquire/check this method makes
                // itself. Unlike every other refusal (a schema violation,
                // a staged key changed on disk), a lock-related refusal
                // means the commit attempt never got a fair try at
                // validating or applying this transaction's state at
                // all — so, alone among refusals, it must not discard the
                // transaction: put it back so a caller can retry
                // `iwe_tx_commit` once the lock is free. Matched on the
                // backend's `ValidationFailure::LockTimeout` /
                // `LockStale` wording (`crates/diwe/src/
                // validating_transaction.rs`) since `commit_or_abort`
                // only returns a rendered `String`, not the failure enum.
                let lock_related = message
                    .contains("timed out waiting to acquire the store's commit lock")
                    || message.contains("commit lock hold was superseded");
                if lock_related {
                    // `commit_or_abort()`'s own failure handling already
                    // called the backend's `abort()`, clearing its
                    // pending writes (needed to leave the backend
                    // reusable) — so a bare reinsert would hand a retry a
                    // transaction with nothing left to commit. Re-stage
                    // every collapsed effect from this transaction's own
                    // graph (its record of the final per-key state this
                    // transaction stages) so the backend has the same
                    // pending writes to try again.
                    {
                        let tx_graph = tx.graph.lock().await;
                        for effect in collapse_effects(&tx.effects) {
                            let effect_key = Key::name(&effect.key);
                            let write = match effect.effect {
                                diwe::journal::Effect::Delete => TxWrite::Remove(effect_key),
                                diwe::journal::Effect::Create | diwe::journal::Effect::Update => {
                                    match tx_graph.export_key(&effect_key) {
                                        Some(content) => TxWrite::Put(effect_key, content),
                                        None => continue,
                                    }
                                }
                            };
                            let _ = tx.backend.write(write);
                        }
                    }
                    self.open_txs
                        .lock()
                        .expect("open transaction lock")
                        .insert(key, tx);
                    return Err(McpError::invalid_params(
                        format!(
                            "transaction commit refused: {message}; nothing was written and the transaction remains open for retry"
                        ),
                        None,
                    ));
                }
                // The default handle's staged writes live on the shared
                // graph, so discarding them means reloading the whole
                // graph from disk — exactly today's behavior. An
                // explicit handle's staged writes lived only in its own
                // private graph (dropped with `tx` here), so the shared
                // graph — and any other still-open transaction's own
                // staged state — was never touched and needs no reload.
                if is_default {
                    self.reload_graph_from_disk().await;
                }
                Err(McpError::invalid_params(
                    format!(
                        "transaction refused; nothing was written and the staged changes to {} were discarded: {message}",
                        if keys.is_empty() { "no documents".to_string() } else { keys.join(", ") }
                    ),
                    None,
                ))
            }
        }
    }

    #[tool(
        description = "Abort the open transaction: discards every staged write and reloads the graph from disk. Nothing is written. Pass the `handle` returned by iwe_tx_begin to abort a specific transaction; omit it for the implicit default one."
    )]
    async fn iwe_tx_abort(
        &self,
        Parameters(params): Parameters<TxAbortParams>,
    ) -> Result<CallToolResult, McpError> {
        let key = resolve_tx_handle(&params.handle);
        let is_default = key == DEFAULT_TX_HANDLE;
        let taken = {
            let _graph = self.graph.lock().await;
            self.open_txs.lock().expect("open transaction lock").remove(&key)
        };
        let Some(mut tx) = taken else {
            return Err(McpError::invalid_params(
                "no transaction is open".to_string(),
                None,
            ));
        };
        let keys = tx.staged_keys();
        let _ = tx.backend.abort();
        // Only the default handle's staged writes ever touched the
        // shared graph (aliased to it); an explicit handle's staged
        // writes lived solely in its own private graph, dropped with
        // `tx` — nothing else to discard, and no other open transaction
        // (including the default) is disturbed.
        if is_default {
            self.reload_graph_from_disk().await;
        }

        #[derive(Serialize)]
        struct TxAborted {
            status: &'static str,
            discarded: Vec<String>,
        }
        to_json_result(&TxAborted {
            status: "aborted",
            discarded: keys,
        })
    }
}

fn build_tree_node(
    graph: &Graph,
    key: &Key,
    max_depth: u8,
    visited: &mut HashSet<Key>,
) -> Option<TreeNode> {
    graph.get_node_id(key)?;

    let title = graph.get_ref_text(key).unwrap_or_default();
    let key_str = key.to_string();

    if visited.contains(key) {
        return Some(TreeNode {
            key: key_str,
            title,
            children: vec![],
        });
    }
    visited.insert(key.clone());

    let children = if max_depth > 1 {
        let ref_node_ids = graph.get_inclusion_edges_in(key);
        let mut refs: Vec<Key> = ref_node_ids
            .iter()
            .filter_map(|id| graph.graph_node(*id).ref_key())
            .collect();
        refs.sort();
        refs.into_iter()
            .filter_map(|ref_key| build_tree_node(graph, &ref_key, max_depth - 1, visited))
            .collect()
    } else {
        vec![]
    };

    Some(TreeNode {
        key: key_str,
        title,
        children,
    })
}

#[prompt_router]
impl IweServer {
    #[prompt(
        name = "explore",
        description = "Start exploring the knowledge graph. Provides an overview of size, structure, root entry points, broken links, and orphaned documents"
    )]
    async fn explore(&self) -> Result<GetPromptResult, McpError> {
        let graph = self.graph.lock().await;
        let stats = GraphStatistics::from_graph(&graph);
        let stats_json = serde_json::to_string_pretty(&stats).unwrap_or_else(|_| "{}".to_string());

        let messages = vec![PromptMessage::new_text(
            Role::User,
            format!(
                "Here is an overview of the IWE knowledge graph.\n\n## Statistics\n\n```json\n{}\n```\n\nExplore the graph using iwe_retrieve to read documents, iwe_find to search, and iwe_tree to navigate the structure.",
                stats_json
            ),
        )];

        Ok(GetPromptResult::new(messages).with_description("Overview of the IWE knowledge graph"))
    }

    #[prompt(
        name = "review",
        description = "Review a specific document within its graph context — its content, parents, children, and backlinks"
    )]
    async fn review(
        &self,
        Parameters(args): Parameters<ReviewPromptArgs>,
    ) -> Result<GetPromptResult, McpError> {
        let graph = self.graph.lock().await;
        let key = Key::name(&args.key);
        let reader = DocumentReader::new(&graph);
        let output = reader.retrieve(
            &key,
            &RetrieveOptions {
                includes: 2,
                included_by: 2,
                backlinks: true,
                ..Default::default()
            },
        );
        let json =
            serde_json::to_string_pretty(&output.documents).unwrap_or_else(|_| "[]".to_string());

        let messages = vec![PromptMessage::new_text(
            Role::User,
            format!(
                "Review this document and its context in the knowledge graph:\n\n```json\n{}\n```\n\nConsider: Is it well-placed in the graph? Are there missing links? Is the content clear and well-structured? What sections might be extracted into separate documents?",
                json
            ),
        )];

        Ok(GetPromptResult::new(messages)
            .with_description(format!("Review of document '{}'", args.key)))
    }

    #[prompt(
        name = "refactor",
        description = "Analyze a section of the knowledge graph and suggest restructuring using extract, inline, and rename operations"
    )]
    async fn refactor(
        &self,
        Parameters(args): Parameters<RefactorPromptArgs>,
    ) -> Result<GetPromptResult, McpError> {
        let graph = self.graph.lock().await;
        let key = Key::name(&args.key);
        let reader = DocumentReader::new(&graph);
        let output = reader.retrieve(
            &key,
            &RetrieveOptions {
                includes: 3,
                included_by: 1,
                backlinks: true,
                ..Default::default()
            },
        );
        let json =
            serde_json::to_string_pretty(&output.documents).unwrap_or_else(|_| "[]".to_string());

        let messages = vec![PromptMessage::new_text(
            Role::User,
            format!(
                "Analyze this document tree and suggest restructuring:\n\n```json\n{}\n```\n\nIdentify documents that are too large (should be extracted with iwe_extract), too small (should be inlined with iwe_inline), poorly named (should be renamed with iwe_rename), or missing connections. Propose a sequence of operations to improve the structure.",
                json
            ),
        )];

        Ok(GetPromptResult::new(messages)
            .with_description(format!("Refactoring analysis for '{}'", args.key)))
    }
}

#[tool_handler]
#[prompt_handler]
impl ServerHandler for IweServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .enable_resources()
                .build(),
        )
        .with_server_info(Implementation::new("iwe", env!("CARGO_PKG_VERSION")))
        .with_instructions(
            "IWE knowledge graph server. Tools: iwe_find, iwe_retrieve, iwe_tree, iwe_stats, iwe_squash, iwe_create, iwe_update, iwe_delete, iwe_query, iwe_rename, iwe_extract, iwe_inline, iwe_normalize, iwe_attach, iwe_argue, iwe_check. Prompts: explore, review, refactor. Resources: iwe://documents/{key}, iwe://tree, iwe://stats, iwe://config."
                .to_string(),
        )
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        let graph = self.graph.lock().await;
        let mut resources = vec![
            Resource::new("iwe://tree", "tree")
                .with_description("Full document tree structure")
                .with_mime_type("application/json"),
            Resource::new("iwe://stats", "stats")
                .with_description("Aggregate graph statistics")
                .with_mime_type("application/json"),
            Resource::new("iwe://config", "config")
                .with_description("Project configuration: markdown options, templates, actions")
                .with_mime_type("application/json"),
        ];

        for key in graph.keys().iter().take(100) {
            let title = (&*graph)
                .get_key_title(key)
                .unwrap_or_else(|| key.to_string());
            resources.push(
                Resource::new(format!("iwe://documents/{}", key), title)
                    .with_mime_type("text/markdown"),
            );
        }

        Ok(ListResourcesResult::with_all_items(resources))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        let uri = &request.uri;
        let graph = self.graph.lock().await;

        if uri == "iwe://tree" {
            let paths = graph.paths();
            let mut root_keys: Vec<Key> = paths
                .iter()
                .filter(|n| n.ids().len() == 1)
                .filter_map(|n| n.first_id())
                .map(|id| (&*graph).node(id).node_key())
                .collect();
            root_keys.sort();
            root_keys.dedup();

            let mut trees: Vec<TreeNode> = Vec::new();
            for root_key in &root_keys {
                let mut visited: HashSet<Key> = HashSet::new();
                if let Some(node) = build_tree_node(&graph, root_key, 4, &mut visited) {
                    trees.push(node);
                }
            }
            let json = serde_json::to_string_pretty(&trees)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(
                ReadResourceResult::new(vec![ResourceContents::text(json, uri.clone())]).into(),
            );
        }

        if uri == "iwe://stats" {
            let stats = GraphStatistics::from_graph(&graph);
            let json = serde_json::to_string_pretty(&stats)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(
                ReadResourceResult::new(vec![ResourceContents::text(json, uri.clone())]).into(),
            );
        }

        if uri == "iwe://config" {
            let config_view = ConfigResource::from_config(&self.config, self)
                .map_err(|e| McpError::internal_error(e, None))?;
            let json = serde_json::to_string_pretty(&config_view)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(
                ReadResourceResult::new(vec![ResourceContents::text(json, uri.clone())]).into(),
            );
        }

        if let Some(key_str) = uri.strip_prefix("iwe://documents/") {
            let key = Key::name(key_str);
            let content = graph
                .get_document(&key)
                .ok_or_else(|| {
                    McpError::resource_not_found(format!("Document '{}' not found", key_str), None)
                })?
                .to_string();
            return Ok(
                ReadResourceResult::new(vec![ResourceContents::text(content, uri.clone())]).into(),
            );
        }

        Err(McpError::resource_not_found(
            format!("Unknown resource: {}", uri),
            None,
        ))
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        Ok(ListResourceTemplatesResult::with_all_items(vec![
            ResourceTemplate::new("iwe://documents/{key}", "document")
                .with_description("A document in the knowledge graph by key")
                .with_mime_type("text/markdown"),
        ]))
    }
}

impl IweServer {
    pub fn new(project_path: &str, configuration: &Configuration) -> Self {
        let root = PathBuf::from_str(project_path).expect("valid path");
        let library = library_path_in(&root, configuration);
        let state = new_for_path(&library, configuration.format);
        let graph = Graph::from_state(
            &state,
            false,
            configuration.format_options(),
            configuration.library.frontmatter_document_title.clone(),
        );
        Self {
            graph: Arc::new(Mutex::new(graph)),
            base_path: Some(library),
            project_path: Some(root),
            config: configuration.clone(),
            index: Arc::new(Mutex::new(None)),
            seen: Arc::new(Mutex::new(HashSet::new())),
            open_txs: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    pub fn from_documents(documents: Vec<(&str, &str)>) -> Self {
        Self::from_documents_with_config(documents, Configuration::default())
    }

    pub fn from_documents_with_config(documents: Vec<(&str, &str)>, config: Configuration) -> Self {
        let state = new_from_hashmap(
            documents
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<HashMap<String, String>>(),
        );
        let graph = Graph::from_state(&state, true, MarkdownOptions::default(), None);
        Self {
            graph: Arc::new(Mutex::new(graph)),
            base_path: None,
            project_path: None,
            config,
            index: Arc::new(Mutex::new(None)),
            seen: Arc::new(Mutex::new(HashSet::new())),
            open_txs: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    fn apply_changes(graph: &mut Graph, changes: &Changes) {
        for key in &changes.removes {
            graph.remove_document(key.clone());
        }
        for (key, markdown) in &changes.creates {
            graph.insert_document(key.clone(), markdown.clone());
        }
        for (key, markdown) in &changes.updates {
            graph.update_document(key.clone(), markdown.clone());
        }
    }

    fn ensure_schema_clean(&self, docs: &[(Key, String)]) -> Result<(), McpError> {
        let result = match &self.project_path {
            Some(root) => validate_pending_documents_in(&schemas_dir_in(root), &self.config, docs),
            None => validate_pending_documents(&self.config, docs),
        };
        match result {
            Ok(run) if run.reports.is_empty() => Ok(()),
            Ok(run) => Err(schema_violation_error(&run.reports)),
            Err(errors) => Err(McpError::invalid_params(
                format!("schema configuration error: {}", errors.join("; ")),
                None,
            )),
        }
    }

    async fn stats_warnings(
        &self,
        graph: &Graph,
        upserts: &[Key],
        removes: &[Key],
        targets: &[Key],
    ) -> Vec<String> {
        let mut index_guard = self.index.lock().await;
        if index_guard.is_none() {
            *index_guard = Some(build_index(graph, self.config.search_language()));
        } else {
            let index = index_guard.as_mut().expect("index present");
            for key in removes {
                index.remove(key);
            }
            for key in upserts {
                index.upsert(key.clone(), corpus_text(graph, key));
            }
        }
        let index = index_guard.as_ref().expect("index present");
        let findings = mutation_findings(graph, index, targets);
        drop(index_guard);

        let mut seen = self.seen.lock().await;
        findings
            .into_iter()
            .filter(|finding| seen.insert(finding.clone()))
            .map(|finding| finding.render())
            .collect()
    }

    async fn stats_after_delete(&self, graph: &Graph, changes: &Changes) -> Vec<String> {
        let neighbors: Vec<Key> = changes.updates.iter().map(|(key, _)| key.clone()).collect();
        self.stats_warnings(graph, &neighbors, &changes.removes, &[])
            .await
    }

    fn document_path(&self, key: &Key) -> Option<PathBuf> {
        let base_path = self.base_path.as_ref()?;
        let extension = self.config.format.extension();
        Some(base_path.join(format!("{}.{}", key, extension)))
    }

    fn document_file_exists(&self, key: &Key) -> bool {
        self.document_path(key)
            .is_some_and(|file_path| file_path.exists())
    }

    /// Mirrors `iwe::main::enforce_write_permission`'s call to the shared
    /// write-permission site (`diwe::permissions::
    /// check_write_permission_for_content`), reached here from every iwec
    /// write instead of from a CLI command handler. Unconditional — never
    /// gated behind a strict/non-strict branch — because write-permission
    /// evaluation must fire identically regardless of invocation mode.
    ///
    /// Called from inside `write_file`'s transaction bracket (after
    /// `begin()`, before the actual filesystem write) rather than before it
    /// starts: per `m2/design-transactions`, a write-permission rejection
    /// must be able to drive the transaction into its failed/aborted state
    /// rather than the transaction never having begun.
    ///
    /// Resolved from `self.project_path`, not `self.base_path`/cwd: same
    /// `schemas_dir_in(root)` precedent `ensure_schema_clean` already uses,
    /// so the schemas directory used to read `mutable:` rules matches the
    /// one used to validate `--strict`/MCP schema compliance, and so tests
    /// can run in-process against a temp directory without the server's
    /// actual cwd having to match it.
    ///
    /// Returns the rejection's own `Display` message (document key + rule;
    /// both `WritePermissionError::Frozen` and
    /// `WritePermissionError::PropertyImmutable` carry their own key)
    /// rather than the error value itself, since every caller only needs to
    /// surface it — as an `McpError` (`write_file`'s callers) or as a log
    /// line (`write_changes`, whose `diwe::fs::apply_changes` hook has no
    /// room to propagate a message through to an MCP tool response).
    ///
    /// Reads `key`'s prior on-disk content itself (via `self.document_path`,
    /// `None` if it doesn't exist yet) before evaluating — the fix for M2's
    /// freeze-bypass defect (`m2/design-freeze-semantics`): a predicate fed
    /// only the outgoing `content` can't enforce a rule about a transition
    /// (e.g. "frozen, unless this write's sole effect is lifting freeze").
    fn enforce_write_permission(&self, key: &Key, content: &str) -> Result<(), String> {
        let prior_content = self
            .document_path(key)
            .and_then(|path| std::fs::read_to_string(path).ok());
        let result = match &self.project_path {
            Some(root) => diwe::permissions::check_write_permission_for_content_in(
                &self.config,
                &schemas_dir_in(root),
                key,
                content,
                prior_content.as_deref(),
                diwe::permissions::WriteOperation::Write,
            ),
            None => diwe::permissions::check_write_permission_for_content(
                &self.config,
                key,
                content,
                prior_content.as_deref(),
                diwe::permissions::WriteOperation::Write,
            ),
        };
        result.map_err(|rejected| rejected.to_string())
    }

    // WP-12 (iwe_create/iwe_update/iwe_delete/iwe_query/iwe_rename/
    // iwe_extract/iwe_inline/iwe_attach that write content, not just remove
    // it) and WP-13 (iwe_normalize) share this single call site; wrapping
    // it here covers both. Routed through the no-op Transaction interface,
    // with the write-permission check running inside the transaction
    // bracket (after `begin()`, before the actual filesystem write) so a
    // rejection aborts the transaction instead of the write never having
    // been attempted.
    //
    // T10 (EXT-FREEZE): a rejection is surfaced to the caller (as
    // `Err(message)`) rather than silently discarded, so a frozen
    // document's write is both refused on disk and reported as a tool
    // error rather than a false success — combined here with T6's generic
    // transaction-backend wiring (`write_file_with`, parameterized over
    // `TX` so tests can drive it with a `RecordingTransaction`) and T6's
    // commit-gates-persist ordering (`commit` is attempted before the real
    // filesystem write, not after, so a commit refusal actually prevents
    // the write rather than merely being noticed once it already landed).
    fn write_file(&self, key: &Key, content: &str, handle: &Option<String>) -> Result<(), String> {
        let existed = self.document_file_exists(key);
        let effect = if existed {
            diwe::journal::Effect::Update
        } else {
            diwe::journal::Effect::Create
        };
        let tx_key = resolve_tx_handle(handle);
        if self.document_path(key).is_some() {
            let mut open = self.open_txs.lock().expect("open transaction lock");
            if let Some(tx) = open.get_mut(&tx_key) {
                // Staged, not committed: permission is judged now (a
                // refused write leaves the transaction open and
                // untouched), validation at `iwe_tx_commit`.
                self.enforce_write_permission(key, content)?;
                if tx
                    .backend
                    .write(TxWrite::Put(key.clone(), content.to_string()))
                    .is_err()
                {
                    return Err(format!("write rejected by transaction backend for '{key}'"));
                }
                tx.effects.push(diwe::journal::KeyEffect::new(key, effect));
                return Ok(());
            }
            // An explicit handle that names no open transaction is a
            // caller error, not a silent fall-through to an unstaged
            // direct write — only the default (omitted) handle falls
            // through, matching today's exact no-handle behavior.
            if tx_key != DEFAULT_TX_HANDLE {
                return Err(format!(
                    "no transaction is open for handle '{tx_key}'; call iwe_tx_begin first"
                ));
            }
        }
        match self.validating_backend() {
            // 6-t1: a `NoopTransaction` write never goes through a
            // validating backend's lock, so — mirroring the CLI's
            // `acquire_cli_commit_lock` branch — it takes the store-wide
            // commit lock for the whole commit attempt itself: acquired
            // at the start, fenced immediately before the filesystem
            // write inside `write_file_with`, held through the journal
            // record and the `[commit]` trigger, released on every exit
            // (the `drop(guard)` below runs regardless of which arm the
            // closure took). This is what presents those journal records
            // to the trigger inside a commit-lock window.
            None => match self.project_path.clone() {
                Some(root) => {
                    let guard = liwe::write_lock::acquire_commit_lock(&root)
                        .map_err(|error| format!("write refused: {error}"))?;
                    let result = (|| {
                        self.write_file_with(key, content, Some(&guard), NoopTransaction::new)?;
                        self.record_journal_commit(
                            vec![diwe::journal::KeyEffect::new(key, effect)],
                            Some(&guard),
                        );
                        Ok(())
                    })();
                    drop(guard);
                    result
                }
                None => {
                    self.write_file_with(key, content, None, NoopTransaction::new)?;
                    self.record_journal_commit(
                        vec![diwe::journal::KeyEffect::new(key, effect)],
                        None,
                    );
                    Ok(())
                }
            },
            Some(tx) => {
                self.write_file_validated(key, content, tx)?;
                self.record_journal_commit(vec![diwe::journal::KeyEffect::new(key, effect)], None);
                Ok(())
            }
        }
    }

    /// Stages the whole of `changes` on the open transaction named by
    /// `handle` (the default/implicit one when omitted), if there is one
    /// — `Some(result)` — or reports `None` for the caller to commit them
    /// itself. Permission is judged per key now, as
    /// [`ValidatingTransaction::apply_changes`] does; a refusal leaves the
    /// transaction as it was.
    fn stage_changes(&self, changes: &Changes, handle: &Option<String>) -> Option<Result<(), String>> {
        let root = self.project_path.as_ref().or(self.base_path.as_ref())?;
        let tx_key = resolve_tx_handle(handle);
        let mut open = self.open_txs.lock().expect("open transaction lock");
        let tx = open.get_mut(&tx_key)?;
        let schemas_dir = schemas_dir_in(root);
        let check = |key: &Key, content: &str, operation: diwe::permissions::WriteOperation| {
            let prior = self
                .document_path(key)
                .and_then(|path| std::fs::read_to_string(path).ok());
            diwe::permissions::check_write_permission_for_content_in(
                &self.config,
                &schemas_dir,
                key,
                content,
                prior.as_deref(),
                operation,
            )
            .map_err(|rejected| rejected.to_string())
        };
        for key in &changes.removes {
            if self.document_file_exists(key) {
                if let Err(message) = check(key, "", diwe::permissions::WriteOperation::Delete) {
                    return Some(Err(message));
                }
            }
        }
        for (key, markdown) in changes.creates.iter().chain(changes.updates.iter()) {
            if let Err(message) = check(key, markdown, diwe::permissions::WriteOperation::Write) {
                return Some(Err(message));
            }
        }
        for key in &changes.removes {
            if tx.backend.write(TxWrite::Remove(key.clone())).is_err() {
                return Some(Err(format!("write rejected by transaction backend for '{key}'")));
            }
        }
        for (key, markdown) in changes.creates.iter().chain(changes.updates.iter()) {
            if tx
                .backend
                .write(TxWrite::Put(key.clone(), markdown.clone()))
                .is_err()
            {
                return Some(Err(format!("write rejected by transaction backend for '{key}'")));
            }
        }
        tx.effects.extend(diwe::fs::journal_effects_for(changes));
        Some(Ok(()))
    }

    /// Replaces the in-memory graph with what is on disk — the move that
    /// drops an aborted transaction's staged state. The search index is
    /// rebuilt lazily on the next write.
    async fn reload_graph_from_disk(&self) {
        let Some(base_path) = self.base_path.as_ref() else {
            return;
        };
        let state = new_for_path(base_path, self.config.format);
        let reloaded = Graph::from_state(
            &state,
            false,
            self.config.format_options(),
            self.config.library.frontmatter_document_title.clone(),
        );
        *self.graph.lock().await = reloaded;
        *self.index.lock().await = None;
    }

    /// The graph a write tool should read and mutate for `handle`: the
    /// server's shared, live `self.graph` for the default (omitted)
    /// handle — identical to today's single-transaction behavior, so a
    /// no-handle caller's own reads keep seeing its staged writes — or
    /// the named transaction's own private snapshot for an explicit
    /// handle, kept invisible to every other handle until its commit
    /// (T7: handle-keyed per-transaction isolation). Errors if an
    /// explicit handle names no open transaction, rather than silently
    /// falling back to the shared graph and bypassing the isolation the
    /// caller asked for by naming a handle at all.
    fn tx_graph(&self, handle: &Option<String>) -> Result<Arc<Mutex<Graph>>, McpError> {
        let key = resolve_tx_handle(handle);
        if key == DEFAULT_TX_HANDLE {
            return Ok(self.graph.clone());
        }
        let open = self.open_txs.lock().expect("open transaction lock");
        match open.get(&key) {
            Some(tx) => Ok(tx.graph.clone()),
            None => Err(McpError::invalid_params(
                format!("no transaction is open for handle '{key}'; call iwe_tx_begin first"),
                None,
            )),
        }
    }

    /// The validating backend `[transactions]` asks for over this server's
    /// store: either `[transactions] validate` set to a non-`None` scope,
    /// or `[transactions] deny`/`allow` non-empty (the write-scope
    /// enforcement gate). `None` only when the section is fully at its
    /// default (`validate = none`, `deny = []`, `allow = []`), or this
    /// server has no store on disk to validate against.
    fn validating_backend(&self) -> Option<ValidatingTransaction> {
        let base_path = self.base_path.as_ref()?;
        let root = self.project_path.as_ref().unwrap_or(base_path);
        ValidatingTransaction::for_config(&self.config, base_path, root)
    }

    /// [`Self::write_file_with`] for a backend that applies the write
    /// itself at `commit()` — the store is touched inside the backend's
    /// lock and nowhere else, so a concurrent writer can never be
    /// clobbered by a second, unlocked write of the same bytes.
    fn write_file_validated(
        &self,
        key: &Key,
        content: &str,
        tx: ValidatingTransaction,
    ) -> Result<(), String> {
        if self.document_path(key).is_none() {
            return Ok(());
        }
        tx.put_one(key, content, |_prior| self.enforce_write_permission(key, content))
    }

    /// One transaction over the whole of `changes` — removes, creates and
    /// updates staged together and validated as one final state, so a
    /// rename or extract whose intermediate states dangle is judged on
    /// where it ends up (`m2/design-transactions`). Write permission is
    /// checked per key inside the bracket, as `diwe::fs::apply_changes_with`
    /// does.
    fn write_changes_validated(
        &self,
        changes: &Changes,
        tx: ValidatingTransaction,
    ) -> Result<(), String> {
        let Some(root) = self.project_path.as_ref().or(self.base_path.as_ref()) else {
            return Ok(());
        };
        let schemas_dir = schemas_dir_in(root);
        tx.apply_changes(changes, |key, content, prior, operation| {
            diwe::permissions::check_write_permission_for_content_in(
                &self.config,
                &schemas_dir,
                key,
                content,
                prior,
                operation,
            )
            .map_err(|rejected| rejected.to_string())
        })
    }

    /// Where this server's transaction journal (`journal.path`, see
    /// [`diwe::journal`]) resolves to, if configured — `None` both when
    /// unconfigured (the default) and when this server has no
    /// `project_path` (the in-memory-only construction used by some
    /// tests), the same way write-permission resolution already falls
    /// back when there is no project root to resolve a relative path
    /// against.
    fn journal_path(&self) -> Option<PathBuf> {
        let root = self.project_path.as_ref()?;
        diwe::config::journal_path_in(root, &self.config)
    }

    /// Appends one journal record for `effects` if a journal is
    /// configured — the single call every write path (`write_file`,
    /// `write_changes_with`) makes after its own writes have all
    /// succeeded, so a rejected or aborted write never reaches this call
    /// at all — and runs the `[commit]` trigger exactly when that append
    /// produced a record. `hold` is the commit-lock guard currently live
    /// at this call site: `Some` from `write_file`/`write_changes`'s
    /// `NoopTransaction` branches (which hold the store's commit lock
    /// across the write and the record), `None` from the validated
    /// branches, whose backend took and released its own hold inside
    /// `commit()` — the trigger then acquires a fresh hold for its own
    /// window.
    fn record_journal_commit(
        &self,
        effects: Vec<diwe::journal::KeyEffect>,
        hold: Option<&CommitLockGuard>,
    ) {
        let root = self.project_path.as_deref();
        diwe::commit_trigger::record_commit_and_trigger(
            self.journal_path().as_deref(),
            effects,
            root.map(|root| diwe::commit_trigger::CommitTriggerContext {
                options: &self.config.commit,
                store_root: root,
            }),
            hold,
        );
    }

    /// Generic core of [`Self::write_file`], parameterized over the
    /// transaction backend via a factory (`new_tx`) called once to build
    /// the transaction used for this write. `write_file` (every MCP tool
    /// call's actual write path) always calls this with
    /// `NoopTransaction::new`, per AB9's "transactions default to no-op
    /// passthrough" — that default is unchanged.
    ///
    /// `pub`, not merely `pub(crate)`/private: this is the interface
    /// boundary itself, and M2's fix-wave found it *not* genuinely routed
    /// through by any production path outside this module — every real
    /// call site hardcoded `NoopTransaction::new` with no override
    /// reachable from outside `iwec`, which is a defect distinct from
    /// [`AffectedSetTransaction`](diwe::validating_transaction::AffectedSetTransaction)
    /// itself correctly staying dormant as the default. Making this `pub`
    /// (mirroring `diwe::fs::apply_changes_with` /
    /// `diwe::fs::write_store_at_path_with` / `iwe::new::write_document_with`,
    /// which were already `pub`) is what lets a future caller
    /// install a different backend without rewriting this call site. T6's tests already call this with
    /// a factory that builds a call-recording stub
    /// (`liwe::transaction::RecordingTransaction`), to prove this MCP call
    /// site actually drives `begin`/`write`/`commit`/`abort`, rather than
    /// merely compiling against the trait.
    ///
    /// `lock_guard`: `Some(guard)` when this write lands under
    /// `write_file`'s own commit-lock hold (its `NoopTransaction`
    /// branch) — `check_fencing` is re-checked against it immediately
    /// before the actual filesystem write below, the same 5-iwe-t4 idiom
    /// `diwe::fs::apply_changes_with` / `write_store_at_path_with`
    /// follow. `None` for every other caller (T6's tests, driving a stub
    /// `Transaction` that never went through that lock at all).
    pub fn write_file_with<TX: Transaction>(
        &self,
        key: &Key,
        content: &str,
        lock_guard: Option<&CommitLockGuard>,
        mut new_tx: impl FnMut() -> TX,
    ) -> Result<(), String> {
        let Some(file_path) = self.document_path(key) else {
            return Ok(());
        };
        if let Some(parent) = file_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let mut tx = new_tx();
        tx.begin()
            .map_err(|_| format!("transaction backend failed to begin for '{key}'"))?;

        if tx
            .write(TxWrite::Put(key.clone(), content.to_string()))
            .is_err()
        {
            let _ = tx.commit();
            let _ = tx.abort();
            return Err(format!("write rejected by transaction backend for '{key}'"));
        }
        if let Err(message) = self.enforce_write_permission(key, content) {
            let _ = tx.abort();
            return Err(message);
        }
        if tx.commit().is_err() {
            let _ = tx.abort();
            return Err(format!(
                "write rejected: transaction backend refused to commit for '{key}'"
            ));
        }
        // Fencing check, immediately before the irreversible step (the
        // actual filesystem write below) — mirrors the checks
        // `diwe::fs::apply_changes_with` runs before each of its on-disk
        // steps: a `NoopTransaction` write never goes through
        // `ValidatingTransaction`'s own `commit_locked` fence, so it needs
        // its own guard against a slow holder's write landing after its
        // lock was reclaimed.
        if let Some(guard) = lock_guard {
            guard
                .check_fencing()
                .map_err(|e| format!("write refused: {e}"))?;
        }
        match std::fs::write(&file_path, content) {
            Ok(()) => Ok(()),
            Err(error) => Err(format!(
                "Failed to write '{}': {}",
                file_path.display(),
                error
            )),
        }
    }

    fn read_file(&self, key: &Key) -> Option<String> {
        std::fs::read_to_string(self.document_path(key)?).ok()
    }

    // WP-12: `apply_changes` now takes the write-permission check as a hook
    // run inside its own per-key transaction bracket (after `begin()`,
    // before the actual filesystem write/remove), so this no longer needs
    // to run the check separately before entering `apply_changes` — the
    // CLI's delete/rename/extract/inline commands (which call
    // `diwe::fs::apply_changes` through their own wrapper in `main.rs`)
    // wire the identical hook, so enforcement is consistent across both
    // binaries without re-implementing it here.
    fn write_changes(&self, changes: &Changes, handle: &Option<String>) -> Result<(), String> {
        if let Some(staged) = self.stage_changes(changes, handle) {
            return staged;
        }
        // Same guard as `write_file`: an explicit handle naming no open
        // transaction is a caller error, not a silent unstaged write.
        if resolve_tx_handle(handle) != DEFAULT_TX_HANDLE {
            return Err(format!(
                "no transaction is open for handle '{}'; call iwe_tx_begin first",
                resolve_tx_handle(handle)
            ));
        }
        match self.validating_backend() {
            // 6-t1: commit-lock coverage for the `NoopTransaction` write
            // path, exactly as `write_file` takes it (see there): the
            // write and the journal record + `[commit]` trigger all land
            // inside one held commit-lock window, fenced per on-disk step
            // by `diwe::fs::apply_changes_with`, released on every exit.
            None => match self.project_path.clone() {
                Some(root) => {
                    let guard = liwe::write_lock::acquire_commit_lock(&root)
                        .map_err(|error| format!("write refused: {error}"))?;
                    let result =
                        self.write_changes_with(changes, Some(&guard), NoopTransaction::new);
                    drop(guard);
                    result
                }
                None => self.write_changes_with(changes, None, NoopTransaction::new),
            },
            Some(tx) => {
                self.write_changes_validated(changes, tx)?;
                self.record_journal_commit(diwe::fs::journal_effects_for(changes), None);
                Ok(())
            }
        }
    }

    /// Generic core of [`Self::write_changes`], parameterized over the
    /// transaction backend the same way [`Self::write_file_with`] is —
    /// `pub` for the same reason; see that method's doc comment.
    ///
    /// `lock_guard` is threaded straight through to
    /// `diwe::fs::apply_changes_with` (whose per-on-disk-step fencing
    /// checks it re-validates) and to the journal record + `[commit]`
    /// trigger at the end — `Some(&guard)` from `write_changes`'s
    /// `NoopTransaction` branch, `None` for every other caller.
    pub fn write_changes_with<TX: Transaction>(
        &self,
        changes: &Changes,
        lock_guard: Option<&CommitLockGuard>,
        new_tx: impl FnMut() -> TX,
    ) -> Result<(), String> {
        let Some(base_path) = &self.base_path else {
            return Ok(());
        };
        {
            let result = diwe::fs::apply_changes_with(
                changes,
                base_path,
                self.config.format,
                |key, content, prior_content, operation| {
                    // Same `project_path`-rooted resolution as
                    // `enforce_write_permission` above. `prior_content` is
                    // supplied by `apply_changes_with` itself, read from
                    // disk immediately before this document's write.
                    // `operation` is `apply_changes_with`'s own explicit
                    // M4/R1 signal (`WriteOperation::Delete` for
                    // `changes.removes`, `::Write` otherwise) — forwarded
                    // as-is, never re-inferred here.
                    match &self.project_path {
                        Some(root) => diwe::permissions::check_write_permission_for_content_in(
                            &self.config,
                            &schemas_dir_in(root),
                            key,
                            content,
                            prior_content,
                            operation,
                        ),
                        None => diwe::permissions::check_write_permission_for_content(
                            &self.config,
                            key,
                            content,
                            prior_content,
                            operation,
                        ),
                    }
                },
                // The commit-lock guard `write_changes`'s `NoopTransaction`
                // branch holds (if any): passed through so
                // `apply_changes_with`'s per-write fencing checks and this
                // call site's journal record + `[commit]` trigger all land
                // inside the same held commit-lock window.
                lock_guard,
                new_tx,
            );
            // One journal record for this whole batch (every key
            // `changes` names), not one per document — mirrors
            // `diwe::fs::apply_changes`'s own journal composition on the
            // CLI side, reimplemented here rather than routed through
            // that no-op-`Transaction`-default wrapper because this call
            // site is already generic over `TX`. Nothing is recorded if
            // `apply_changes_with` returned an error partway through.
            match result {
                Ok(()) => {
                    self.record_journal_commit(
                        diwe::fs::journal_effects_for(changes),
                        lock_guard,
                    );
                    Ok(())
                }
                Err(error) => Err(error.to_string()),
            }
        }
    }

    pub fn start_watching(&self) {
        if let Some(base_path) = &self.base_path {
            watcher::start(self.graph.clone(), base_path.clone(), self.config.format);
        }
    }

    fn render_key_template(&self, template: &str) -> Result<String, String> {
        let now = Local::now();
        let date_format = self
            .config
            .library
            .date_format
            .as_deref()
            .unwrap_or(DEFAULT_KEY_DATE_FORMAT);
        let formatted = now.format(date_format).to_string();
        Environment::new()
            .template_from_str(template)
            .map_err(|e| format!("invalid key template: {}", e))?
            .render(context! {
                today => formatted,
                now => formatted,
            })
            .map_err(|e| format!("key template rendering failed: {}", e))
    }

    fn render_document_template(&self, template: &str, content: &str) -> Result<String, String> {
        let now = Local::now();
        let date_format = self
            .config
            .markdown
            .date_format
            .as_deref()
            .unwrap_or("%b %d, %Y");
        let formatted = now.format(date_format).to_string();
        Environment::new()
            .template_from_str(template)
            .map_err(|e| format!("invalid document template: {}", e))?
            .render(context! {
                today => formatted,
                now => formatted,
                content => content,
            })
            .map_err(|e| format!("document template rendering failed: {}", e))
    }
}

// T6: `write_file_with` and `write_changes_with` are the generic cores
// behind every MCP write tool — WP-12 (iwe_create/iwe_update/iwe_delete/
// iwe_query/iwe_rename/iwe_extract/iwe_inline/iwe_attach) via `write_file`
// and WP-13 (iwe_normalize) also via `write_file`, plus WP-12's
// delete/rename/extract/inline surface via `write_changes` (which
// delegates directly to `diwe::fs::apply_changes_with`, already covered
// from the CLI side in `diwe::fs`'s own tests — see its doc comment).
// These tests construct a real `IweServer` over a temp directory and
// drive both generic cores directly with a
// `liwe::transaction::RecordingTransaction` in place of `NoopTransaction`
// to prove the wiring at these MCP call sites is real.
#[cfg(test)]
mod transaction_tests {
    use super::*;
    use liwe::transaction::{RecordingTransaction, TransactionLog};

    fn server_over(dir: &std::path::Path) -> IweServer {
        IweServer::new(dir.to_str().unwrap(), &Configuration::default())
    }

    /// WP-12/WP-13 (MCP create/update/attach/normalize, all funneled
    /// through `write_file`): an ordinary write drives exactly one
    /// `begin` and one `commit`, and the content lands on disk.
    #[test]
    fn write_file_drives_begin_and_commit() {
        let dir = tempfile::tempdir().unwrap();
        let server = server_over(dir.path());
        let log = TransactionLog::new();

        let result = server.write_file_with(&Key::name("note"), "# Note\n", None, {
            let log = log.clone();
            move || RecordingTransaction::new(log.clone())
        });

        assert!(result.is_ok(), "{result:?}");
        assert_eq!(log.begin_count(), 1);
        assert_eq!(log.commit_count(), 1);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("note.md")).unwrap(),
            "# Note\n"
        );
    }

    /// WP-12 (MCP delete/rename/extract/inline, via `write_changes` ->
    /// `apply_changes_with`): an ordinary write drives exactly one
    /// `begin` and one `commit`.
    #[test]
    fn write_changes_drives_begin_and_commit() {
        let dir = tempfile::tempdir().unwrap();
        let server = server_over(dir.path());
        let changes = Changes::new().create(Key::name("note"), "# Note\n".to_string());
        let log = TransactionLog::new();

        server
            .write_changes_with(&changes, None, {
                let log = log.clone();
                move || RecordingTransaction::new(log.clone())
            })
            .unwrap();

        assert_eq!(log.begin_count(), 1);
        assert_eq!(log.commit_count(), 1);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("note.md")).unwrap(),
            "# Note\n"
        );
    }
}
