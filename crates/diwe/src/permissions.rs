//! The single shared site for write-permission evaluation (WP-02..WP-13).
//!
//! [`check_write_permission`] is the one function every write path — CLI and
//! MCP, ordinary and `--strict` — must call, with identically-derived
//! inputs, immediately before a write is allowed to reach durable storage.
//! It lives in `diwe` (not `liwe`) because it takes schema-static data
//! (`SchemaBinding`) as an input, and `diwe` is the first crate in the
//! dependency graph that both `liwe::query::document::PropertyRef` and
//! schema configuration are available in; it is already a shared dependency
//! of both the `iwe` (CLI) and `iwec` (MCP) binaries, the same way
//! `diwe::fs::apply_changes` and `diwe::schema::validate_pending_documents`
//! already are (see `pending_from_changes` / `gate_pending` /
//! `ensure_schema_clean` for the existing precedent of one shared function
//! called identically from both binaries).
//!
//! T10 implements the first rule: EXT-FREEZE, the `freeze` marker (see
//! [`FREEZE_FIELD`]). T11 (per-property mutability) and T12 (their
//! composition — freeze dominates mutability) build on top of this site,
//! not the other way around.
//!
//! Per `m2/design`, this check is pure and schema-static: it is given only
//! the target document's own state, the target property, and schema-static
//! configuration. It must never be given any other document's state.

use crate::config::{Configuration, Patterns, SchemaBinding};
use crate::schema::SchemaBindings;
use liwe::model::document::Document;
use liwe::model::{parse_leading_frontmatter, Key};
use liwe::query::PropertyRef;
use serde_yaml::Value;

/// The reserved frontmatter marker (EXT-FREEZE, T10) that makes a document
/// immutable: `freeze: true` on a document's own frontmatter rejects every
/// write to that document — body or any frontmatter field — regardless of
/// which schema the document is bound to. It is a document-level marker,
/// not a per-schema configuration option: any schema may declare `freeze`
/// as one of its properties (so `iwe schema validate` can type-check it),
/// but the write-permission check below reads it directly off the target
/// document's own frontmatter, the same way it is the only state
/// [`check_write_permission`] is allowed to read. This is what keeps
/// enforcement "schema/permission-layer" rather than hardcoded against any
/// one document's key: the same reserved field name is honored for every
/// document, uniformly.
pub const FREEZE_FIELD: &str = "freeze";

/// Whether `document`'s own frontmatter carries `freeze: true`. Reads only
/// `document`'s own state — no other document, no graph, no store.
fn is_frozen(document: &Document) -> bool {
    document
        .frontmatter
        .as_ref()
        .and_then(|frontmatter| frontmatter.get(Value::String(FREEZE_FIELD.to_string())))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Why a write was rejected by write-permission evaluation.
///
/// WP-02..WP-13 each add a variant (or a shared variant parameterized by
/// which WP rule fired) as they are implemented. [`check_write_permission`]
/// itself is never given the target document's key (per its signature, it
/// ranges only over the document's own state, the property, and
/// schema-static data), so a `WritePermissionError` does not carry a key
/// either — every caller that surfaces one already has the key in scope
/// separately (it is what they passed to
/// [`check_write_permission_for_content`]), so [`WritePermissionError::message`]
/// takes it as a parameter to produce the final, document-naming rejection
/// text.
#[derive(Debug, Clone, PartialEq)]
pub enum WritePermissionError {
    /// The target document carries the `freeze` marker (see [`FREEZE_FIELD`]):
    /// every write to it — body or any frontmatter field — is rejected,
    /// unconditionally, regardless of which property is being written.
    Frozen,
}

impl WritePermissionError {
    /// The full rejection message for a write to `key`: names the document
    /// and the rule that rejected it, plus what would lift the rejection —
    /// matching the "document + rule at named-property granularity" shape
    /// M1 found violation reports must have.
    pub fn message(&self, key: &Key) -> String {
        match self {
            WritePermissionError::Frozen => format!(
                "write to '{key}' rejected: document is frozen (unset '{FREEZE_FIELD}' to allow writes)"
            ),
        }
    }
}

/// Evaluates write permission for one property write on one document.
///
/// This is the single site every ordinary and every strict invocation of a
/// write must reach, with the same inputs, before the write proceeds:
///
/// - `document` — the *target* document's own current state only (its
///   frontmatter and body as they exist right now, before this write is
///   applied). Never another document's state.
/// - `property` — which property of `document` this call is evaluating
///   ([`PropertyRef::Frontmatter`] at a field path, or [`PropertyRef::Body`]
///   for the document body).
/// - `schema` — the schema-static configuration bound to `document` (e.g.
///   its matched [`SchemaBinding`]). No other runtime state.
///
/// Strict invocation (`--strict` on the CLI, or MCP's unconditional
/// `ensure_schema_clean`) must call this exact function with these exact
/// inputs — it must not vary or re-implement this check. Strict invocation
/// additionally runs schema validation (`validate_pending_documents` /
/// `ensure_schema_clean`) as a separate, prior check; that check is
/// unrelated to this one and must not be conflated with it. This function
/// itself takes no "strict" parameter, by design: whether the *caller* is
/// running in strict mode changes nothing about how this evaluates, which
/// is exactly the "strict is a superset, never a variant" property `m2/
/// design-enforcement-modes` requires.
pub fn check_write_permission(
    document: &Document,
    property: &PropertyRef,
    schema: &SchemaBinding,
) -> Result<(), WritePermissionError> {
    // T3 verification marker: proves every insertion site reaches this
    // exact function at runtime, in both the CLI and MCP binaries. Emitted
    // via `log::debug!` (silent by default, visible with RUST_LOG=debug)
    // and mirrored to stderr with a distinctive prefix so it can be
    // grepped even where no `log` backend is wired up (as in the `iwe`
    // CLI today, which uses `tracing_subscriber` without a `log` bridge).
    // T10/T11/T12 may remove this once WP-02..WP-13 add real logging.
    log::debug!(
        "write-permission check reached (T3 site): property={:?}",
        property
    );
    if std::env::var_os("IWE_T3_WRITE_PERMISSION_TRACE").is_some() {
        eprintln!(
            "[T3-write-permission] check reached: property={:?}",
            property
        );
    }
    // T10 (EXT-FREEZE): freeze overrides regardless of `property` — a
    // frozen document rejects a body write exactly as it rejects a
    // frontmatter-field write, so this check runs before any per-property
    // branching (T11's mutability marking, when it lands, only narrows what
    // an *unfrozen* document allows; it never overrides freeze — that
    // composition is T12's job, out of scope here).
    //
    // T11/T12: implement the remaining WP-02..WP-13 rules here, after this
    // check.
    let _ = schema;
    if is_frozen(document) {
        return Err(WritePermissionError::Frozen);
    }
    Ok(())
}

/// Resolves the [`SchemaBinding`] bound to `key` under `config`'s `schemas`
/// table, for callers that need to pass a real (not placeholder) `schema`
/// argument into [`check_write_permission`]. Uses the same schema-matching
/// precedent already established by `explain_documents` /
/// `pending_from_changes` / `ensure_schema_clean` in `diwe::schema`
/// (`SchemaBindings::compile` + `schemas_for`) rather than re-implementing
/// pattern matching here.
///
/// If `key` matches more than one schema, the first match (in
/// `SchemaBindings::compile`'s deterministic, alphabetically-sorted rule
/// order) is used; resolving multiple simultaneously-bound schemas is not
/// this function's job. If `key` matches no schema, or `config.schemas`
/// fails to compile, a schema-less binding (empty `match` list) is
/// returned — the same shape WP-02/WP-03/WP-12 used as a placeholder before
/// this function existed, now reached only when there truly is no bound
/// schema rather than unconditionally.
pub fn resolve_schema_binding(config: &Configuration, key: &Key) -> SchemaBinding {
    let schema_less = || SchemaBinding {
        r#match: Patterns::Many(Vec::new()),
    };
    let Ok(bindings) = SchemaBindings::compile(&config.schemas) else {
        return schema_less();
    };
    match bindings.schemas_for(key.as_str()).first() {
        Some(name) => config
            .schemas
            .get(*name)
            .cloned()
            .unwrap_or_else(schema_less),
        None => schema_less(),
    }
}

/// Parses `content`'s frontmatter into a [`Document`], resolves `key`'s
/// schema binding via [`resolve_schema_binding`], and calls
/// [`check_write_permission`] against [`PropertyRef::Body`] — the one
/// helper every whole-document-rewrite call site (WP-02..WP-13) uses so
/// document/schema/property resolution is implemented once, not
/// re-implemented per caller (the "one mechanism, not two" principle of
/// `m2/design-enforcement-modes`).
///
/// `content` is whatever markdown is about to reach durable storage for
/// `key` — the new content for a create/update, or (for a removal) the
/// document's content as it exists on disk immediately before the removal.
/// Finer per-property (`PropertyRef::Frontmatter`) enforcement for these
/// whole-document operations is a T10/T11/T12 scoping decision, not this
/// function's.
///
/// Frontmatter is parsed via [`liwe::model::parse_leading_frontmatter`]
/// rather than by hand-stripping `---` delimiters: the slice
/// `split_raw_frontmatter` returns includes the delimiter lines themselves,
/// which is not valid standalone YAML on its own (a naive
/// `serde_yaml::from_str` on it fails, silently on `.ok()`, on every
/// document with a real closing `---`/`...` line — which is every
/// document's frontmatter in practice), so a marker like `freeze: true`
/// would never actually be seen here without going through the same
/// metadata-block-aware parsing the rest of the codebase already uses.
pub fn check_write_permission_for_content(
    config: &Configuration,
    key: &Key,
    content: &str,
) -> Result<(), WritePermissionError> {
    let document = Document {
        blocks: Vec::new(),
        frontmatter: parse_leading_frontmatter(content),
    };
    let schema = resolve_schema_binding(config, key);
    check_write_permission(&document, &PropertyRef::Body, &schema)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Patterns;
    use liwe::query::FieldPath;

    fn empty_document() -> Document {
        Document {
            blocks: Vec::new(),
            frontmatter: None,
        }
    }

    fn document_with_frontmatter(yaml: &str) -> Document {
        Document {
            blocks: Vec::new(),
            frontmatter: serde_yaml::from_str(yaml).expect("valid frontmatter mapping"),
        }
    }

    fn schema_less() -> SchemaBinding {
        SchemaBinding {
            r#match: Patterns::Many(Vec::new()),
        }
    }

    #[test]
    fn unfrozen_document_allows_body_write() {
        let document = empty_document();
        let result = check_write_permission(&document, &PropertyRef::Body, &schema_less());
        assert!(result.is_ok());
    }

    #[test]
    fn unfrozen_document_allows_frontmatter_write() {
        let document = document_with_frontmatter("status: active\n");
        let property = PropertyRef::Frontmatter(FieldPath::from_dotted("status"));
        let result = check_write_permission(&document, &property, &schema_less());
        assert!(result.is_ok());
    }

    #[test]
    fn frozen_document_rejects_body_write() {
        let document = document_with_frontmatter("freeze: true\n");
        let result = check_write_permission(&document, &PropertyRef::Body, &schema_less());
        assert_eq!(result, Err(WritePermissionError::Frozen));
    }

    #[test]
    fn frozen_document_rejects_frontmatter_write_too() {
        // Freeze dominates regardless of which property is nominally being
        // written — this is what makes freeze a whole-document rule rather
        // than a per-property one, even though T11's actual per-property
        // mutability mechanism does not exist yet.
        let document = document_with_frontmatter("freeze: true\nstatus: active\n");
        let property = PropertyRef::Frontmatter(FieldPath::from_dotted("status"));
        let result = check_write_permission(&document, &property, &schema_less());
        assert_eq!(result, Err(WritePermissionError::Frozen));
    }

    #[test]
    fn freeze_false_does_not_reject() {
        let document = document_with_frontmatter("freeze: false\n");
        let result = check_write_permission(&document, &PropertyRef::Body, &schema_less());
        assert!(result.is_ok());
    }

    #[test]
    fn non_boolean_freeze_value_does_not_reject() {
        // Only a literal `true` freezes; a malformed marker (e.g. a string)
        // is not silently treated as frozen.
        let document = document_with_frontmatter("freeze: \"yes\"\n");
        let result = check_write_permission(&document, &PropertyRef::Body, &schema_less());
        assert!(result.is_ok());
    }

    #[test]
    fn rejection_message_names_the_document_and_the_rule() {
        let key = Key::name("some/document");
        let message = WritePermissionError::Frozen.message(&key);
        assert!(message.contains("some/document"));
        assert!(message.contains("frozen"));
        assert!(message.contains(FREEZE_FIELD));
    }

    #[test]
    fn check_write_permission_for_content_rejects_a_frozen_document() {
        let config = Configuration::default();
        let key = Key::name("frozen-doc");
        let content = "---\nfreeze: true\n---\n\n# Frozen\n\nbody\n";
        let result = check_write_permission_for_content(&config, &key, content);
        assert_eq!(result, Err(WritePermissionError::Frozen));
    }

    #[test]
    fn check_write_permission_for_content_allows_an_unfrozen_document() {
        let config = Configuration::default();
        let key = Key::name("plain-doc");
        let content = "# Plain\n\nbody\n";
        let result = check_write_permission_for_content(&config, &key, content);
        assert!(result.is_ok());
    }
}
