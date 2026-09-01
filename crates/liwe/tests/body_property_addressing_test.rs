//! T9 (M2, knowledge-compositor) — independent, contract-only tests for the
//! body-as-property retrofit.
//!
//! Written by Test-builder, working independently of Developer: these tests
//! were derived from T9's acceptance criteria alone, never from reading
//! Developer's implementation. Developer's actual retrofit lives elsewhere
//! (possibly not landed at all in this worktree) and may take a different
//! shape than the stub below.
//!
//! ============================================================================
//! STUB DISCLAIMER — everything in the `stub` module below is Test-builder's
//! own, non-authoritative, minimal implementation, written only so these
//! tests are executable. It is NOT the retrofit. In particular:
//!   - `stub::BODY_PROPERTY_KEY` is Test-builder's proposed reserved
//!     identifier, chosen to reuse the codebase's *existing* `$content`
//!     pseudo-field name (`liwe::query::document::PseudoField::Content`)
//!     rather than invent a new one — see the reasoning in the collision
//!     test below. Developer may choose a different identifier.
//!   - `stub::is_property_writable` answers "is this document's frontmatter
//!     unrestricted right now" (today's actual mutability state: nothing
//!     stops a write, because EXT-FREEZE/EXT-MUTABILITY are T10/T11, not
//!     this task). It exists only to give the "ask uniformly" behavior a
//!     concrete, callable shape to test against.
//! ============================================================================

use liwe::graph::Graph;
use liwe::markdown::MarkdownReader;
use liwe::model::Key;
use liwe::query::document::{is_operator_segment, FieldPath, PseudoField};
use liwe::query::filter::{resolve_path, Resolution};
use liwe::schema::{build_document, compile_schema};

/// Test-builder's own stub — see module-level disclaimer. Not authoritative.
mod stub {
    use super::*;

    /// Reserved identifier addressing the document body as a property.
    ///
    /// Chosen, rather than invented, to reuse
    /// `liwe::query::document::PseudoField::Content`'s existing `"$content"`
    /// spelling: the codebase already treats `$content` as the body's name
    /// in the projection grammar (`crates/liwe/src/query/document.rs`), so
    /// this retrofit does not need to teach a second name for "the body" —
    /// it needs to make the *existing* name askable for writability too.
    pub const BODY_PROPERTY_KEY: &str = "$content";

    /// Uniform address resolution: same input type (`&str`), same function,
    /// for both the body and a declared frontmatter property.
    pub enum PropertyAddress {
        Body,
        Frontmatter(FieldPath),
    }

    pub fn resolve_address(path: &str) -> PropertyAddress {
        if path == BODY_PROPERTY_KEY {
            PropertyAddress::Body
        } else {
            PropertyAddress::Frontmatter(FieldPath::from_dotted(path))
        }
    }

    /// "Is property P writable on document D?" — asked with the *same*
    /// signature for P = body and P = a declared frontmatter property.
    ///
    /// Today (pre-T10/T11) nothing in IWE restricts writes, so the only
    /// real gate is "does the document exist at all" — which is itself
    /// answerable identically for both addresses. That is deliberate: this
    /// task is about uniform *addressability*, not about implementing
    /// freeze/mutability semantics (T10/T11's job).
    pub fn is_property_writable(graph: &Graph, key: &Key, path: &str) -> bool {
        match resolve_address(path) {
            PropertyAddress::Body => graph.maybe_key(key).is_some(),
            PropertyAddress::Frontmatter(_) => graph.maybe_key(key).is_some(),
        }
    }
}

use stub::{is_property_writable, resolve_address, PropertyAddress, BODY_PROPERTY_KEY};

fn graph_from(content: &str) -> Graph {
    let mut graph = Graph::new();
    graph.from_markdown(Key::name("doc"), content, MarkdownReader::new());
    graph
}

// ---------------------------------------------------------------------------
// AC: "is property P writable on document D?" askable uniformly for P = body,
// using the same addressing as a declared frontmatter property.
// ---------------------------------------------------------------------------

#[test]
fn body_writability_is_askable_through_the_same_function_and_shape_as_a_frontmatter_property() {
    let graph = graph_from("---\nstatus: draft\n---\n# Title\n\nSome body text.\n");
    let key = Key::name("doc");

    // Same function, same &str-addressed input type, same bool output —
    // for a declared frontmatter property and for the body.
    let frontmatter_writable: bool = is_property_writable(&graph, &key, "status");
    let body_writable: bool = is_property_writable(&graph, &key, BODY_PROPERTY_KEY);

    assert!(frontmatter_writable, "declared frontmatter property should be writable");
    assert!(body_writable, "body, addressed uniformly, should be writable too");
}

#[test]
fn writability_of_a_nonexistent_document_is_uniformly_false_for_both_addresses() {
    let graph = graph_from("# Title\n\ntext\n");
    let missing = Key::name("does-not-exist");

    assert!(!is_property_writable(&graph, &missing, "status"));
    assert!(!is_property_writable(&graph, &missing, BODY_PROPERTY_KEY));
}

#[test]
fn nested_frontmatter_paths_and_body_share_the_same_address_resolution_entry_point() {
    // `resolve_address` is the single entry point both P's addressing goes
    // through; this asserts the entry point itself branches on the *value*
    // of the address, not on some separate call path for body vs frontmatter.
    let graph = graph_from(
        "---\nquery:\n  filter:\n    status: draft\n---\n# Title\n\nbody\n",
    );
    let key = Key::name("doc");

    assert!(is_property_writable(&graph, &key, "query.filter.status"));
    assert!(is_property_writable(&graph, &key, BODY_PROPERTY_KEY));

    match resolve_address("query.filter.status") {
        PropertyAddress::Frontmatter(path) => {
            assert_eq!(path.segments(), &["query", "filter", "status"]);
        }
        PropertyAddress::Body => panic!("dotted path must not resolve to Body"),
    }
    match resolve_address(BODY_PROPERTY_KEY) {
        PropertyAddress::Body => {}
        PropertyAddress::Frontmatter(_) => panic!("reserved body key must resolve to Body"),
    }
}

// ---------------------------------------------------------------------------
// AC: the reserved body-property identifier does not collide with any
// existing or plausible user-authorable frontmatter key.
//
// Collision risk, enumerated:
//  (1) A plausible frontmatter key literally equals the reserved identifier
//      as a string (e.g. someone writes `$content: ...` in frontmatter).
//  (2) The reserved identifier shadows an *already-reserved* name used
//      elsewhere in the codebase for a different purpose (a second concept
//      claiming the same spelling).
//  (3) The existing frontmatter path-resolution machinery
//      (`liwe::query::filter::resolve_path`) would have treated that same
//      string as reachable frontmatter data, so reserving it *takes away*
//      something that used to work.
// ---------------------------------------------------------------------------

#[test]
fn reserved_body_key_is_not_a_plausible_frontmatter_key() {
    // Drawn from real frontmatter keys already exercised in this codebase's
    // own tests/fixtures (crates/liwe/src/schema/document.rs) plus generic,
    // ordinary user-authored names.
    let plausible_user_frontmatter_keys = [
        "status", "title", "id", "priority", "tags", "_internal", "$foo", "query",
        "content", // bare, no `$` — the ordinary way a user would name this
    ];

    for key in plausible_user_frontmatter_keys {
        assert_ne!(
            key, BODY_PROPERTY_KEY,
            "plausible frontmatter key {key:?} must not equal the reserved body key"
        );
    }
}

#[test]
fn reserved_body_key_is_the_codebases_existing_reserved_name_not_a_fresh_claim() {
    // Risk (2): does not collide with a different existing reserved concept,
    // because it *is* that existing reserved concept (PseudoField::Content,
    // already used read-only in the projection grammar) — not a second,
    // competing name for the same idea.
    assert_eq!(
        PseudoField::from_selector(BODY_PROPERTY_KEY),
        Some(PseudoField::Content),
        "the reserved body-property key must line up with the codebase's existing $content pseudo-field"
    );

    // And it is unambiguously in the operator/reserved segment namespace
    // (leading `$`), the same namespace every other pseudo-field already
    // occupies (`$key`, `$title`, `$includedBy`, ...) — never the bare-name
    // namespace frontmatter fields are addressed through.
    assert!(is_operator_segment(BODY_PROPERTY_KEY));
}

#[test]
fn reserved_body_key_was_already_unreachable_as_frontmatter_via_the_existing_path_resolver() {
    // Risk (3), made concrete: even if a document's raw frontmatter mapping
    // literally contains a "$content" entry (legal YAML — see
    // crates/liwe/src/schema/document.rs's `validates_dollar_named_frontmatter_fields`
    // test for the same pattern with "$foo"), the *existing*, unmodified
    // `resolve_path` function already refuses to resolve any path segment
    // starting with `$` (`is_operator_segment`) — so reserving "$content"
    // for the body does not take away frontmatter reachability that
    // existed before this retrofit; that reachability never existed.
    let graph = graph_from(
        "---\n\"$content\": \"user data that happens to share the reserved spelling\"\nstatus: draft\n---\n# Title\n\nreal body text\n",
    );
    let key = Key::name("doc");

    let mapping = graph.frontmatter(&key).expect("frontmatter present");
    // The raw value is genuinely there in storage...
    assert!(mapping.get(serde_yaml::Value::String("$content".to_string())).is_some());

    // ...but was already Missing through the existing path resolver, before
    // and regardless of this retrofit.
    let resolution = resolve_path(mapping, &FieldPath::from_dotted("$content"));
    assert!(matches!(resolution, Resolution::Missing));

    // And under the uniform writability address, "$content" now
    // deterministically means Body — a documented, tested shadow of that
    // one literal spelling, not a silent ambiguity.
    match resolve_address("$content") {
        PropertyAddress::Body => {}
        PropertyAddress::Frontmatter(_) => panic!("must resolve to Body"),
    }
}

// ---------------------------------------------------------------------------
// AC: scope containment — validation/serialization paths NOT required for
// the addressing capability remain unaffected. The retrofit, properly
// scoped, should not have side effects beyond making body addressable.
// ---------------------------------------------------------------------------

#[test]
fn asking_about_writability_does_not_change_the_markdown_round_trip() {
    let content = "---\nstatus: draft\n---\n# Title\n\nbody paragraph\n";
    let graph = graph_from(content);
    let key = Key::name("doc");

    let frontmatter_before = graph.frontmatter(&key).cloned();
    let body_before = graph.to_markdown_skip_frontmatter(&key);

    // Ask about writability, several times, for both addresses. This takes
    // `&Graph` (immutable) — the type system already backs the claim that
    // this cannot mutate storage; these assertions make it concrete.
    let _ = is_property_writable(&graph, &key, "status");
    let _ = is_property_writable(&graph, &key, BODY_PROPERTY_KEY);
    let _ = is_property_writable(&graph, &key, "status");

    let frontmatter_after = graph.frontmatter(&key).cloned();
    let body_after = graph.to_markdown_skip_frontmatter(&key);

    assert_eq!(frontmatter_before, frontmatter_after, "frontmatter storage must be untouched");
    assert_eq!(body_before, body_after, "body serialization must be untouched");
}

#[test]
fn asking_about_writability_does_not_change_schema_validation_of_existing_constructs() {
    // A schema whose `frontmatter:`/`sections:` keywords are entirely
    // pre-existing, untouched by this retrofit.
    let schema = "\
frontmatter:
  type: object
  required: [status]
  properties:
    status: { type: string }
sections:
  - header: { const: Title }
";
    let graph = graph_from("---\nstatus: draft\n---\n# Title\n\nbody paragraph\n");
    let key = Key::name("doc");

    let compiled = compile_schema(schema).expect("schema compiles");
    let violations_before = compiled.validate(&build_document(&graph, &key, |_| 0));

    let _ = is_property_writable(&graph, &key, "status");
    let _ = is_property_writable(&graph, &key, BODY_PROPERTY_KEY);

    let violations_after = compiled.validate(&build_document(&graph, &key, |_| 0));

    assert_eq!(violations_before, violations_after);
    assert!(violations_before.is_empty(), "schema should validate cleanly either way");
}

// ---------------------------------------------------------------------------
// A criterion this task's stub cannot exercise, noted rather than skipped:
//
// Actual mutability enforcement (a write to a property marked immutable
// being rejected, per-property or via document-level freeze) is EXT-FREEZE
// / EXT-MUTABILITY, tasks T10/T11 in this same milestone — not yet built in
// any worktree. `is_property_writable` above always answers `true` for an
// existing document because that mirrors IWE's actual behavior today
// (nothing restricts writes yet). A future, real "is P writable" must
// additionally consult declared mutability/freeze state; that behavior has
// no construct to test against yet and is intentionally left untested here.
// ---------------------------------------------------------------------------
