//! T7 — commit-time schema validation over an index-backed affected set.
//!
//! # Independence notice
//!
//! Written by Test-builder, working independently of Developer on this
//! task: no Developer implementation exists in this worktree, so
//! `AffectedSetTransaction` below is Test-builder's **own, non-authoritative**
//! `Transaction` backend, built only far enough to exercise T7's acceptance
//! criteria end-to-end. It is not a proposal for the shipped design — a
//! real implementation may batch, cache, or route differently. Treat every
//! type in this file as test scaffolding, not production code.
//!
//! # Citations (design context's "locate `RefIndex` and `ViaWalk::inbound`
//! yourself")
//!
//! - `RefIndex`: `crates/liwe/src/graph/index.rs:8` (`pub struct RefIndex`).
//!   Its module (`crates/liwe/src/graph.rs`: `mod index;`) is
//!   crate-private, so external code — this file included — reaches it
//!   only through `Graph`'s public, index-backed wrapper methods:
//!   `Graph::get_reference_edges_to` (`crates/liwe/src/graph.rs:629`) and
//!   `Graph::get_inclusion_edges_to` (`crates/liwe/src/graph.rs:608`),
//!   both of which delegate to `self.index: RefIndex`
//!   (`crates/liwe/src/graph.rs:56`).
//! - `ViaWalk::inbound`: `crates/liwe/src/query/via.rs:53`, re-exported at
//!   `crates/liwe/src/query.rs:40` (`pub use via::ViaWalk;`).
//!
//! # Unbounded rule forms (M1's finding, checked against
//! `crates/diwe/src/schema.rs` as it exists on this branch)
//!
//! M1 says: "Unrestricted-filter rules (`asserts`/`links.target`/
//! `links.some`/`invariants`): whole-store — intrinsic to the rule shape."
//! Reading `crates/diwe/src/schema.rs` confirms the *mechanism* behind
//! that claim: `links.target` / `links.some` (the non-`$this` variants)
//! and `asserts`'s `that` are evaluated through `filter_set` /
//! `this_filter_set`, which call `liwe::query::evaluate(filter, graph)` —
//! a scan with no restriction to the touched document's neighborhood
//! (`crates/diwe/src/schema.rs:579`, `:819`); `check_invariants` does the
//! same over the whole graph (`crates/diwe/src/schema.rs:1157`). A write
//! to *any* document that flips whether it matches such a filter can flip
//! another, topologically unrelated document's validity, so no
//! `RefIndex`/`ViaWalk` closure can bound the affected set for these forms
//! — this is what `schema_declares_unbounded_rule` below (my own,
//! deliberately narrow YAML-level classifier, not a use of
//! `diwe::schema`'s private `LinkRule`/`AssertRule` types, which are not
//! introspectable from outside the crate) routes to whole-store
//! validation.
//!
//! Independent finding beyond what M1 named: `links.when` and
//! `requires.when` go through the exact same unrestricted
//! `evaluate(filter, graph)` primitive (`crates/diwe/src/schema.rs:840`,
//! `:980`) as `target`/`some`. M1's list does not name them, and this file
//! does not attempt to classify them (no test schema below uses a
//! cross-document `when`), but a complete routing implementation should
//! resolve whether `when` clauses need the same whole-store treatment —
//! recorded here as an open item, not adjudicated.
//!
//! # Folder siblings (M1's open item)
//!
//! Grepping `crates/diwe/src/schema.rs`'s rule-evaluation functions
//! (`check_links`, `check_requires`, `check_asserts`, `check_invariants`)
//! turns up no directory/folder-adjacency read of any kind — every
//! cross-document read in that file goes through the graph's link edges
//! (`RefIndex`/`ViaWalk`) or a whole-store `Filter` evaluation. The only
//! place a document's folder path matters at all is `SchemaBindings::
//! schemas_for` (glob matching a key's path string to decide which schema
//! binds, `crates/diwe/src/schema.rs:46`) — a schema-*binding* concern,
//! not a validation-rule cross-document data dependency. So on this
//! codebase, as it stands on `4d39071`, "folder siblings" name no
//! additional edge my affected-set closure would need to account for.
//! This corroborates T3's "no evidence" rather than T2's claim, but per
//! scope this file does not adjudicate that discrepancy — it only reports
//! what grepping the actual validation code shows.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use diwe::schema::validate_documents_against_file;
use liwe::graph::{Graph, GraphContext};
use liwe::model::config::{Format, MarkdownOptions};
use liwe::model::{Key, State};
use liwe::query::block::BlockPredicate;
use liwe::query::ViaWalk;
use liwe::transaction::{CommitError, Transaction, Write, WriteRejected};

use tempfile::TempDir;

/// What a rejected commit carries: one human-readable line per violation
/// found in the affected (or whole-store) set, `"<key>: <message>"`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ValidationRejected {
    violations: Vec<String>,
}

/// Test-builder's own (non-authoritative) `Transaction` backend: batches
/// writes in memory between `begin` and `commit`, and validates the
/// *final* state exactly once, at `commit`, over an index-backed affected
/// set — never per-write, and never (for the bounded rule forms) over the
/// whole store.
struct AffectedSetTransaction {
    root: PathBuf,
    schema_path: PathBuf,
    /// The `within` scope of the schema's `reach` rule, if it has one.
    /// Hardcoded per test rather than parsed out of the schema source: a
    /// real implementation would derive this from the compiled schema
    /// (`LinkRule::within` in `diwe::schema`, not reachable from outside
    /// that crate) instead of asking the caller to supply it.
    reach_scope: BlockPredicate,
    pending: Vec<Write>,
    failed: bool,
}

impl AffectedSetTransaction {
    fn new(root: PathBuf, schema_path: PathBuf, reach_scope: BlockPredicate) -> Self {
        Self {
            root,
            schema_path,
            reach_scope,
            pending: Vec::new(),
            failed: false,
        }
    }

    fn on_disk_state(&self) -> State {
        let mut state = HashMap::new();
        for (key, path) in diwe::fs::walk_md_paths(&self.root, Format::Markdown) {
            if let Ok(content) = fs::read_to_string(&path) {
                state.insert(key, content);
            }
        }
        state
    }

    /// This transaction's pending writes applied on top of `base` — the
    /// prospective full state `commit()` validates before anything reaches
    /// the filesystem.
    fn prospective_state(&self, base: &State) -> State {
        let mut state = base.clone();
        for write in &self.pending {
            match write {
                Write::Put(key, content) => {
                    state.insert(key.as_str().to_string(), content.clone());
                }
                Write::Remove(key) => {
                    state.remove(key.as_str());
                }
            }
        }
        state
    }

    fn touched_keys(&self) -> HashSet<Key> {
        self.pending
            .iter()
            .map(|write| match write {
                Write::Put(key, _) => key.clone(),
                Write::Remove(key) => key.clone(),
            })
            .collect()
    }

    /// The index-backed affected set: touched keys, plus their one-hop
    /// inbound referrers (`RefIndex`, via `Graph::get_reference_edges_to`),
    /// plus whatever transitively reaches a touched key through the
    /// schema's `reach`-rule scope (`ViaWalk::inbound`). Bounded rule forms
    /// only — see module docs for why `target`/`some`/`asserts`/
    /// `invariants` are routed elsewhere instead of relying on this.
    fn affected_set(&self, graph: &Graph) -> HashSet<Key> {
        let touched = self.touched_keys();
        let mut affected: HashSet<Key> = touched.clone();

        // RefIndex-backed one-hop closure: who links directly to a
        // touched document?
        for key in &touched {
            for node_id in graph.get_reference_edges_to(key) {
                affected.insert(graph.key_of(node_id));
            }
        }

        // ViaWalk::inbound-backed transitive closure: who reaches a
        // touched document through the `reach` rule's scoped links?
        let via = ViaWalk::new(graph, &self.reach_scope);
        for key in &touched {
            for (referrer, _distance) in via.inbound(key, u32::MAX) {
                affected.insert(referrer);
            }
        }

        affected
    }
}

impl Transaction for AffectedSetTransaction {
    type Error = ValidationRejected;

    fn begin(&mut self) -> Result<(), Self::Error> {
        self.pending.clear();
        self.failed = false;
        Ok(())
    }

    fn write(&mut self, write: Write) -> Result<(), WriteRejected<Self::Error>> {
        self.pending.push(write);
        Ok(())
    }

    fn commit(&mut self) -> Result<(), CommitError<Self::Error>> {
        if self.failed {
            return Err(CommitError::Failed);
        }

        let base = self.on_disk_state();
        let prospective = self.prospective_state(&base);
        let graph = Graph::import(&prospective, MarkdownOptions::default(), None);

        let schema_source = fs::read_to_string(&self.schema_path).unwrap_or_default();
        let unbounded = schema_declares_unbounded_rule(&schema_source);

        // The routing decision M1 calls for: bounded rule forms are
        // checked over the index-backed affected set; a schema declaring
        // any unbounded form widens that to every document in the
        // prospective store. A faithful implementation would route the
        // unbounded case to a separate install-time pass instead of
        // recomputing it inline on every commit; this harness does both
        // paths inside `commit()` for simplicity — see module docs.
        let validation_keys: Vec<Key> = if unbounded {
            graph.keys()
        } else {
            self.affected_set(&graph).into_iter().collect()
        };

        let run = validate_documents_against_file(&graph, &validation_keys, &self.schema_path)
            .expect("test fixture schemas compile");

        if !run.reports.is_empty() {
            let violations = run
                .reports
                .iter()
                .flat_map(|report| {
                    report
                        .violations
                        .iter()
                        .map(move |violation| format!("{}: {}", report.key, violation.message))
                })
                .collect();
            return Err(CommitError::Other(ValidationRejected { violations }));
        }

        for write in self.pending.drain(..) {
            match write {
                Write::Put(key, content) => write_key(&self.root, key.as_str(), &content),
                Write::Remove(key) => remove_key(&self.root, key.as_str()),
            }
        }
        Ok(())
    }

    fn abort(&mut self) -> Result<(), Self::Error> {
        self.pending.clear();
        self.failed = false;
        Ok(())
    }
}

/// My own, deliberately narrow classifier for whether a schema source
/// declares a rule form M1 calls unrestricted/whole-store: a top-level
/// `asserts` list, or a `links[].target` / `links[].some` whose value does
/// not anchor entirely on `$this` (the `$this`-anchored variants —
/// `target_this`/`some_this` in `diwe::schema::LinkRule` — are restricted
/// to the document's own direct link targets, so they stay index-bound).
/// This is my own reimplementation, not a call into `diwe::schema`'s
/// private `contains_this`/`LinkRule` — those aren't visible outside that
/// crate.
fn schema_declares_unbounded_rule(source: &str) -> bool {
    let Ok(value) = serde_yaml::from_str::<serde_yaml::Value>(source) else {
        return false;
    };
    let Some(mapping) = value.as_mapping() else {
        return false;
    };
    if mapping.contains_key(serde_yaml::Value::String("asserts".to_string())) {
        return true;
    }
    if let Some(serde_yaml::Value::Sequence(links)) =
        mapping.get(serde_yaml::Value::String("links".to_string()))
    {
        for entry in links {
            let Some(link_map) = entry.as_mapping() else {
                continue;
            };
            for keyword in ["target", "some"] {
                if let Some(value) = link_map.get(serde_yaml::Value::String(keyword.to_string()))
                {
                    if !anchors_entirely_on_this(value) {
                        return true;
                    }
                }
            }
        }
    }
    false
}

fn anchors_entirely_on_this(value: &serde_yaml::Value) -> bool {
    match value {
        serde_yaml::Value::String(s) => s == "$this" || s.starts_with("$this."),
        serde_yaml::Value::Sequence(items) => items.iter().all(anchors_entirely_on_this),
        serde_yaml::Value::Mapping(map) => map
            .iter()
            .all(|(k, v)| anchors_entirely_on_this(k) && anchors_entirely_on_this(v)),
        _ => false,
    }
}

fn write_key(root: &Path, key: &str, content: &str) {
    let path = root.join(format!("{key}.md"));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent dirs");
    }
    fs::write(path, content).expect("write file");
}

fn remove_key(root: &Path, key: &str) {
    let path = root.join(format!("{key}.md"));
    let _ = fs::remove_file(path);
}

fn write_schema(dir: &Path, name: &str, source: &str) -> PathBuf {
    let schemas = dir.join(".iwe").join("schemas");
    fs::create_dir_all(&schemas).expect("create schema dir");
    let path = schemas.join(format!("{name}.yaml"));
    fs::write(&path, source).expect("write schema");
    path
}

fn seed(root: &Path, key: &str, content: &str) {
    write_key(root, key, content);
}

fn on_disk(root: &Path, key: &str) -> Option<String> {
    fs::read_to_string(root.join(format!("{key}.md"))).ok()
}

// ---------------------------------------------------------------------
// Test A — a 2-write transaction that passes through an invalid
// intermediate state, and commits only because the *final* state is
// valid; the same first write alone, as a standalone transaction, is
// rejected.
// ---------------------------------------------------------------------

const MIN_ONE_REF_SCHEMA: &str = "\
links:
  - within: Refs
    min: 1
    description: every note needs at least one Refs link
";

#[test]
fn test_a_multi_write_transaction_commits_only_the_final_valid_state() {
    let temp = TempDir::new().unwrap();
    let schema_path = write_schema(temp.path(), "note", MIN_ONE_REF_SCHEMA);
    seed(temp.path(), "notes/b", "# B\n");

    let zero_refs = "# A\n\n## Refs\n";
    let one_ref = "# A\n\n## Refs\n\n- [b](notes/b)\n";

    // Multi-write transaction: write 1 alone would violate min:1 (zero
    // links in Refs); write 2 (same key, still inside one transaction)
    // adds the link. Only the state after both writes is validated.
    let mut tx = AffectedSetTransaction::new(
        temp.path().to_path_buf(),
        schema_path.clone(),
        BlockPredicate::empty(),
    );
    tx.begin().unwrap();
    tx.write(Write::Put(Key::name("notes/a"), zero_refs.to_string()))
        .unwrap();
    tx.write(Write::Put(Key::name("notes/a"), one_ref.to_string()))
        .unwrap();
    let result = tx.commit();

    assert!(
        result.is_ok(),
        "2-write transaction whose final state is valid must commit: {result:?}"
    );
    assert_eq!(
        on_disk(temp.path(), "notes/a").as_deref(),
        Some(one_ref),
        "committed content must be the final write, not the intermediate one"
    );

    // Empirical difference: the exact same first write, alone, as a
    // standalone single-write transaction, is rejected.
    let mut standalone = AffectedSetTransaction::new(
        temp.path().to_path_buf(),
        schema_path,
        BlockPredicate::empty(),
    );
    standalone.begin().unwrap();
    standalone
        .write(Write::Put(Key::name("notes/a"), zero_refs.to_string()))
        .unwrap();
    let standalone_result = standalone.commit();

    assert!(
        matches!(standalone_result, Err(CommitError::Other(_))),
        "the same first write alone must be rejected at commit: {standalone_result:?}"
    );
    if let Err(CommitError::Other(rejected)) = &standalone_result {
        assert!(
            rejected.violations.iter().any(|v| v.contains("fewer than the minimum")),
            "rejection should name the min:1 violation, got {:?}",
            rejected.violations
        );
    }
    // And that standalone commit did not touch the file the successful
    // transaction above already wrote.
    assert_eq!(on_disk(temp.path(), "notes/a").as_deref(), Some(one_ref));
}

// ---------------------------------------------------------------------
// Test B — a transaction whose final state is invalid is rejected at
// commit, and none of its writes land on the filesystem.
// ---------------------------------------------------------------------

#[test]
fn test_b_invalid_final_state_is_rejected_and_nothing_is_written() {
    let temp = TempDir::new().unwrap();
    let schema_path = write_schema(temp.path(), "note", MIN_ONE_REF_SCHEMA);

    // Neither "notes/a" nor "notes/c" exist on disk before the
    // transaction begins.
    assert!(on_disk(temp.path(), "notes/a").is_none());
    assert!(on_disk(temp.path(), "notes/c").is_none());

    let mut tx = AffectedSetTransaction::new(
        temp.path().to_path_buf(),
        schema_path,
        BlockPredicate::empty(),
    );
    tx.begin().unwrap();
    // One write that would satisfy the rule on its own...
    tx.write(Write::Put(
        Key::name("notes/a"),
        "# A\n\n## Refs\n\n- [c](notes/c)\n".to_string(),
    ))
    .unwrap();
    // ...and a second write, to a different key, that leaves the final
    // state invalid (zero Refs links).
    tx.write(Write::Put(Key::name("notes/c"), "# C\n\n## Refs\n".to_string()))
        .unwrap();

    let result = tx.commit();
    assert!(
        matches!(result, Err(CommitError::Other(_))),
        "a transaction whose final state is invalid must be rejected: {result:?}"
    );

    // Neither write landed — not "notes/a" (which alone would have been
    // fine) and not "notes/c" (which is what actually violates the rule).
    assert!(
        on_disk(temp.path(), "notes/a").is_none(),
        "no write from a rejected commit should reach the filesystem"
    );
    assert!(
        on_disk(temp.path(), "notes/c").is_none(),
        "no write from a rejected commit should reach the filesystem"
    );
}

// ---------------------------------------------------------------------
// Closure demonstration — RefIndex one-hop: an untouched document that
// directly links to a touched one is pulled into the affected set, and a
// touched-keys-only validation (no closure) would have missed its
// violation.
// ---------------------------------------------------------------------

const REFERRER_MUST_LINK_TO_PUBLISHED_SCHEMA: &str = "\
links:
  - when:
      kind: referrer
    within: Refs
    min: 1
    max: 1
    target:
      status: published
    description: every note's Refs link must point at a published document
";

#[test]
fn ref_index_closure_catches_violation_in_untouched_referrer() {
    let temp = TempDir::new().unwrap();
    let schema_path =
        write_schema(temp.path(), "note", REFERRER_MUST_LINK_TO_PUBLISHED_SCHEMA);

    // "notes/a" links to "notes/b" in Refs; "notes/b" starts out
    // published, so "notes/a" is valid and untouched by the transaction
    // below.
    seed(
        temp.path(),
        "notes/a",
        "---\nkind: referrer\n---\n# A\n\n## Refs\n\n- [b](b)\n",
    );
    seed(
        temp.path(),
        "notes/b",
        "---\nkind: target\nstatus: published\n---\n# B\n",
    );

    // This schema uses the non-`$this` `target` filter, which M1 and this
    // file both classify as unrestricted/whole-store — so my classifier
    // routes this commit to whole-store validation regardless of the
    // affected set, and the closure question is moot for it. To isolate
    // the RefIndex-closure mechanism itself (not the whole-store
    // fallback), this test compares the closure directly against a
    // touched-keys-only baseline instead of going through `commit()`.
    let prospective_content = "---\nkind: target\nstatus: draft\n---\n# B\n";
    let mut state: State = HashMap::new();
    state.insert("notes/a".to_string(), fs::read_to_string(temp.path().join("notes/a.md")).unwrap());
    state.insert("notes/b".to_string(), prospective_content.to_string());
    let graph = Graph::import(&state, MarkdownOptions::default(), None);

    let touched_only = vec![Key::name("notes/b")];
    let touched_only_run =
        validate_documents_against_file(&graph, &touched_only, &schema_path).unwrap();
    assert!(
        touched_only_run.reports.is_empty(),
        "touched-keys-only validation has no rule bound to notes/b itself, so it \
         wrongly finds nothing wrong: {:?}",
        touched_only_run.reports
    );

    // Now compute the RefIndex-backed closure by hand: who links directly
    // to the touched key ("notes/b")?
    let mut closure: HashSet<Key> = touched_only.iter().cloned().collect();
    for key in &touched_only {
        for node_id in graph.get_reference_edges_to(key) {
            closure.insert((&graph).key_of(node_id));
        }
    }
    assert!(
        closure.contains(&Key::name("notes/a")),
        "RefIndex-backed closure must find notes/a as a referrer of notes/b"
    );

    let closure_keys: Vec<Key> = closure.into_iter().collect();
    let closure_run =
        validate_documents_against_file(&graph, &closure_keys, &schema_path).unwrap();
    assert!(
        !closure_run.reports.is_empty(),
        "closure-based validation must catch notes/a's now-broken target filter"
    );
    assert!(closure_run
        .reports
        .iter()
        .any(|report| report.key == Key::name("notes/a")));
}

// ---------------------------------------------------------------------
// Closure demonstration — ViaWalk::inbound (`reach`): an untouched
// document whose validity depends on a `reach` chain running *through* a
// touched (but itself rule-exempt) document is pulled into the affected
// set; a touched-keys-only validation would have missed it.
// ---------------------------------------------------------------------

const LEAVES_MUST_REACH_ROOT_SCHEMA: &str = "\
links:
  - when:
      kind: leaf
    within: Is a
    reach: root
    description: every leaf must reach root via Is a
";

#[test]
fn via_walk_inbound_closure_catches_broken_reach_in_untouched_upstream_document() {
    let temp = TempDir::new().unwrap();
    let schema_path = write_schema(temp.path(), "chain", LEAVES_MUST_REACH_ROOT_SCHEMA);

    // root <- hub <- leaf, all via "Is a". Only "leaf" is subject to the
    // reach:root rule (the `when` filter excludes root and hub, both
    // kind != leaf). Initially valid.
    seed(temp.path(), "root", "---\nkind: root\n---\n# Root\n");
    seed(
        temp.path(),
        "hub",
        "---\nkind: hub\n---\n# Hub\n\n## Is a\n\n- [Root](root)\n",
    );
    seed(
        temp.path(),
        "leaf",
        "---\nkind: leaf\n---\n# Leaf\n\n## Is a\n\n- [Hub](hub)\n",
    );

    // Touch only "hub": redirect it away from "root". "hub" itself is not
    // subject to the reach rule (kind != leaf), so a touched-keys-only
    // validation of {hub} would find nothing wrong — even though "leaf"
    // (untouched) no longer reaches root.
    let mut state: State = HashMap::new();
    for key in ["root", "hub", "leaf"] {
        state.insert(
            key.to_string(),
            fs::read_to_string(temp.path().join(format!("{key}.md"))).unwrap(),
        );
    }
    state.insert(
        "hub".to_string(),
        "---\nkind: hub\n---\n# Hub\n\n## Is a\n\n- [Decoy](decoy)\n".to_string(),
    );
    let graph = Graph::import(&state, MarkdownOptions::default(), None);

    let touched_only = vec![Key::name("hub")];
    let touched_only_run =
        validate_documents_against_file(&graph, &touched_only, &schema_path).unwrap();
    assert!(
        touched_only_run.reports.is_empty(),
        "touched-keys-only validation wrongly finds nothing wrong, since hub itself \
         is exempt from the reach rule: {:?}",
        touched_only_run.reports
    );

    // ViaWalk::inbound-backed closure: who reaches "hub" via "Is a"?
    let reach_scope = BlockPredicate::empty().within_section("Is a");
    let via = ViaWalk::new(&graph, &reach_scope);
    let mut closure: HashSet<Key> = touched_only.iter().cloned().collect();
    for key in &touched_only {
        for (referrer, _distance) in via.inbound(key, u32::MAX) {
            closure.insert(referrer);
        }
    }
    assert!(
        closure.contains(&Key::name("leaf")),
        "ViaWalk::inbound closure must find leaf as an upstream document routed through hub"
    );

    let closure_keys: Vec<Key> = closure.into_iter().collect();
    let closure_run =
        validate_documents_against_file(&graph, &closure_keys, &schema_path).unwrap();
    assert!(
        !closure_run.reports.is_empty(),
        "closure-based validation must catch leaf's now-broken reach chain"
    );
    assert!(closure_run
        .reports
        .iter()
        .any(|report| report.key == Key::name("leaf")));
}

// ---------------------------------------------------------------------
// Unbounded-rule routing — a schema using `asserts` (per M1, unrestricted/
// whole-store) is routed to whole-store validation at commit, catching a
// violation on a document with no link-graph relationship at all to the
// touched key.
// ---------------------------------------------------------------------

const GLOBAL_UNIQUENESS_ASSERT_SCHEMA: &str = "\
asserts:
  - that:
      slug:
        $ne: taken
    description: slug must not equal the reserved literal 'taken'
";

#[test]
fn unbounded_rule_form_is_routed_to_whole_store_validation() {
    let temp = TempDir::new().unwrap();
    let schema_path = write_schema(temp.path(), "slugged", GLOBAL_UNIQUENESS_ASSERT_SCHEMA);

    assert!(
        schema_declares_unbounded_rule(GLOBAL_UNIQUENESS_ASSERT_SCHEMA),
        "an `asserts` schema must be classified as unbounded"
    );
    assert!(
        !schema_declares_unbounded_rule(MIN_ONE_REF_SCHEMA),
        "a plain min/max links schema must not be classified as unbounded"
    );
    assert!(
        !schema_declares_unbounded_rule(LEAVES_MUST_REACH_ROOT_SCHEMA),
        "a `reach` schema must not be classified as unbounded"
    );

    // "far/away" has no link-graph relationship to the touched key at
    // all — not a referrer, not on a reach chain — so no closure could
    // ever put it in the affected set. It already violates the assert.
    seed(
        temp.path(),
        "far/away",
        "---\nslug: taken\n---\n# Far away\n",
    );

    let mut tx = AffectedSetTransaction::new(
        temp.path().to_path_buf(),
        schema_path,
        BlockPredicate::empty(),
    );
    tx.begin().unwrap();
    // Touch a completely unrelated document; a closure-only validation
    // would never look at "far/away".
    tx.write(Write::Put(
        Key::name("unrelated"),
        "---\nslug: fine\n---\n# Unrelated\n".to_string(),
    ))
    .unwrap();
    let result = tx.commit();

    assert!(
        matches!(result, Err(CommitError::Other(_))),
        "whole-store routing must catch far/away's pre-existing violation \
         even though it has no graph relationship to the touched key: {result:?}"
    );
    if let Err(CommitError::Other(rejected)) = &result {
        assert!(
            rejected.violations.iter().any(|v| v.starts_with("far/away")),
            "rejection should name far/away, got {:?}",
            rejected.violations
        );
    }
    assert!(
        on_disk(temp.path(), "unrelated").is_none(),
        "the rejected commit must not have written anything"
    );
}
