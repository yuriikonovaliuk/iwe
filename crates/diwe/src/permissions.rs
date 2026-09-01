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

use std::path::Path;

use crate::config::{schemas_dir, Configuration, Patterns, SchemaBinding};
use crate::schema::{schema_mutability_rules, MutabilityRule, SchemaBindings};
use liwe::model::document::Document;
use liwe::model::{split_raw_frontmatter, Key};
use liwe::query::PropertyRef;

/// Why a write was rejected by write-permission evaluation.
///
/// T11 adds [`WritePermissionError::PropertyImmutable`]
/// (EXT-PER-PROPERTY-MUTABILITY, LAW-09). T10/T12 add or compose further
/// variants (e.g. freeze) alongside it.
#[derive(Debug, Clone, PartialEq)]
pub enum WritePermissionError {
    /// Placeholder used only so the type is inhabited before WP-02..WP-13
    /// land. No caller should construct or match on this today.
    Placeholder,
    /// A write to one property of `key` was rejected because the schema
    /// bound to it declares that property immutable via a `mutable: false`
    /// entry (EXT-PER-PROPERTY-MUTABILITY, LAW-09's construct). Unlike
    /// freeze's whole-document rejection (`m2/design-extensions`: "naming
    /// document and rule"), this names the specific property that was
    /// rejected, not just the document — the sufficiency point of this
    /// construct existing as distinct from freeze.
    PropertyImmutable {
        /// The document the rejected write targeted.
        key: Key,
        /// Which property was rejected.
        property: PropertyRef,
        /// The property selector exactly as written in the schema's
        /// `mutable:` mapping (`$content`, or a dotted frontmatter path).
        selector: String,
    },
}

impl std::fmt::Display for WritePermissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WritePermissionError::Placeholder => {
                write!(f, "write rejected by write-permission check")
            }
            WritePermissionError::PropertyImmutable {
                key,
                selector,
                property,
            } => write!(
                f,
                "write rejected: document '{key}', rule 'mutable: false', property '{selector}'{}",
                if property.is_body() {
                    " (the document body)"
                } else {
                    ""
                }
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
/// - `key` — which document `document` is, so a rejection can name it.
/// - `property` — which property of `document` this call is evaluating
///   ([`PropertyRef::Frontmatter`] at a field path, or [`PropertyRef::Body`]
///   for the document body).
/// - `schema` — the schema-static configuration bound to `document` (e.g.
///   its matched [`SchemaBinding`]). No other runtime state.
/// - `mutability` — the `mutable:` rules `key`'s schema declares (T11,
///   EXT-PER-PROPERTY-MUTABILITY), resolved by [`resolve_mutability_rules`].
///   A property with no rule here is mutable — this predicate only ever
///   *restricts*, never grants (AB9's default-mutable guarantee).
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
/// design-enforcement-modes` requires — and, per `m2/design-enforcement-
/// modes`'s "mode one" requirement, every caller (ordinary or strict, CLI
/// or MCP) reaches this same evaluation unconditionally, never gated behind
/// a flag.
///
/// T11 implements only the per-property mutability branch. T10's freeze
/// check is a clearly-separated branch alongside it (not implemented here —
/// see this module's top-level doc comment) so a later composition step
/// (T12) can make freeze short-circuit over mutability without entangling
/// the two.
pub fn check_write_permission(
    document: &Document,
    key: &Key,
    property: &PropertyRef,
    schema: &SchemaBinding,
    mutability: &[MutabilityRule],
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
    let _ = (document, schema);

    // T11 (EXT-PER-PROPERTY-MUTABILITY, LAW-09): reject a write to
    // `property` iff the schema explicitly marks it `mutable: false`. A
    // property with no rule at all — the entire vector empty, as for any
    // document whose schema (or lack of one) never mentions `mutable` — is
    // mutable, so this branch is a strict no-op for every document that
    // predates or never opts into this construct (AB9).
    if let Some(rule) = mutability.iter().find(|rule| rule.property == *property) {
        if !rule.mutable {
            return Err(WritePermissionError::PropertyImmutable {
                key: key.clone(),
                property: property.clone(),
                selector: rule.selector.clone(),
            });
        }
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

/// Resolves the `mutable:` rules `key`'s bound schema declares (T11,
/// EXT-PER-PROPERTY-MUTABILITY), for callers that need to pass a real (not
/// empty) `mutability` argument into [`check_write_permission`]. Resolves
/// the schemas directory itself via `diwe::config::schemas_dir` (the
/// process's current directory) — the convention every other cwd-rooted
/// caller in this crate already uses (`validate_documents`,
/// `explain_documents`). Callers whose root is not necessarily the process
/// cwd (e.g. `iwec`, which keeps its own `project_path` distinct from cwd
/// precisely so tests can run in-process against a temp directory — see
/// `IweServer::ensure_schema_clean`'s `schemas_dir_in(root)` precedent)
/// should call [`resolve_mutability_rules_in`] instead.
///
/// If `key` matches no schema, `config.schemas` fails to compile, or the
/// schemas directory can't be resolved, this returns no rules — mutable by
/// default (AB9) — rather than blocking writes on a problem unrelated to
/// mutability.
pub fn resolve_mutability_rules(config: &Configuration, key: &Key) -> Vec<MutabilityRule> {
    match schemas_dir() {
        Ok(dir) => resolve_mutability_rules_in(&dir, config, key),
        Err(_) => Vec::new(),
    }
}

/// Same as [`resolve_mutability_rules`], but reading schema files from the
/// caller-supplied `schemas_dir` rather than one derived from the process's
/// current directory — see that function's doc comment for when to prefer
/// this.
///
/// Mirrors [`resolve_schema_binding`]'s schema-matching precedent
/// (`SchemaBindings::compile` + `schemas_for`, first match wins), then reads
/// `key`'s matched schema's *file* (`<schemas_dir>/<name>.yaml`) — where
/// `mutable`, like `links`/`requires`/`asserts`, is actually declared,
/// rather than in the `config.toml`-derived [`SchemaBinding`], which only
/// ever carries `match` patterns (`diwe::schema::schema_mutability_rules`).
pub fn resolve_mutability_rules_in(
    schemas_dir: &Path,
    config: &Configuration,
    key: &Key,
) -> Vec<MutabilityRule> {
    let Ok(bindings) = SchemaBindings::compile(&config.schemas) else {
        return Vec::new();
    };
    let Some(name) = bindings.schemas_for(key.as_str()).first().copied() else {
        return Vec::new();
    };
    schema_mutability_rules(schemas_dir, name)
}

/// Parses `content`'s frontmatter into a [`Document`], resolves `key`'s
/// schema binding via [`resolve_schema_binding`] and its `mutable:` rules
/// via [`resolve_mutability_rules`] (cwd-rooted schemas directory — see
/// that function's doc comment; [`check_write_permission_for_content_in`]
/// is the explicit-root equivalent), and calls [`check_write_permission`]
/// against [`PropertyRef::Body`] — the one helper every whole-document-
/// rewrite call site (WP-02..WP-13) uses so document/schema/property
/// resolution is implemented once, not re-implemented per caller (the "one
/// mechanism, not two" principle of `m2/design-enforcement-modes`).
///
/// `content` is whatever markdown is about to reach durable storage for
/// `key` — the new content for a create/update, or (for a removal) the
/// document's content as it exists on disk immediately before the removal.
/// Finer per-property (`PropertyRef::Frontmatter`) enforcement for these
/// whole-document operations is a T10/T11/T12 scoping decision, not this
/// function's; T11 leaves it unaddressed here and instead exercises
/// per-property discrimination through direct [`check_write_permission`]
/// calls (its own construct-level tests) — `m2/design-extensions` requires
/// only that "the construct exists and the construct itself is tested" at
/// M2, not end-to-end law enforcement through every call path.
pub fn check_write_permission_for_content(
    config: &Configuration,
    key: &Key,
    content: &str,
) -> Result<(), WritePermissionError> {
    let mutability = resolve_mutability_rules(config, key);
    check_write_permission_with_mutability(config, key, content, mutability)
}

/// Same as [`check_write_permission_for_content`], but resolving `mutable:`
/// rules from the caller-supplied `schemas_dir` rather than one derived
/// from the process's current directory (see [`resolve_mutability_rules_in`]).
pub fn check_write_permission_for_content_in(
    config: &Configuration,
    schemas_dir: &Path,
    key: &Key,
    content: &str,
) -> Result<(), WritePermissionError> {
    let mutability = resolve_mutability_rules_in(schemas_dir, config, key);
    check_write_permission_with_mutability(config, key, content, mutability)
}

fn check_write_permission_with_mutability(
    config: &Configuration,
    key: &Key,
    content: &str,
    mutability: Vec<MutabilityRule>,
) -> Result<(), WritePermissionError> {
    let (front, _body) = split_raw_frontmatter(content);
    let document = Document {
        blocks: Vec::new(),
        frontmatter: front.and_then(|front| serde_yaml::from_str(front).ok()),
    };
    let schema = resolve_schema_binding(config, key);
    check_write_permission(&document, key, &PropertyRef::Body, &schema, &mutability)
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

    fn schema_less() -> SchemaBinding {
        SchemaBinding {
            r#match: Patterns::Many(Vec::new()),
        }
    }

    fn key(name: &str) -> Key {
        Key::name(name)
    }

    #[test]
    fn placeholder_check_always_allows_with_no_mutability_rules() {
        let document = empty_document();
        let schema = schema_less();
        let result = check_write_permission(
            &document,
            &key("docs/one"),
            &PropertyRef::Body,
            &schema,
            &[],
        );
        assert!(result.is_ok());
    }

    /// AB9 / default-mutable: a corpus of documents and properties with no
    /// `mutable:` rule declared anywhere — the shape every document had
    /// before T11, and the shape any document without the construct still
    /// has today — sees zero rejections from this construct.
    #[test]
    fn default_mutable_corpus_sees_zero_rejections() {
        let document = empty_document();
        let schema = schema_less();
        let properties = [
            PropertyRef::Body,
            PropertyRef::from_selector("status"),
            PropertyRef::from_selector("owner.name"),
            PropertyRef::from_selector("tags"),
        ];
        for doc_key in ["docs/one", "docs/two", "notes/three"] {
            for property in &properties {
                let result =
                    check_write_permission(&document, &key(doc_key), property, &schema, &[]);
                assert!(
                    result.is_ok(),
                    "unmarked property {property:?} on '{doc_key}' was rejected: {result:?}"
                );
            }
        }
    }

    /// LAW-09's structural shape, in layer-free vocabulary: a schema marks
    /// the document body immutable and one ordinary property mutable. The
    /// body write is rejected; the mutable property's write succeeds. This
    /// is the construct's sufficiency point over unqualified freeze: freeze
    /// rejects every write to the document, including to `archived`, which
    /// is exactly why this per-property mechanism exists as distinct from
    /// it (`m2/design-extensions`).
    #[test]
    fn body_immutable_other_property_mutable_matches_law_09_shape() {
        let document = empty_document();
        let schema = schema_less();
        let mutability = vec![
            MutabilityRule {
                selector: "$content".to_string(),
                property: PropertyRef::Body,
                mutable: false,
            },
            MutabilityRule {
                selector: "archived".to_string(),
                property: PropertyRef::from_selector("archived"),
                mutable: true,
            },
        ];
        let doc_key = key("notes/reference");

        let body_result = check_write_permission(
            &document,
            &doc_key,
            &PropertyRef::Body,
            &schema,
            &mutability,
        );
        assert_eq!(
            body_result,
            Err(WritePermissionError::PropertyImmutable {
                key: doc_key.clone(),
                property: PropertyRef::Body,
                selector: "$content".to_string(),
            })
        );

        let archived_result = check_write_permission(
            &document,
            &doc_key,
            &PropertyRef::from_selector("archived"),
            &schema,
            &mutability,
        );
        assert!(archived_result.is_ok());
    }

    /// A property absent from the `mutable:` mapping entirely — distinct
    /// from an explicit `mutable: true` — is still mutable, even when the
    /// same mapping declares other properties. Only an explicit `false`
    /// rejects.
    #[test]
    fn property_absent_from_mutable_mapping_is_mutable() {
        let document = empty_document();
        let schema = schema_less();
        let mutability = vec![MutabilityRule {
            selector: "$content".to_string(),
            property: PropertyRef::Body,
            mutable: false,
        }];
        let result = check_write_permission(
            &document,
            &key("docs/one"),
            &PropertyRef::from_selector("status"),
            &schema,
            &mutability,
        );
        assert!(result.is_ok());
    }

    /// The rejection names the document, the rule, and the specific
    /// property — more granular than freeze's whole-document rejection
    /// (`m2/design-extensions`: freeze "reject[s] every write ... naming
    /// document and rule").
    #[test]
    fn rejection_names_document_rule_and_property() {
        let error = WritePermissionError::PropertyImmutable {
            key: key("notes/reference"),
            property: PropertyRef::Body,
            selector: "$content".to_string(),
        };
        let message = error.to_string();
        assert!(message.contains("notes/reference"), "{message}");
        assert!(message.contains("mutable: false"), "{message}");
        assert!(message.contains("$content"), "{message}");
    }

    /// Rejection for a frontmatter property names that property's selector,
    /// not `$content` — proving the message is genuinely per-property, not
    /// a copy-pasted body message.
    #[test]
    fn rejection_names_the_frontmatter_property_that_was_rejected() {
        let document = empty_document();
        let schema = schema_less();
        let mutability = vec![MutabilityRule {
            selector: "status".to_string(),
            property: PropertyRef::from_selector("status"),
            mutable: false,
        }];
        let result = check_write_permission(
            &document,
            &key("docs/one"),
            &PropertyRef::from_selector("status"),
            &schema,
            &mutability,
        );
        let message = result.unwrap_err().to_string();
        assert!(message.contains("docs/one"), "{message}");
        assert!(message.contains("mutable: false"), "{message}");
        assert!(message.contains("'status'"), "{message}");
        assert!(!message.contains("$content"), "{message}");
    }
}
