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
//! This module intentionally does not yet implement WP-02..WP-13 — that is
//! T10/T11/T12's work. It fixes the *site* and the *signature* so those
//! tasks do not each re-decide where the check goes.
//!
//! Per `m2/design`, this check is pure and schema-static: it is given only
//! the target document's own state, the target property, and schema-static
//! configuration. It must never be given any other document's state.

use crate::config::{Configuration, Patterns, SchemaBinding};
use crate::schema::SchemaBindings;
use liwe::model::document::Document;
use liwe::model::{split_raw_frontmatter, Key};
use liwe::query::PropertyRef;

/// Why a write was rejected by write-permission evaluation.
///
/// A placeholder today — WP-02..WP-13 each add a variant (or a shared
/// variant parameterized by which WP rule fired) when they are implemented.
#[derive(Debug, Clone, PartialEq)]
pub enum WritePermissionError {
    /// Placeholder used only so the type is inhabited before WP-02..WP-13
    /// land. No caller should construct or match on this today.
    Placeholder,
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
    // T10/T11/T12: implement WP-02..WP-13 here. Until then this is a
    // deliberate no-op (always allow) so the site can be wired into every
    // call path without changing behavior yet.
    let _ = (document, property, schema);
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
pub fn check_write_permission_for_content(
    config: &Configuration,
    key: &Key,
    content: &str,
) -> Result<(), WritePermissionError> {
    let (front, _body) = split_raw_frontmatter(content);
    let document = Document {
        blocks: Vec::new(),
        frontmatter: front.and_then(|front| serde_yaml::from_str(front).ok()),
    };
    let schema = resolve_schema_binding(config, key);
    check_write_permission(&document, &PropertyRef::Body, &schema)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Patterns;

    fn empty_document() -> Document {
        Document {
            blocks: Vec::new(),
            frontmatter: None,
        }
    }

    #[test]
    fn placeholder_check_always_allows() {
        let document = empty_document();
        let schema = SchemaBinding {
            r#match: Patterns::Many(Vec::new()),
        };
        let result = check_write_permission(&document, &PropertyRef::Body, &schema);
        assert!(result.is_ok());
    }
}
