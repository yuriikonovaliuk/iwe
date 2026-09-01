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
//! [`FREEZE_FIELD`]). T11 implements the second: EXT-PER-PROPERTY-MUTABILITY,
//! the schema-declared `mutable:` construct. Their composition (this M2
//! reconciliation, folding in what would otherwise have been T12's work) is
//! implemented directly in [`check_write_permission`]: freeze is checked
//! first, and dominates — see that function's doc comment for the R15/
//! LAW-13 citation and a note on a discrepancy this composition
//! deliberately preserves rather than silently resolves.
//!
//! Per `m2/design`, this check is pure and schema-static: it is given only
//! the target document's own state, the target property, and schema-static
//! configuration. It must never be given any other document's state.

use std::path::Path;

use crate::config::{schemas_dir, Configuration, Patterns, SchemaBinding};
use crate::schema::{schema_mutability_rules, MutabilityRule, SchemaBindings};
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
/// T10 adds [`WritePermissionError::Frozen`] (EXT-FREEZE). T11 adds
/// [`WritePermissionError::PropertyImmutable`]
/// (EXT-PER-PROPERTY-MUTABILITY, LAW-09). Both variants carry their own
/// `key` (rather than requiring a caller to separately supply one, as an
/// earlier T10 draft of this type did) so [`std::fmt::Display`] alone is
/// enough to produce the final, document-naming rejection text — no
/// separate `message(&self, key: &Key)` method is needed.
#[derive(Debug, Clone, PartialEq)]
pub enum WritePermissionError {
    /// The target document carries the `freeze` marker (see [`FREEZE_FIELD`]):
    /// every write to it — body or any frontmatter field — is rejected,
    /// unconditionally, regardless of which property is being written.
    Frozen {
        /// The document the rejected write targeted.
        key: Key,
    },
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
            WritePermissionError::Frozen { key } => write!(
                f,
                "write to '{key}' rejected: document is frozen (unset '{FREEZE_FIELD}' to allow writes)"
            ),
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
/// # Composition: freeze dominates mutability (R15 / LAW-13)
///
/// Freeze (T10) is checked first, unconditionally, and short-circuits: a
/// frozen document rejects every write, including to a property the
/// document's schema marks `mutable: true`. Only when the document is
/// *not* frozen does per-property mutability (T11) get a say. This is
/// R15's own text ("Freeze is document-level and dominates per-property
/// mutability: a frozen document rejects every write, including to
/// properties marked mutable") and LAW-13's normative requirement, and this
/// is the one site both rules are evaluated together, so it is the one
/// place this composition can actually be enforced.
///
/// This deliberately does **not** match M1's own T7 sketch for LAW-13,
/// which said the opposite — "the flag's finer-grained permission takes
/// precedence over the document-level freeze". That sketch is wrong per
/// R15's normative text and per this milestone's own scope, which is why
/// this function does not follow it; it is flagged here, not silently
/// corrected without a trace, per the M2 reconciliation brief that folded
/// this composition in.
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
    let _ = schema;

    // T10 (EXT-FREEZE), checked first: freeze overrides regardless of
    // `property` — a frozen document rejects a body write exactly as it
    // rejects a frontmatter-field write. See this function's "Composition"
    // doc section above: freeze dominates mutability (R15/LAW-13), so this
    // branch must run — and return — before the mutability branch below
    // ever gets a say.
    if is_frozen(document) {
        return Err(WritePermissionError::Frozen { key: key.clone() });
    }

    // T11 (EXT-PER-PROPERTY-MUTABILITY, LAW-09): reject a write to
    // `property` iff the schema explicitly marks it `mutable: false`. A
    // property with no rule at all — the entire vector empty, as for any
    // document whose schema (or lack of one) never mentions `mutable` — is
    // mutable, so this branch is a strict no-op for every document that
    // predates or never opts into this construct (AB9). Only reached when
    // the document is not frozen, per the composition above.
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
/// whole-document operations is out of scope for this helper; T11 exercises
/// per-property discrimination through direct [`check_write_permission`]
/// calls (its own construct-level tests) — `m2/design-extensions` requires
/// only that "the construct exists and the construct itself is tested" at
/// M2, not end-to-end law enforcement through every call path.
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
    let document = Document {
        blocks: Vec::new(),
        frontmatter: parse_leading_frontmatter(content),
    };
    let schema = resolve_schema_binding(config, key);
    check_write_permission(&document, key, &PropertyRef::Body, &schema, &mutability)
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

    fn key(name: &str) -> Key {
        Key::name(name)
    }

    #[test]
    fn unfrozen_document_with_no_mutability_rules_allows_body_write() {
        let document = empty_document();
        let result = check_write_permission(
            &document,
            &key("docs/one"),
            &PropertyRef::Body,
            &schema_less(),
            &[],
        );
        assert!(result.is_ok());
    }

    #[test]
    fn unfrozen_document_allows_frontmatter_write() {
        let document = document_with_frontmatter("status: active\n");
        let property = PropertyRef::Frontmatter(FieldPath::from_dotted("status"));
        let result = check_write_permission(
            &document,
            &key("docs/one"),
            &property,
            &schema_less(),
            &[],
        );
        assert!(result.is_ok());
    }

    #[test]
    fn frozen_document_rejects_body_write() {
        let document = document_with_frontmatter("freeze: true\n");
        let doc_key = key("docs/one");
        let result = check_write_permission(
            &document,
            &doc_key,
            &PropertyRef::Body,
            &schema_less(),
            &[],
        );
        assert_eq!(result, Err(WritePermissionError::Frozen { key: doc_key }));
    }

    #[test]
    fn frozen_document_rejects_frontmatter_write_too() {
        // Freeze dominates regardless of which property is nominally being
        // written — this is what makes freeze a whole-document rule rather
        // than a per-property one.
        let document = document_with_frontmatter("freeze: true\nstatus: active\n");
        let doc_key = key("docs/one");
        let property = PropertyRef::Frontmatter(FieldPath::from_dotted("status"));
        let result = check_write_permission(&document, &doc_key, &property, &schema_less(), &[]);
        assert_eq!(result, Err(WritePermissionError::Frozen { key: doc_key }));
    }

    #[test]
    fn freeze_false_does_not_reject() {
        let document = document_with_frontmatter("freeze: false\n");
        let result = check_write_permission(
            &document,
            &key("docs/one"),
            &PropertyRef::Body,
            &schema_less(),
            &[],
        );
        assert!(result.is_ok());
    }

    #[test]
    fn non_boolean_freeze_value_does_not_reject() {
        // Only a literal `true` freezes; a malformed marker (e.g. a string)
        // is not silently treated as frozen.
        let document = document_with_frontmatter("freeze: \"yes\"\n");
        let result = check_write_permission(
            &document,
            &key("docs/one"),
            &PropertyRef::Body,
            &schema_less(),
            &[],
        );
        assert!(result.is_ok());
    }

    #[test]
    fn freeze_rejection_message_names_the_document_and_the_rule() {
        let message = WritePermissionError::Frozen {
            key: key("some/document"),
        }
        .to_string();
        assert!(message.contains("some/document"));
        assert!(message.contains("frozen"));
        assert!(message.contains(FREEZE_FIELD));
    }

    #[test]
    fn check_write_permission_for_content_rejects_a_frozen_document() {
        let config = Configuration::default();
        let doc_key = key("frozen-doc");
        let content = "---\nfreeze: true\n---\n\n# Frozen\n\nbody\n";
        let result = check_write_permission_for_content(&config, &doc_key, content);
        assert_eq!(result, Err(WritePermissionError::Frozen { key: doc_key }));
    }

    #[test]
    fn check_write_permission_for_content_allows_an_unfrozen_document() {
        let config = Configuration::default();
        let key = key("plain-doc");
        let content = "# Plain\n\nbody\n";
        let result = check_write_permission_for_content(&config, &key, content);
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

    /// T12-shaped composition test: freeze dominates mutability. A document
    /// carries both `freeze: true` and a `mutable: true` rule for one of
    /// its properties — the write to that "mutable" property is still
    /// rejected, and rejected *as a freeze*, not as a (non-existent, since
    /// the property is marked mutable) property-immutability violation.
    /// This is R15's/LAW-13's composition, proven at the unit level; see
    /// `crates/iwe/tests/freeze_dominates_mutability_test.rs` for the
    /// integration-level proof through a real schema + document on disk.
    #[test]
    fn freeze_dominates_a_property_explicitly_marked_mutable() {
        let document = document_with_frontmatter("freeze: true\narchived: false\n");
        let doc_key = key("notes/reference");
        let schema = schema_less();
        let mutability = vec![MutabilityRule {
            selector: "archived".to_string(),
            property: PropertyRef::from_selector("archived"),
            mutable: true,
        }];

        let result = check_write_permission(
            &document,
            &doc_key,
            &PropertyRef::from_selector("archived"),
            &schema,
            &mutability,
        );

        assert_eq!(result, Err(WritePermissionError::Frozen { key: doc_key }));
    }
}
