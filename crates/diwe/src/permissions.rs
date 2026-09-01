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

use crate::config::{schemas_dir, Configuration};
use crate::schema::{schema_deletable_rule, schema_mutability_rules, MutabilityRule, SchemaBindings};
use liwe::model::document::Document;
use liwe::model::{parse_leading_frontmatter, split_raw_frontmatter, Frontmatter, Key};
use liwe::query::filter::{resolve_path, Resolution};
use liwe::query::PropertyRef;
use serde_yaml::Value;

/// Which kind of write [`check_write_permission_for_content`] (and its
/// `_in`/`_with_mutability` siblings) is evaluating — explicit, never
/// inferred from `content`'s shape (e.g. "`content` happens to be `\"\"`").
///
/// M4/R1 (`m2/design-deletion-carrier`) needs the write-permission predicate
/// to "distinguish which operation it is judging" so a delete-specific
/// prohibition (LAW-16, [`WritePermissionError::DeleteProhibited`]) can be
/// evaluated only for an actual removal, and can never be triggered — or
/// silently skipped — by an ordinary create/update whose outgoing content
/// happens to be empty. Every call site that can tell whether it is
/// removing a document (`diwe::fs::apply_changes[_with]`'s `changes.removes`
/// loop) passes [`WriteOperation::Delete`] explicitly; every call site that
/// can only ever create or update (whole-document rewrites, `iwe normalize`,
/// every MCP tool other than `iwe_delete`) passes [`WriteOperation::Write`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOperation {
    /// A create or update: the document continues to exist afterward.
    Write,
    /// A removal: the document ceases to exist afterward.
    Delete,
}

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
        .map(frontmatter_is_frozen)
        .unwrap_or(false)
}

/// Same as [`is_frozen`], but reading a bare [`Frontmatter`] mapping rather
/// than a whole [`Document`] — the shape [`is_solitary_unfreeze`] needs when
/// comparing a prior and a next frontmatter mapping directly.
fn frontmatter_is_frozen(frontmatter: &Frontmatter) -> bool {
    frontmatter
        .get(Value::String(FREEZE_FIELD.to_string()))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// `frontmatter` with [`FREEZE_FIELD`] removed, for comparing two
/// frontmatter mappings "except for freeze" (`m2/design-freeze-semantics`'s
/// "identical ... except that it is no longer frozen").
fn without_freeze(frontmatter: &Frontmatter) -> Frontmatter {
    let mut copy = frontmatter.clone();
    copy.remove(Value::String(FREEZE_FIELD.to_string()));
    copy
}

/// The bypass-closing rule from `m2/design-freeze-semantics`: whether a
/// write from `prior_content` to `next_content` has, as its *sole* effect,
/// lifting freeze — the one exception to "a frozen document rejects every
/// write."
///
/// Per that design's own text: "the resulting document must be identical to
/// the prior one in every frontmatter property and in the body, except that
/// it is no longer frozen." This checks all three parts of that: the body is
/// byte-identical, every frontmatter property other than [`FREEZE_FIELD`] is
/// identical, and freeze's own *effective* state (not literal property —
/// `freeze: false` and an absent `freeze` key are both "lifted") transitions
/// from frozen to unfrozen. A write that leaves freeze untouched (still
/// frozen, or already unfrozen) never qualifies here — this function is only
/// ever consulted when the prior document is already known to be frozen.
fn is_solitary_unfreeze(prior_content: &str, next_content: &str) -> bool {
    let (_, prior_body) = split_raw_frontmatter(prior_content);
    let (_, next_body) = split_raw_frontmatter(next_content);
    if prior_body != next_body {
        return false;
    }

    let prior_frontmatter = parse_leading_frontmatter(prior_content).unwrap_or_default();
    let next_frontmatter = parse_leading_frontmatter(next_content).unwrap_or_default();

    if !frontmatter_is_frozen(&prior_frontmatter) || frontmatter_is_frozen(&next_frontmatter) {
        return false;
    }

    without_freeze(&prior_frontmatter) == without_freeze(&next_frontmatter)
}

/// D1 fix (M4-extension defect): whether `property` is actually one of the
/// properties this write changes, comparing `property`'s value immediately
/// before the write (`prior_frontmatter`/`prior_body`) against its value in
/// the write's outgoing content (`next_frontmatter`/`next_body`).
///
/// This is the mirror of M2's freeze-bypass insight
/// ([`check_write_permission_for_content`]'s own doc comment: "a predicate
/// that cannot see the prior state cannot enforce a rule about a
/// transition"): a predicate that cannot see *which* property a write
/// targets cannot enforce a rule that is supposed to be per-property.
/// [`check_write_permission_with_mutability`] used to check every write
/// against [`PropertyRef::Body`] unconditionally, regardless of whether the
/// write touched the body at all — so a `mutable: false` rule on `$content`
/// rejected every write to the document, including ones that only ever
/// touched a separately-mutable frontmatter field. This function is what
/// lets that caller ask, for each property a schema actually declares a
/// rule about, "did *this* write change it" instead of assuming the body
/// always did.
///
/// - [`PropertyRef::Body`]: touched iff the body text differs.
/// - [`PropertyRef::Frontmatter`]: touched iff the value at that (possibly
///   dotted) path differs — including "absent in one, present in the
///   other", which is a change too. Resolved via
///   [`liwe::query::filter::resolve_path`], the same path-resolution this
///   codebase already uses for filter/projection evaluation, so nested
///   selectors (e.g. `owner.name`) are handled identically here, not
///   reimplemented as a shallow top-level-key comparison.
fn property_touched(
    prior_frontmatter: &Frontmatter,
    next_frontmatter: &Frontmatter,
    prior_body: &str,
    next_body: &str,
    property: &PropertyRef,
) -> bool {
    match property {
        PropertyRef::Body => prior_body != next_body,
        PropertyRef::Frontmatter(path) => {
            match (
                resolve_path(prior_frontmatter, path),
                resolve_path(next_frontmatter, path),
            ) {
                (Resolution::Missing, Resolution::Missing) => false,
                (Resolution::Present(prior), Resolution::Present(next)) => prior != next,
                // Present on exactly one side: the property went from
                // absent to set, or from set to absent — a change either
                // way.
                _ => true,
            }
        }
    }
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
    /// M4/R1 (LAW-16, `m2/design-deletion-carrier`): a [`WriteOperation::
    /// Delete`] was rejected because the schema bound to `key` declares
    /// `deletable: false`. Independent of [`WritePermissionError::
    /// PropertyImmutable`]/[`WritePermissionError::Frozen`] — evaluated by
    /// its own construct (`crate::schema::schema_deletable_rule`), never as
    /// a side effect of a `mutable:`/`freeze` rule rejecting the same
    /// removal for an unrelated reason. See this module's "deletion
    /// prohibition" section (near `check_write_permission_with_mutability`)
    /// for why the two rejection paths must not be conflated.
    DeleteProhibited {
        /// The document whose deletion was rejected.
        key: Key,
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
            WritePermissionError::DeleteProhibited { key } => write!(
                f,
                "delete rejected: document '{key}', rule 'deletable: false' (this document cannot be deleted)"
            ),
        }
    }
}

/// Evaluates write permission for one property write on one document.
///
/// This is the single site every ordinary and every strict invocation of a
/// write must reach, with the same inputs, before the write proceeds:
///
/// - `document` — the *target* document's own current (prior-to-this-write)
///   state only (its frontmatter and body as they exist right now, before
///   this write is applied). Never another document's state, and never the
///   outgoing/resulting content — see [`check_write_permission_for_content`]'s
///   doc comment for why that distinction is exactly what M2's freeze-bypass
///   fix turns on.
/// - `key` — which document `document` is, so a rejection can name it.
/// - `property` — which property of `document` this call is evaluating
///   ([`PropertyRef::Frontmatter`] at a field path, or [`PropertyRef::Body`]
///   for the document body).
/// - `mutability` — the `mutable:` rules `key`'s schema declares (T11,
///   EXT-PER-PROPERTY-MUTABILITY), resolved by [`resolve_mutability_rules`]
///   from `key`'s schema binding — already schema-derived by the time it
///   reaches here, which is why this function takes no separate schema
///   parameter (see the M2 fix-wave note below).
///
/// This function used to also take a `schema: &SchemaBinding` parameter,
/// present in the signature but never read (`let _ = schema;`). M2's
/// fix-wave investigated whether that was a real gap — schema-static data
/// the predicate should consult but didn't — or genuine redundancy.
/// [`SchemaBinding`] carries only `match` patterns (used solely to *select*
/// which schema binds `key`); it carries no mutability or freeze data of its
/// own. `mutability` above is resolved via that exact same match-pattern
/// selection (`resolve_mutability_rules` / `resolve_mutability_rules_in`),
/// so every piece of schema-static data `schema` could have supplied was
/// already reaching this predicate through `mutability`. The parameter was
/// redundant, not a gap, and per `m2/design-extensions`'s "dead-but-present
/// must not stand either way" it has been removed rather than left unused.
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
/// Resolves `key`'s bound schema by the same schema-matching precedent used
/// throughout this crate (`SchemaBindings::compile` + `schemas_for`, first
/// match wins), then reads that schema's *file*
/// (`<schemas_dir>/<name>.yaml`) — where `mutable`, like
/// `links`/`requires`/`asserts`, is actually declared, rather than in the
/// `config.toml`-derived `SchemaBinding`, which only ever carries `match`
/// patterns (`diwe::schema::schema_mutability_rules`).
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

/// Resolves the `deletable:` rule `key`'s bound schema(s) declare (M4/R1,
/// LAW-16's carrier), for callers that need to pass a real `deletable`
/// argument into [`check_write_permission_with_mutability`]. Resolves the
/// schemas directory via `diwe::config::schemas_dir` (the process's current
/// directory) — see [`resolve_mutability_rules`]'s doc comment for the same
/// convention; [`resolve_deletable_rule_in`] is the explicit-root
/// equivalent.
///
/// If `key` matches no schema, `config.schemas` fails to compile, or the
/// schemas directory can't be resolved, this returns `None` — deletable by
/// default (mirrors AB9) — rather than blocking a removal on a problem
/// unrelated to deletion.
pub fn resolve_deletable_rule(config: &Configuration, key: &Key) -> Option<bool> {
    match schemas_dir() {
        Ok(dir) => resolve_deletable_rule_in(&dir, config, key),
        Err(_) => None,
    }
}

/// Same as [`resolve_deletable_rule`], but reading schema files from the
/// caller-supplied `schemas_dir` rather than one derived from the process's
/// current directory — see [`resolve_mutability_rules_in`]'s doc comment for
/// when to prefer this.
///
/// Deliberately **not** [`resolve_mutability_rules_in`]'s "first bound
/// schema wins" resolution: `mutable:` is safe to resolve that way only
/// because `schema_gen::law_09`'s own doc comment requires its file to be
/// "the sole/alphabetically-first schema bound to a mint key" for its map to
/// take effect at all — a constraint the compositor's schema *naming*
/// happens to satisfy today, but that a delete prohibition must not be
/// allowed to depend on, since `deletable` is meant as an unconditional
/// prohibition, not a per-property allowlist a single first-matching schema
/// is expected to own. If `generated-law-16` (or any other schema declaring
/// `deletable: false`) sorted *after* another schema bound to the same key —
/// exactly the case here, since `"generated-law-16"` sorts after
/// `"generated-law-09"` — a first-match resolution would silently never see
/// it. Instead, every schema `key` is bound to is consulted, and the most
/// restrictive answer wins: `Some(false)` from *any* of them makes the whole
/// document non-deletable, regardless of what any other bound schema says or
/// leaves unsaid; only when no bound schema says `false` does an explicit
/// `Some(true)` apply; absent from every bound schema is `None` (deletable
/// by default, AB9-shaped).
pub fn resolve_deletable_rule_in(
    schemas_dir: &Path,
    config: &Configuration,
    key: &Key,
) -> Option<bool> {
    let Ok(bindings) = SchemaBindings::compile(&config.schemas) else {
        return None;
    };
    let mut saw_true = false;
    for name in bindings.schemas_for(key.as_str()) {
        match schema_deletable_rule(schemas_dir, name) {
            Some(false) => return Some(false),
            Some(true) => saw_true = true,
            None => {}
        }
    }
    if saw_true {
        Some(true)
    } else {
        None
    }
}

/// Resolves `key`'s `mutable:` rules via [`resolve_mutability_rules`]
/// (cwd-rooted schemas directory — see that function's doc comment;
/// [`check_write_permission_for_content_in`] is the explicit-root
/// equivalent), and calls [`check_write_permission`] against
/// [`PropertyRef::Body`] — the one helper every whole-document-rewrite call
/// site (WP-02..WP-13) uses so document/schema/property resolution is
/// implemented once, not re-implemented per caller (the "one mechanism, not
/// two" principle of `m2/design-enforcement-modes`).
///
/// `content` is whatever markdown is about to reach durable storage for
/// `key` — the new content for a create/update, or (for a removal) the
/// document's content as it exists on disk immediately before the removal
/// (passed as both `content` and `prior_content`, since a removal's
/// "resulting" content is, in effect, its own unchanged prior content —
/// see below).
///
/// `prior_content` is `key`'s content *as it stands on disk right now*,
/// before this write — `None` when there is no such content (a genuine
/// create). This is the fix for M2's freeze-bypass defect
/// (`m2/design-freeze-semantics`): the predicate used to be evaluated
/// against `content` alone (the outgoing/resulting write), which made it
/// impossible to enforce a rule about a *transition* — a single call that
/// set `freeze: false` and changed another field was validated only against
/// the (now unfrozen) result, and both changes landed. The rule this
/// restores: a write to a document that is frozen *as it stands before the
/// write* is rejected, unless the write's sole effect is lifting freeze
/// (checked by [`is_solitary_unfreeze`] — identical frontmatter other than
/// `freeze`, identical body, freeze going from effectively frozen to
/// effectively unfrozen). If `prior_content` is `None` or not itself
/// frozen, no freeze-related restriction applies here — freezing a
/// previously-unfrozen (or brand-new) document, plus other changes in the
/// same write, stays unrestricted by design (`m2/design-freeze-semantics`:
/// "Freezing is not restricted").
///
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
    prior_content: Option<&str>,
    operation: WriteOperation,
) -> Result<(), WritePermissionError> {
    let mutability = resolve_mutability_rules(config, key);
    let deletable = resolve_deletable_rule(config, key);
    check_write_permission_with_mutability(key, content, prior_content, mutability, operation, deletable)
}

/// Same as [`check_write_permission_for_content`], but resolving `mutable:`
/// rules from the caller-supplied `schemas_dir` rather than one derived
/// from the process's current directory (see [`resolve_mutability_rules_in`]).
pub fn check_write_permission_for_content_in(
    config: &Configuration,
    schemas_dir: &Path,
    key: &Key,
    content: &str,
    prior_content: Option<&str>,
    operation: WriteOperation,
) -> Result<(), WritePermissionError> {
    let mutability = resolve_mutability_rules_in(schemas_dir, config, key);
    let deletable = resolve_deletable_rule_in(schemas_dir, config, key);
    check_write_permission_with_mutability(key, content, prior_content, mutability, operation, deletable)
}

/// The single site freeze (LAW-12/13), per-property mutability (LAW-09), and
/// the delete prohibition (LAW-16, M4/R1) are evaluated together for a
/// whole-document write/removal.
///
/// # The delete prohibition is a separate check (M4/R1, `m2/design-deletion-carrier`)
///
/// `deletable` is resolved and checked entirely independently of
/// `mutability`: it is consulted only when `operation` is
/// [`WriteOperation::Delete`], its own `Some(false)`/`Some(true)`/`None`
/// tri-state is never derived from (or folded into) any `MutabilityRule`,
/// and it produces its own [`WritePermissionError::DeleteProhibited`]
/// rather than reusing [`WritePermissionError::PropertyImmutable`]. This is
/// deliberate, not an oversight: `m2/design-deletion-carrier` requires the
/// deletion prohibition to hold even where a document's `mutable:` map
/// (LAW-09's own construct) would, as an unrelated side effect of diffing
/// `content = ""` against `prior_content`, also happen to reject the same
/// removal — the two must not depend on each other, since a future
/// narrowing of the body-immutability rule must not silently stop
/// protecting deletion (and, symmetrically, a future change to `deletable`
/// resolution must not silently affect body-immutability). Both checks may
/// legitimately fire together on the same rejected delete; neither is
/// required for the other to be correct.
fn check_write_permission_with_mutability(
    key: &Key,
    content: &str,
    prior_content: Option<&str>,
    mutability: Vec<MutabilityRule>,
    operation: WriteOperation,
    deletable: Option<bool>,
) -> Result<(), WritePermissionError> {
    let prior_frontmatter = prior_content.and_then(parse_leading_frontmatter);
    let prior_frozen = prior_frontmatter
        .as_ref()
        .map(frontmatter_is_frozen)
        .unwrap_or(false);

    // The bypass fix: reject unless this write's sole effect is lifting
    // freeze. Short-circuits before mutability even gets a say, since a
    // solitary unfreeze by definition changes nothing else, and a rejected
    // write to a still-frozen document is rejected as `Frozen`, not
    // re-litigated as a mutability question.
    if prior_frozen {
        let prior = prior_content.expect("prior_frozen implies prior_content is Some");
        return if is_solitary_unfreeze(prior, content) {
            Ok(())
        } else {
            Err(WritePermissionError::Frozen { key: key.clone() })
        };
    }

    // M4/R1 (LAW-16, `m2/design-deletion-carrier`): the delete prohibition,
    // evaluated as its own, separate check -- see this function's own doc
    // comment ("The delete prohibition is a separate check") for why it
    // must not be folded into, or inferred from, the mutability loop below.
    // Only consulted for an actual removal (`operation ==
    // WriteOperation::Delete`); a create/update is never affected by
    // `deletable`, regardless of what `content` happens to look like.
    // Reached only once freeze has already had its say (freeze still
    // dominates, per LAW-13, unchanged from before this fix): a frozen
    // document is rejected as `Frozen` above, never reaching this branch.
    if operation == WriteOperation::Delete && deletable == Some(false) {
        return Err(WritePermissionError::DeleteProhibited { key: key.clone() });
    }

    // Not frozen prior to this write (including "no prior document at
    // all"): `document` here is deliberately built from the *prior* state
    // (empty when there is none), never from `content` (the outgoing
    // write) — see `check_write_permission`'s doc comment. This is what
    // keeps "freezing a previously-unfrozen document plus other changes in
    // the same write" unrestricted: `is_frozen` inside `check_write_
    // permission` is evaluated against the prior (unfrozen) state, not
    // against `content`, which may itself now carry `freeze: true`.
    let document = Document {
        blocks: Vec::new(),
        frontmatter: prior_frontmatter.clone(),
    };

    // D1 fix: this whole-document-content entry point used to check every
    // write unconditionally against `PropertyRef::Body`, regardless of
    // which property the write actually targeted — so a `mutable: false`
    // rule on `$content` rejected a write that never touched the body at
    // all. Per `property_touched`'s doc comment, resolve which property (or
    // properties) this write actually changed, and check the mutability
    // rule for each one that is both touched and schema-declared — not a
    // hardcoded `$content` check. A property this write never touches is
    // never evaluated, regardless of its rule; a property with no rule at
    // all stays mutable by default (AB9), exactly as `check_write_
    // permission` already enforces.
    let prior_frontmatter_map = prior_frontmatter.unwrap_or_default();
    let next_frontmatter_map = parse_leading_frontmatter(content).unwrap_or_default();
    let (_, prior_body) = prior_content.map(split_raw_frontmatter).unwrap_or((None, ""));
    let (_, next_body) = split_raw_frontmatter(content);

    for rule in &mutability {
        if !property_touched(
            &prior_frontmatter_map,
            &next_frontmatter_map,
            prior_body,
            next_body,
            &rule.property,
        ) {
            continue;
        }
        // Reuse the single shared predicate (this module's own doc
        // comment: "the one function every write path ... must call") for
        // the actual mutable/immutable decision on this touched property,
        // rather than re-implementing the `!rule.mutable` check here.
        check_write_permission(&document, key, &rule.property, &mutability)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn key(name: &str) -> Key {
        Key::name(name)
    }

    #[test]
    fn unfrozen_document_with_no_mutability_rules_allows_body_write() {
        let document = empty_document();
        let result =
            check_write_permission(&document, &key("docs/one"), &PropertyRef::Body, &[]);
        assert!(result.is_ok());
    }

    #[test]
    fn unfrozen_document_allows_frontmatter_write() {
        let document = document_with_frontmatter("status: active\n");
        let property = PropertyRef::Frontmatter(FieldPath::from_dotted("status"));
        let result = check_write_permission(&document, &key("docs/one"), &property, &[]);
        assert!(result.is_ok());
    }

    #[test]
    fn frozen_document_rejects_body_write() {
        let document = document_with_frontmatter("freeze: true\n");
        let doc_key = key("docs/one");
        let result = check_write_permission(&document, &doc_key, &PropertyRef::Body, &[]);
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
        let result = check_write_permission(&document, &doc_key, &property, &[]);
        assert_eq!(result, Err(WritePermissionError::Frozen { key: doc_key }));
    }

    #[test]
    fn freeze_false_does_not_reject() {
        let document = document_with_frontmatter("freeze: false\n");
        let result =
            check_write_permission(&document, &key("docs/one"), &PropertyRef::Body, &[]);
        assert!(result.is_ok());
    }

    #[test]
    fn non_boolean_freeze_value_does_not_reject() {
        // Only a literal `true` freezes; a malformed marker (e.g. a string)
        // is not silently treated as frozen.
        let document = document_with_frontmatter("freeze: \"yes\"\n");
        let result =
            check_write_permission(&document, &key("docs/one"), &PropertyRef::Body, &[]);
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
    fn check_write_permission_for_content_rejects_a_write_to_an_already_frozen_document() {
        // The M2 bypass fix: the write being *evaluated* (`content`) is not
        // itself what freezes the document — the document is *already*
        // frozen on disk (`prior_content`), and this write changes the body
        // too, so it is rejected as a whole (not a solitary unfreeze).
        let config = Configuration::default();
        let doc_key = key("frozen-doc");
        let prior = "---\nfreeze: true\n---\n\n# Frozen\n\noriginal body\n";
        let next = "---\nfreeze: true\n---\n\n# Frozen\n\nchanged body\n";
        let result =
            check_write_permission_for_content(&config, &doc_key, next, Some(prior), WriteOperation::Write);
        assert_eq!(result, Err(WritePermissionError::Frozen { key: doc_key }));
    }

    #[test]
    fn check_write_permission_for_content_allows_an_unfrozen_document() {
        let config = Configuration::default();
        let key = key("plain-doc");
        let content = "# Plain\n\nbody\n";
        let result = check_write_permission_for_content(&config, &key, content, None, WriteOperation::Write);
        assert!(result.is_ok());
    }

    /// The bypass itself, reproduced at this level and proven closed: a
    /// single write that both lifts freeze (`freeze: false`) *and* changes
    /// another field must still be rejected — its effect is not solely
    /// lifting freeze, so the freeze guarantee still applies.
    #[test]
    fn a_write_that_lifts_freeze_and_changes_another_field_is_still_rejected() {
        let config = Configuration::default();
        let doc_key = key("frozen-doc");
        let prior = "---\nfreeze: true\nstatus: draft\n---\n\n# Frozen\n\nbody\n";
        let next = "---\nfreeze: false\nstatus: changed\n---\n\n# Frozen\n\nbody\n";
        let result =
            check_write_permission_for_content(&config, &doc_key, next, Some(prior), WriteOperation::Write);
        assert_eq!(result, Err(WritePermissionError::Frozen { key: doc_key }));
    }

    /// A solitary unfreeze — `freeze: false`, nothing else different —
    /// succeeds even though the prior document was frozen.
    #[test]
    fn a_solitary_unfreeze_via_explicit_false_succeeds() {
        let config = Configuration::default();
        let doc_key = key("frozen-doc");
        let prior = "---\nfreeze: true\nstatus: draft\n---\n\n# Frozen\n\nbody\n";
        let next = "---\nfreeze: false\nstatus: draft\n---\n\n# Frozen\n\nbody\n";
        let result =
            check_write_permission_for_content(&config, &doc_key, next, Some(prior), WriteOperation::Write);
        assert!(result.is_ok());
    }

    /// Effective state, not literal property: removing the `freeze` key
    /// outright is an equally solitary unfreeze as setting it `false`.
    #[test]
    fn a_solitary_unfreeze_via_marker_removal_succeeds() {
        let config = Configuration::default();
        let doc_key = key("frozen-doc");
        let prior = "---\nfreeze: true\nstatus: draft\n---\n\n# Frozen\n\nbody\n";
        let next = "---\nstatus: draft\n---\n\n# Frozen\n\nbody\n";
        let result =
            check_write_permission_for_content(&config, &doc_key, next, Some(prior), WriteOperation::Write);
        assert!(result.is_ok());
    }

    /// Freezing is not restricted: a write that sets freeze on a
    /// *previously unfrozen* document may carry other changes in the same
    /// write — no guarantee is engaged, since the document was not frozen
    /// when the predicate ran.
    #[test]
    fn freezing_a_previously_unfrozen_document_plus_other_changes_succeeds() {
        let config = Configuration::default();
        let doc_key = key("plain-doc");
        let prior = "---\nstatus: draft\n---\n\n# Plain\n\nbody\n";
        let next = "---\nfreeze: true\nstatus: changed\n---\n\n# Plain\n\nnew body\n";
        let result =
            check_write_permission_for_content(&config, &doc_key, next, Some(prior), WriteOperation::Write);
        assert!(result.is_ok());
    }

    /// A brand-new document (no prior content at all) carrying `freeze:
    /// true` from the moment it is created succeeds: there is no prior
    /// frozen state to violate, and freezing itself is unrestricted.
    #[test]
    fn creating_a_document_that_is_frozen_from_the_start_succeeds() {
        let config = Configuration::default();
        let doc_key = key("brand-new");
        let content = "---\nfreeze: true\n---\n\n# Brand New\n\nbody\n";
        let result = check_write_permission_for_content(&config, &doc_key, content, None, WriteOperation::Write);
        assert!(result.is_ok());
    }

    /// A no-op rewrite of an already-frozen document (same content,
    /// still frozen) is not a solitary unfreeze — freeze was never
    /// lifted — so it is still rejected.
    #[test]
    fn rewriting_a_frozen_document_unchanged_is_still_rejected() {
        let config = Configuration::default();
        let doc_key = key("frozen-doc");
        let content = "---\nfreeze: true\n---\n\n# Frozen\n\nbody\n";
        let result =
            check_write_permission_for_content(&config, &doc_key, content, Some(content), WriteOperation::Write);
        assert_eq!(result, Err(WritePermissionError::Frozen { key: doc_key }));
    }

    /// AB9 / default-mutable: a corpus of documents and properties with no
    /// `mutable:` rule declared anywhere — the shape every document had
    /// before T11, and the shape any document without the construct still
    /// has today — sees zero rejections from this construct.
    #[test]
    fn default_mutable_corpus_sees_zero_rejections() {
        let document = empty_document();
        let properties = [
            PropertyRef::Body,
            PropertyRef::from_selector("status"),
            PropertyRef::from_selector("owner.name"),
            PropertyRef::from_selector("tags"),
        ];
        for doc_key in ["docs/one", "docs/two", "notes/three"] {
            for property in &properties {
                let result = check_write_permission(&document, &key(doc_key), property, &[]);
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

        let body_result =
            check_write_permission(&document, &doc_key, &PropertyRef::Body, &mutability);
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
        let mutability = vec![MutabilityRule {
            selector: "$content".to_string(),
            property: PropertyRef::Body,
            mutable: false,
        }];
        let result = check_write_permission(
            &document,
            &key("docs/one"),
            &PropertyRef::from_selector("status"),
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
        let mutability = vec![MutabilityRule {
            selector: "status".to_string(),
            property: PropertyRef::from_selector("status"),
            mutable: false,
        }];
        let result = check_write_permission(
            &document,
            &key("docs/one"),
            &PropertyRef::from_selector("status"),
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
        let mutability = vec![MutabilityRule {
            selector: "archived".to_string(),
            property: PropertyRef::from_selector("archived"),
            mutable: true,
        }];

        let result = check_write_permission(
            &document,
            &doc_key,
            &PropertyRef::from_selector("archived"),
            &mutability,
        );

        assert_eq!(result, Err(WritePermissionError::Frozen { key: doc_key }));
    }

    // =======================================================================
    // D1 (M4-extension defect) fix: `check_write_permission_with_mutability`
    // (reached via `check_write_permission_for_content[_in]`, WP-02..WP-13's
    // whole-document-content entry point) used to check every write
    // unconditionally against `PropertyRef::Body`, so a `mutable: false`
    // rule on `$content` rejected every write to the document — including
    // ones that never touched the body at all. These five cases are the
    // matrix this fix was scoped against: one mint-origin-shaped fixture,
    // body immutable, one property explicitly schema-marked mutable, one
    // property explicitly schema-marked *immutable* (see the judgment-call
    // note on case 3 below), and (in a sixth, supplementary case) a property
    // carrying no rule at all, to prove AB9's confirmed default-mutable
    // convention still holds through this same content-based path.
    // =======================================================================

    /// The D1 fixture's `mutable:` rules, shared by every case below:
    /// `$content` immutable, `status` and `owner` explicitly mutable,
    /// `archived` explicitly immutable. `tags` (used by the supplementary
    /// AB9 case) is deliberately left off this list — a property with no
    /// rule at all.
    fn d1_mutability() -> Vec<MutabilityRule> {
        vec![
            MutabilityRule {
                selector: "$content".to_string(),
                property: PropertyRef::Body,
                mutable: false,
            },
            MutabilityRule {
                selector: "status".to_string(),
                property: PropertyRef::from_selector("status"),
                mutable: true,
            },
            MutabilityRule {
                selector: "owner".to_string(),
                property: PropertyRef::from_selector("owner"),
                mutable: true,
            },
            MutabilityRule {
                selector: "archived".to_string(),
                property: PropertyRef::from_selector("archived"),
                mutable: false,
            },
        ]
    }

    const D1_PRIOR: &str = "---\nstatus: draft\nowner: alice\narchived: false\n---\n\n# Doc\n\nOriginal body.\n";

    /// Case 1/5: body-only write. Already correct before this fix — proven
    /// here as a non-regression, not the fix target itself.
    #[test]
    fn d1_case1_body_only_write_is_rejected() {
        let doc_key = key("mind/case1");
        let next = "---\nstatus: draft\nowner: alice\narchived: false\n---\n\n# Doc\n\nChanged body.\n";
        let result = check_write_permission_with_mutability(
            &doc_key,
            next,
            Some(D1_PRIOR),
            d1_mutability(),
            WriteOperation::Write,
            None,
        );
        assert_eq!(
            result,
            Err(WritePermissionError::PropertyImmutable {
                key: doc_key,
                property: PropertyRef::Body,
                selector: "$content".to_string(),
            })
        );
    }

    /// Case 2/5: the fix target. A write to `status` only (explicitly
    /// schema-marked mutable, no body touch) must now succeed, where it was
    /// previously rejected identically to a body write (D1's own signature:
    /// rejection referencing `$content` regardless of the property actually
    /// targeted).
    #[test]
    fn d1_case2_schema_mutable_property_only_write_now_succeeds() {
        let doc_key = key("mind/case2");
        let next = "---\nstatus: approved\nowner: alice\narchived: false\n---\n\n# Doc\n\nOriginal body.\n";
        let result = check_write_permission_with_mutability(
            &doc_key,
            next,
            Some(D1_PRIOR),
            d1_mutability(),
            WriteOperation::Write,
            None,
        );
        assert!(result.is_ok(), "{result:?}");
    }

    /// Case 3/5: a write to a property the schema explicitly marks
    /// `mutable: false` (`archived`) is rejected, naming that property —
    /// not `$content`.
    ///
    /// Judgment call: the task's own matrix describes this case's target
    /// property as merely "not marked mutable" and glosses it as "not
    /// mutable by default per AB9's convention". That gloss contradicts
    /// this codebase's own confirmed default: a property absent from a
    /// schema's `mutable:` mapping entirely is mutable
    /// (`default_mutable_corpus_sees_zero_rejections` /
    /// `property_absent_from_mutable_mapping_is_mutable`, both already
    /// above in this file, plus `m2/design-extensions`'s "AB9" itself). A
    /// merely-unmarked property write would succeed, not reject, so it
    /// cannot exercise "REJECTED" here. This case therefore uses a property
    /// explicitly marked `mutable: false` (`archived`) instead, which is
    /// what actually demonstrates a genuine non-mutable-property rejection
    /// under this codebase's real semantics. The unmarked-property case is
    /// covered separately, and correctly, by
    /// `d1_unmarked_property_write_succeeds_ab9_default` below.
    #[test]
    fn d1_case3_explicitly_immutable_property_write_is_rejected() {
        let doc_key = key("mind/case3");
        let next = "---\nstatus: draft\nowner: alice\narchived: true\n---\n\n# Doc\n\nOriginal body.\n";
        let result = check_write_permission_with_mutability(
            &doc_key,
            next,
            Some(D1_PRIOR),
            d1_mutability(),
            WriteOperation::Write,
            None,
        );
        assert_eq!(
            result,
            Err(WritePermissionError::PropertyImmutable {
                key: doc_key,
                property: PropertyRef::from_selector("archived"),
                selector: "archived".to_string(),
            })
        );
    }

    /// Case 4/5: bundling a legitimate mutable-property change (`status`)
    /// with a body change in one write does not rescue the write — body is
    /// touched, so the whole write is still rejected.
    #[test]
    fn d1_case4_body_plus_legit_mutable_property_bundle_is_rejected() {
        let doc_key = key("mind/case4");
        let next = "---\nstatus: approved\nowner: alice\narchived: false\n---\n\n# Doc\n\nChanged body.\n";
        let result = check_write_permission_with_mutability(
            &doc_key,
            next,
            Some(D1_PRIOR),
            d1_mutability(),
            WriteOperation::Write,
            None,
        );
        assert_eq!(
            result,
            Err(WritePermissionError::PropertyImmutable {
                key: doc_key,
                property: PropertyRef::Body,
                selector: "$content".to_string(),
            })
        );
    }

    /// Case 5/5: two legitimate mutable-property changes together (`status`
    /// and `owner`), neither touching the body, succeed.
    #[test]
    fn d1_case5_two_legit_mutable_property_changes_together_succeed() {
        let doc_key = key("mind/case5");
        let next = "---\nstatus: approved\nowner: bob\narchived: false\n---\n\n# Doc\n\nOriginal body.\n";
        let result = check_write_permission_with_mutability(
            &doc_key,
            next,
            Some(D1_PRIOR),
            d1_mutability(),
            WriteOperation::Write,
            None,
        );
        assert!(result.is_ok(), "{result:?}");
    }

    /// Supplementary: AB9's default-mutable guarantee, proven through this
    /// same content-based path — a property carrying no `mutable:` rule at
    /// all (`tags`, absent from `d1_mutability()`) may be freely changed
    /// even though the same document's body is immutable.
    #[test]
    fn d1_unmarked_property_write_succeeds_ab9_default() {
        let doc_key = key("mind/case6");
        let prior = "---\nstatus: draft\nowner: alice\narchived: false\ntags: [a]\n---\n\n# Doc\n\nOriginal body.\n";
        let next = "---\nstatus: draft\nowner: alice\narchived: false\ntags: [a, b]\n---\n\n# Doc\n\nOriginal body.\n";
        let result =
            check_write_permission_with_mutability(&doc_key, next, Some(prior), d1_mutability(), WriteOperation::Write, None);
        assert!(result.is_ok(), "{result:?}");
    }

    /// Sanity check on `property_touched` itself: a field present in the
    /// prior frontmatter and absent from the next (an unset) counts as
    /// touched, and vice versa — not just "both present but different".
    #[test]
    fn d1_property_touched_treats_presence_change_as_touched() {
        let prior_fm: Frontmatter = serde_yaml::from_str("status: draft\n").unwrap();
        let next_fm: Frontmatter = serde_yaml::from_str("owner: alice\n").unwrap();
        let status = PropertyRef::from_selector("status");
        assert!(property_touched(&prior_fm, &next_fm, "", "", &status));
        let owner = PropertyRef::from_selector("owner");
        assert!(property_touched(&prior_fm, &next_fm, "", "", &owner));
    }

    // =======================================================================
    // D4 (M4-extension defect) fix: `iwe delete` on a document with an
    // immutable body (or any other `mutable: false`/`freeze: true`
    // property) used to panic (exit 101); once D1 landed, the same
    // scenario instead silently succeeded, because `diwe::fs::
    // apply_changes`'s removal check used to call this predicate with the
    // document's existing content as *both* `content` and `prior_content`
    // — indistinguishable, under D1's touched/untouched diff, from a
    // no-op write. The actual fix is the call site
    // (`crates/diwe/src/fs.rs`'s removal loop now passes `content = ""`),
    // but these two tests pin down this predicate's own behavior for
    // exactly the call shape a deletion now produces: `content = ""`,
    // `prior_content = Some(<the document's on-disk content>)`.
    // =======================================================================

    /// A deletion-shaped call (`content = ""`) against a document whose
    /// schema marks the body immutable is rejected — the fix target.
    #[test]
    fn d4_deletion_shaped_call_rejects_a_document_with_an_immutable_body() {
        let doc_key = key("mind/mint-origin");
        let prior = "---\nstatus: draft\n---\n\n# Doc\n\nOriginal body.\n";
        let mutability = vec![MutabilityRule {
            selector: "$content".to_string(),
            property: PropertyRef::Body,
            mutable: false,
        }];

        let result = check_write_permission_with_mutability(
            &doc_key,
            "",
            Some(prior),
            mutability,
            WriteOperation::Delete,
            None,
        );

        assert_eq!(
            result,
            Err(WritePermissionError::PropertyImmutable {
                key: doc_key,
                property: PropertyRef::Body,
                selector: "$content".to_string(),
            })
        );
    }

    /// A deletion-shaped call against an ordinary document (no `mutable:`
    /// rule at all) still succeeds — AB9's default-mutable guarantee holds
    /// for deletion exactly as it does for update/create.
    #[test]
    fn d4_deletion_shaped_call_succeeds_for_an_ordinary_document() {
        let doc_key = key("mind/ordinary");
        let prior = "---\nstatus: draft\n---\n\n# Doc\n\nOriginal body.\n";

        let result = check_write_permission_with_mutability(
            &doc_key,
            "",
            Some(prior),
            vec![],
            WriteOperation::Delete,
            None,
        );

        assert!(result.is_ok(), "{result:?}");
    }

    // =======================================================================
    // M4/R1 (LAW-16, `m2/design-deletion-carrier`): the delete prohibition
    // (`deletable: false`), proven as a genuinely separate construct from
    // LAW-09's `mutable:`/body-immutability rule -- not incidental fallout
    // from it. Each case below is chosen so the two mechanisms' rejections
    // can be told apart (different error variant) and so their independence
    // can be exercised directly: a case where `mutable:` alone would allow a
    // delete but `deletable: false` still rejects it (case-in-the-absence-
    // of-mutable), and a case where `deletable` is entirely absent but
    // `mutable: false` still rejects the delete on its own (already proven
    // above by the D4 tests) -- together showing neither depends on the
    // other to produce the correct rejection.
    // =======================================================================

    /// The core case: a delete against a document whose schema declares
    /// `deletable: false` is rejected as `DeleteProhibited` -- not
    /// `PropertyImmutable` -- even though this document's `mutable:` map is
    /// empty (no body-immutability rule at all, so LAW-09's construct would
    /// have nothing to say about this same removal). Proves the rejection
    /// does not depend on `mutable:` firing too.
    #[test]
    fn delete_of_a_non_deletable_document_is_rejected_even_with_no_mutable_rule_at_all() {
        let doc_key = key("mind/mint-origin");
        let prior = "---\nprovenance:\n  origin: mint:example@1.0.0\n---\n\n# Doc\n\nOriginal body.\n";

        let result = check_write_permission_with_mutability(
            &doc_key,
            "",
            Some(prior),
            vec![],
            WriteOperation::Delete,
            Some(false),
        );

        assert_eq!(
            result,
            Err(WritePermissionError::DeleteProhibited {
                key: doc_key.clone()
            })
        );
        assert_ne!(
            result,
            Err(WritePermissionError::PropertyImmutable {
                key: doc_key,
                property: PropertyRef::Body,
                selector: "$content".to_string(),
            })
        );
    }

    /// The mirror case: a document whose body *is* schema-marked immutable,
    /// but whose schema does not declare `deletable` at all (`None`) --
    /// deletion is rejected purely by the pre-existing `mutable:` mechanism
    /// (D4's own fix, reproduced here with `deletable` explicitly absent),
    /// proving that mechanism does not depend on `deletable` being declared
    /// at all.
    #[test]
    fn delete_still_rejected_by_mutable_rule_alone_when_deletable_is_never_declared() {
        let doc_key = key("mind/mint-origin-2");
        let prior = "---\nstatus: draft\n---\n\n# Doc\n\nOriginal body.\n";
        let mutability = vec![MutabilityRule {
            selector: "$content".to_string(),
            property: PropertyRef::Body,
            mutable: false,
        }];

        let result = check_write_permission_with_mutability(
            &doc_key,
            "",
            Some(prior),
            mutability,
            WriteOperation::Delete,
            None,
        );

        assert_eq!(
            result,
            Err(WritePermissionError::PropertyImmutable {
                key: doc_key,
                property: PropertyRef::Body,
                selector: "$content".to_string(),
            })
        );
    }

    /// The patch path (`m2/design-deletion-carrier`'s "Update of a
    /// schema-mutable property ... permitted"): `deletable: false` on a
    /// document must not block an ordinary property *update* -- only an
    /// actual delete. A write (`WriteOperation::Write`) to a mutable
    /// property succeeds even though the same document's `deletable` rule
    /// is `Some(false)`.
    #[test]
    fn deletable_false_does_not_block_an_update_to_a_mutable_property() {
        let doc_key = key("mind/mint-origin");
        let prior = "---\nstatus: draft\n---\n\n# Doc\n\nOriginal body.\n";
        let next = "---\nstatus: approved\n---\n\n# Doc\n\nOriginal body.\n";
        let mutability = vec![
            MutabilityRule {
                selector: "$content".to_string(),
                property: PropertyRef::Body,
                mutable: false,
            },
            MutabilityRule {
                selector: "status".to_string(),
                property: PropertyRef::from_selector("status"),
                mutable: true,
            },
        ];

        let result = check_write_permission_with_mutability(
            &doc_key,
            next,
            Some(prior),
            mutability,
            WriteOperation::Write,
            Some(false),
        );

        assert!(result.is_ok(), "{result:?}");
    }

    /// The body-immutability side of the same three-way distinction: a
    /// body-only *update* (not a delete) on the same non-deletable document
    /// is still rejected -- but via `PropertyImmutable`, never
    /// `DeleteProhibited`, since `deletable` is never consulted for
    /// `WriteOperation::Write`.
    #[test]
    fn deletable_false_document_still_rejects_a_body_update_via_mutable_rule_not_deletable() {
        let doc_key = key("mind/mint-origin");
        let prior = "---\nstatus: draft\n---\n\n# Doc\n\nOriginal body.\n";
        let next = "---\nstatus: draft\n---\n\n# Doc\n\nChanged body.\n";
        let mutability = vec![MutabilityRule {
            selector: "$content".to_string(),
            property: PropertyRef::Body,
            mutable: false,
        }];

        let result = check_write_permission_with_mutability(
            &doc_key,
            next,
            Some(prior),
            mutability,
            WriteOperation::Write,
            Some(false),
        );

        assert_eq!(
            result,
            Err(WritePermissionError::PropertyImmutable {
                key: doc_key,
                property: PropertyRef::Body,
                selector: "$content".to_string(),
            })
        );
    }

    /// `deletable` absent (`None`) is deletable by default (AB9-shaped): a
    /// delete of an ordinary document with no `deletable` rule at all
    /// succeeds.
    #[test]
    fn deletable_absent_allows_delete_by_default() {
        let doc_key = key("mind/ordinary");
        let prior = "---\nstatus: draft\n---\n\n# Doc\n\nOriginal body.\n";

        let result = check_write_permission_with_mutability(
            &doc_key,
            "",
            Some(prior),
            vec![],
            WriteOperation::Delete,
            None,
        );

        assert!(result.is_ok(), "{result:?}");
    }

    /// Freeze still dominates the delete prohibition exactly as it dominates
    /// mutability (LAW-13): a frozen document's delete is rejected as
    /// `Frozen`, not `DeleteProhibited`, even when `deletable` is explicitly
    /// `Some(true)`.
    #[test]
    fn freeze_dominates_an_explicitly_deletable_document() {
        let doc_key = key("mind/frozen");
        let prior = "---\nfreeze: true\n---\n\n# Doc\n\nOriginal body.\n";

        let result = check_write_permission_with_mutability(
            &doc_key,
            "",
            Some(prior),
            vec![],
            WriteOperation::Delete,
            Some(true),
        );

        assert_eq!(result, Err(WritePermissionError::Frozen { key: doc_key }));
    }
}
