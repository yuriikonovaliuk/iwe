//! Acceptance-criteria tests for
//! `efforts/multi-agent-orchestration/implementation/mind-write-separation/t3-write-scope-check`.
//!
//! Written from the task's persisted contract only (Shared surface:
//! `ValidationFailure::WriteScopeDenied(Vec<Key>)`, a sibling variant of
//! `Conflict`/`LockTimeout`/`LockStale`/`Violations`/`Config`, in
//! `crates/diwe/src/validating_transaction.rs`; consumes t1's already-landed
//! `diwe::config::write_permitted(deny, allow, key)`) -- without reading or
//! waiting on Developer's implementation of the same task, per the
//! Test-builder role's independence requirement. As of this file's writing,
//! `WriteScopeDenied` does not yet exist on this branch: these tests are
//! expected to fail to *compile* until Developer lands the variant and the
//! `commit_locked()` scope check that produces it -- that is the point of
//! writing them from the contract first.
//!
//! Per the contract's test assertion rule: every assertion on a denial goes
//! through `matches!(err, ValidationFailure::WriteScopeDenied(keys) if
//! keys.contains(&expected_key))`, never `Display` string equality (the one
//! exception is the dedicated Display test below, which checks *substring*
//! containment of the two literal fragments the contract pins, not equality).
//!
//! Kept as a separate integration-test file (rather than added inline to
//! `crates/diwe/src/validating_transaction.rs`) to avoid clobbering the same
//! file a Developer agent is concurrently editing for this task.

use std::fs;
use std::path::Path;

use diwe::config::{Configuration, TransactionOptions};
use diwe::validating_transaction::{ValidatingTransaction, ValidationFailure};
use liwe::model::config::Format;
use liwe::model::Key;
use liwe::transaction::{CommitError, Transaction, Write};

use std::path::PathBuf;
use tempfile::TempDir;

fn key(s: &str) -> Key {
    Key::from_stripped(s)
}

fn on_disk(root: &Path, key: &str) -> Option<String> {
    fs::read_to_string(root.join(format!("{key}.md"))).ok()
}

fn seed(root: &Path, key: &str, content: &str) {
    let path = root.join(format!("{key}.md"));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent dirs");
    }
    fs::write(path, content).expect("write fixture file");
}

/// A `Configuration` with no schemas bound (so `validate_final_state`
/// finds nothing to check regardless of scope) and the given
/// `[transactions] deny`/`allow` lists -- everything else at its default.
fn config_with_scope(deny: &[&str], allow: &[&str]) -> Configuration {
    Configuration {
        transactions: TransactionOptions {
            deny: deny.iter().map(|s| s.to_string()).collect(),
            allow: allow.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        },
        ..Default::default()
    }
}

fn transaction_for(temp: &TempDir, config: Configuration) -> ValidatingTransaction {
    ValidatingTransaction::new(
        temp.path().to_path_buf(),
        Format::Markdown,
        config,
        temp.path().join(".iwe").join("schemas"),
    )
}

/// Mirrors [`transaction_for`] but goes through the production
/// `for_config()` construction gate — the path the CLI's
/// `validating_backend` and iwec's MCP backend both take. Tests in this
/// section assert on the contract `for_config` actually serves, not on
/// `new`'s default-scope behavior.
fn backend_for_config(temp: &TempDir, config: Configuration) -> Option<ValidatingTransaction> {
    let base = temp.path().to_path_buf();
    ValidatingTransaction::for_config(&config, &base, &base)
}

/// The schemas dir a `for_config` construction points at — `.iwe/schemas`
/// under the project root. `for_config` does not require this to exist at
/// construction time, but the contract requires the store to have an
/// `.iwe/` so its `commit()` lock-root resolves and the test is exercising
/// the production path.
fn ensure_dot_iwe(temp: &TempDir) {
    let dot_iwe: PathBuf = temp.path().join(".iwe");
    std::fs::create_dir_all(dot_iwe.join("schemas")).expect("create .iwe/schemas");
}

// ---------------------------------------------------------------------
// Test (a): deny = ["mind/**"] (test fixture).
// ---------------------------------------------------------------------

#[test]
fn test_a_deny_pattern_rejects_commit_touching_matching_key_with_write_scope_denied() {
    let temp = TempDir::new().unwrap();
    let config = config_with_scope(&["mind/**"], &[]);
    let mut tx = transaction_for(&temp, config);

    tx.begin().unwrap();
    tx.write(Write::Put(key("mind/notes/a"), "# A\n".to_string()))
        .unwrap();
    let result = tx.commit();

    assert!(
        matches!(
            &result,
            Err(CommitError::Other(ValidationFailure::WriteScopeDenied(keys)))
                if keys.contains(&key("mind/notes/a"))
        ),
        "a commit touching a denied key must fail with WriteScopeDenied naming that key: \
         {result:?}"
    );
}

#[test]
fn test_a_denied_commit_leaves_no_change_on_disk_for_a_new_key() {
    let temp = TempDir::new().unwrap();
    let config = config_with_scope(&["mind/**"], &[]);
    let mut tx = transaction_for(&temp, config);

    assert!(on_disk(temp.path(), "mind/notes/a").is_none());

    tx.begin().unwrap();
    tx.write(Write::Put(key("mind/notes/a"), "# A\n".to_string()))
        .unwrap();
    let result = tx.commit();
    assert!(result.is_err());

    assert!(
        on_disk(temp.path(), "mind/notes/a").is_none(),
        "a re-read after the failed commit must show no change landed"
    );
}

#[test]
fn test_a_denied_commit_leaves_pre_existing_content_of_the_denied_key_untouched() {
    let temp = TempDir::new().unwrap();
    seed(temp.path(), "mind/notes/a", "# Original\n");

    let config = config_with_scope(&["mind/**"], &[]);
    let mut tx = transaction_for(&temp, config);

    tx.begin().unwrap();
    tx.write(Write::Put(
        key("mind/notes/a"),
        "# Overwritten\n".to_string(),
    ))
    .unwrap();
    let result = tx.commit();
    assert!(result.is_err());

    assert_eq!(
        on_disk(temp.path(), "mind/notes/a").as_deref(),
        Some("# Original\n"),
        "a re-read after the failed commit must show the pre-existing content, unchanged"
    );
}

#[test]
fn test_a_non_mind_key_still_succeeds_under_the_same_deny_config() {
    let temp = TempDir::new().unwrap();
    let config = config_with_scope(&["mind/**"], &[]);
    let mut tx = transaction_for(&temp, config);

    tx.begin().unwrap();
    tx.write(Write::Put(key("notes/a"), "# A\n".to_string()))
        .unwrap();
    let result = tx.commit();

    assert!(
        result.is_ok(),
        "a commit touching only a non-denied key must succeed under the same config: {result:?}"
    );
    assert_eq!(on_disk(temp.path(), "notes/a").as_deref(), Some("# A\n"));
}

// ---------------------------------------------------------------------
// Test (d): deny = [] / allow = [] (schema default) -- unrestricted, as
// before this change, including for mind/... keys specifically.
// ---------------------------------------------------------------------

#[test]
fn test_d_schema_default_config_has_empty_deny_and_allow() {
    let config = Configuration::default();
    assert!(config.transactions.deny.is_empty());
    assert!(config.transactions.allow.is_empty());
}

#[test]
fn test_d_default_config_still_permits_a_commit_to_a_mind_key() {
    let temp = TempDir::new().unwrap();
    let config = Configuration::default();
    let mut tx = transaction_for(&temp, config);

    tx.begin().unwrap();
    tx.write(Write::Put(key("mind/notes/a"), "# A\n".to_string()))
        .unwrap();
    let result = tx.commit();

    assert!(
        result.is_ok(),
        "the schema-default (unrestricted) config must not deny a mind/... write -- no \
         accidental restriction: {result:?}"
    );
    assert_eq!(on_disk(temp.path(), "mind/notes/a").as_deref(), Some("# A\n"));
}

// ---------------------------------------------------------------------
// Test (b), commit-level half: allow = ["mind/**"].
// ---------------------------------------------------------------------

#[test]
fn test_b_allow_pattern_permits_commit_touching_only_allow_listed_keys() {
    let temp = TempDir::new().unwrap();
    let config = config_with_scope(&[], &["mind/**"]);
    let mut tx = transaction_for(&temp, config);

    tx.begin().unwrap();
    tx.write(Write::Put(key("mind/a"), "# A\n".to_string()))
        .unwrap();
    tx.write(Write::Put(key("mind/nested/b"), "# B\n".to_string()))
        .unwrap();
    let result = tx.commit();

    assert!(
        result.is_ok(),
        "a commit touching only allow-listed keys must succeed: {result:?}"
    );
    assert_eq!(on_disk(temp.path(), "mind/a").as_deref(), Some("# A\n"));
    assert_eq!(
        on_disk(temp.path(), "mind/nested/b").as_deref(),
        Some("# B\n")
    );
}

#[test]
fn test_b_allow_pattern_denies_commit_touching_a_key_outside_the_allowlist_naming_it() {
    let temp = TempDir::new().unwrap();
    let config = config_with_scope(&[], &["mind/**"]);
    let mut tx = transaction_for(&temp, config);

    tx.begin().unwrap();
    tx.write(Write::Put(key("mind/a"), "# A\n".to_string()))
        .unwrap();
    tx.write(Write::Put(key("other/b"), "# B\n".to_string()))
        .unwrap();
    let result = tx.commit();

    assert!(
        matches!(
            &result,
            Err(CommitError::Other(ValidationFailure::WriteScopeDenied(keys)))
                if keys.contains(&key("other/b"))
        ),
        "a commit touching a key outside the allow-list must fail with WriteScopeDenied \
         naming that key: {result:?}"
    );

    // All-or-nothing: the allow-listed key in the SAME commit must not have
    // landed either, even though it would have been permitted on its own.
    assert!(
        on_disk(temp.path(), "mind/a").is_none(),
        "no partial application: the allow-listed key must not land when the same commit \
         also touches a denied key"
    );
    assert!(on_disk(temp.path(), "other/b").is_none());
}

// ---------------------------------------------------------------------
// Scope check fires before apply_pending(): a denied commit must fall back
// to pending state exactly like the other pre-apply failure modes
// (Conflict/Violations/LockTimeout/LockStale), none of which clear
// `pending`/`baseline` on failure (only `Transaction::commit`'s `Ok` arm
// does) -- so retrying the exact same commit, without a fresh `begin()`,
// must reproduce the identical denial rather than silently succeeding
// (which it would if apply_pending() had already run and partially/fully
// applied the writes) or panicking on already-consumed state.
// ---------------------------------------------------------------------

#[test]
fn scope_check_runs_before_apply_pending_leaving_transaction_retryable_after_denial() {
    let temp = TempDir::new().unwrap();
    let config = config_with_scope(&["mind/**"], &[]);
    let mut tx = transaction_for(&temp, config);

    tx.begin().unwrap();
    tx.write(Write::Put(key("mind/a"), "# A\n".to_string()))
        .unwrap();

    let first = tx.commit();
    assert!(
        matches!(
            &first,
            Err(CommitError::Other(ValidationFailure::WriteScopeDenied(keys)))
                if keys.contains(&key("mind/a"))
        ),
        "first commit attempt: {first:?}"
    );
    assert!(on_disk(temp.path(), "mind/a").is_none());

    // Retried with no fresh `begin()`/`write()`: the same pending write is
    // re-attempted and denied identically, proving nothing was applied and
    // nothing was cleared by the first (failed) attempt.
    let second = tx.commit();
    assert!(
        matches!(
            &second,
            Err(CommitError::Other(ValidationFailure::WriteScopeDenied(keys)))
                if keys.contains(&key("mind/a"))
        ),
        "retried commit attempt on the still-pending transaction: {second:?}"
    );
    assert!(on_disk(temp.path(), "mind/a").is_none());
}

// ---------------------------------------------------------------------
// Display: substring containment only, per the contract's assertion rule
// -- never full string equality.
// ---------------------------------------------------------------------

#[test]
fn write_scope_denied_display_names_the_key_and_says_rejected() {
    let failure = ValidationFailure::WriteScopeDenied(vec![key("mind/notes/a")]);
    let message = failure.to_string();

    assert!(
        message.contains("write to 'mind/notes/a'"),
        "Display message must contain \"write to '<key>'\": {message:?}"
    );
    assert!(
        message.contains("rejected"),
        "Display message must contain \"rejected\": {message:?}"
    );
}

// ---------------------------------------------------------------------
// t4-cli-parity: `ValidatingTransaction::for_config` construction gate.
//
// The pre-t4 defect was that `for_config` returned `None` whenever
// `transactions.validate == None`, even if `deny` or `allow` was
// non-empty — so a store configured for write-scope enforcement only
// (`deny = ["mind/**"]`, no `validate` key) bypassed the validating
// backend entirely and committed through `NoopTransaction`, with the
// scope check never running. `for_config` now constructs a backend
// whenever validate is non-None OR deny/allow is non-empty.
//
// When the only trigger is non-empty deny/allow, the constructed
// backend's internal `scope` is `ValidationScope::None` so the
// schema-validation cost at `commit()` is skipped entirely — the scope
// check is what gates the commit, and no schema work is paid.
//
// These tests cover the gate end-to-end: the construction, the scope
// check's two outcomes, and the skip of schema validation.
// ---------------------------------------------------------------------

#[test]
fn for_config_with_deny_only_config_constructs_a_backend_with_none_scope() {
    let temp = TempDir::new().unwrap();
    ensure_dot_iwe(&temp);

    // deny non-empty, validate left at its default None: a backend must
    // still be built — otherwise the scope check never runs and the
    // milestone's production default (deny-only `[transactions]` blocks)
    // silently no-ops.
    let config = config_with_scope(&["mind/**"], &[]);
    assert_eq!(
        config.transactions.validate,
        diwe::config::ValidationScope::None
    );
    assert!(!config.transactions.deny.is_empty());

    let backend = backend_for_config(&temp, config)
        .expect("for_config must build a backend for a deny-only config");

    // Constructed solely for write-scope enforcement: internal `scope`
    // is None so the schema-validation cost at commit is skipped.
    assert_eq!(
        backend.scope(),
        diwe::config::ValidationScope::None,
        "deny/allow-only construction must keep scope None so commit() skips validation"
    );
}

#[test]
fn for_config_with_default_unrestricted_config_still_returns_none() {
    let temp = TempDir::new().unwrap();
    ensure_dot_iwe(&temp);

    // No-op behavior: validate=None, deny=[], allow=[] — the pre-t4
    // "returns None" case is preserved exactly for the unrestricted
    // default, so a store that opted into nothing stays on
    // NoopTransaction and costs nothing.
    let config = config_with_scope(&[], &[]);
    assert_eq!(
        config.transactions.validate,
        diwe::config::ValidationScope::None
    );
    assert!(config.transactions.deny.is_empty());
    assert!(config.transactions.allow.is_empty());

    assert!(
        backend_for_config(&temp, config).is_none(),
        "unrestricted default config must continue to return None from for_config"
    );
}

#[test]
fn for_config_with_allow_only_config_constructs_a_backend_with_none_scope() {
    let temp = TempDir::new().unwrap();
    ensure_dot_iwe(&temp);

    let config = config_with_scope(&[], &["mind/**"]);
    let backend = backend_for_config(&temp, config)
        .expect("for_config must build a backend for an allow-only config");
    assert_eq!(backend.scope(), diwe::config::ValidationScope::None);
}

/// Symmetric to the deny-only enforcement test below: the allow-only
/// path also goes end-to-end through `for_config` -- a commit outside
/// the allow-list is refused by the scope check, a commit inside it
/// lands, and schema validation is genuinely skipped at `scope ==
/// None`. The construction-only test above
/// (`for_config_with_allow_only_config_constructs_a_backend_with_none_scope`)
/// checks the gate builds a backend and leaves `scope` at `None`; it
/// never exercises `commit()`, so it does not by itself confirm the
/// allow-list is actually enforced. This test closes that gap.
#[test]
fn for_config_allow_only_backend_enforces_scope_and_skips_schema_validation() {
    let temp = TempDir::new().unwrap();
    ensure_dot_iwe(&temp);

    // Same "would-fail-schema" fixture as the deny-only test: a note
    // with no links would trigger `Violations` at scope `AffectedSet`.
    // Used here to prove validation is skipped at `scope == None`.
    fs::write(
        temp.path().join(".iwe/schemas/note.yaml"),
        "links:\n  - min: 1\n",
    )
    .expect("write schema");

    let config = config_with_scope(&[], &["mind/**"]);
    let mut backend = backend_for_config(&temp, config).expect("for_config");

    // Denied commit: key outside the allow-list, content that would
    // also violate the schema -- the scope check must fire first with
    // WriteScopeDenied, not Violations.
    backend.begin().unwrap();
    backend
        .write(Write::Put(
            key("other/b"),
            "# B\n\nno links here\n".to_string(),
        ))
        .unwrap();
    let denied = backend.commit();
    assert!(
        matches!(
            &denied,
            Err(CommitError::Other(ValidationFailure::WriteScopeDenied(keys)))
                if keys.contains(&key("other/b"))
        ),
        "commit outside the allow-list must be refused with WriteScopeDenied naming the key: \
         {denied:?}"
    );
    assert!(
        !matches!(
            &denied,
            Err(CommitError::Other(ValidationFailure::Violations(_)))
        ),
        "the schema-validation cost must be skipped at scope None -- the refusal must come \
         from the scope check, not from a schema Violations run"
    );
    assert!(on_disk(temp.path(), "other/b").is_none());

    // Permitted commit: key inside the allow-list, content that WOULD
    // violate the schema -- schema validation is skipped at scope
    // None, so the commit must land, never be refused with Violations.
    let mut backend2 = backend_for_config(&temp, config_with_scope(&[], &["mind/**"]))
        .expect("for_config for second commit");
    backend2.begin().unwrap();
    backend2
        .write(Write::Put(
            key("mind/a"),
            "# A\n\nno links here\n".to_string(),
        ))
        .unwrap();
    let permitted = backend2.commit();
    assert!(
        matches!(&permitted, Ok(())),
        "a permitted commit must succeed at scope None -- schema validation is skipped: \
         {permitted:?}"
    );
    assert!(
        !matches!(
            &permitted,
            Err(CommitError::Other(ValidationFailure::Violations(_)))
        ),
        "permitted commit must not be refused with Violations at scope None: {permitted:?}"
    );
    assert_eq!(
        on_disk(temp.path(), "mind/a").as_deref(),
        Some("# A\n\nno links here\n"),
        "permitted commit content must land on disk -- schema validation cost was skipped"
    );
}

/// The deny/allow-only path goes end-to-end: a denied commit is
/// refused by the scope check (the reason for the construction gate),
/// a permitted commit lands — and the schema-validation cost is
/// genuinely skipped at `scope == None`, never the refusal reason.
#[test]
fn for_config_deny_only_backend_enforces_scope_and_skips_schema_validation() {
    let temp = TempDir::new().unwrap();
    ensure_dot_iwe(&temp);

    // Schema that would refuse a note with no links: a write to a
    // permitted key whose content lacks the required link would
    // trigger `Violations` at scope `AffectedSet`. We use it here to
    // prove the validation is skipped at `scope == None` — the
    // permitted commit must land, not be refused with Violations,
    // even though the content would otherwise violate this schema.
    fs::write(
        temp.path().join(".iwe/schemas/note.yaml"),
        "links:\n  - min: 1\n",
    )
    .expect("write schema");

    let config = config_with_scope(&["mind/**"], &[]);
    let mut backend = backend_for_config(&temp, config).expect("for_config");

    // Denied commit, content that would also violate the schema — the
    // scope check must fire first and refuse with WriteScopeDenied,
    // not Violations.
    backend.begin().unwrap();
    backend
        .write(Write::Put(
            key("mind/a"),
            "# A\n\nno links here\n".to_string(),
        ))
        .unwrap();
    let denied = backend.commit();
    assert!(
        matches!(
            &denied,
            Err(CommitError::Other(ValidationFailure::WriteScopeDenied(keys)))
                if keys.contains(&key("mind/a"))
        ),
        "denied commit must be refused with WriteScopeDenied naming the key: {denied:?}"
    );

    // The scope check fired, not schema validation: the refused
    // reason must be WriteScopeDenied, never Violations.
    assert!(
        !matches!(
            &denied,
            Err(CommitError::Other(ValidationFailure::Violations(_)))
        ),
        "the schema-validation cost must be skipped at scope None — the refusal must \
         come from the scope check, not from a schema Violations run"
    );

    // Permitted commit, content that WOULD violate the schema — the
    // schema validation is skipped at scope None, so the commit must
    // land, never be refused with Violations. This is the smoking-gun
    // for the gate: had validation run (scope = AffectedSet), this
    // would have failed with Violations; at scope = None it lands.
    let mut backend2 = backend_for_config(&temp, config_with_scope(&["mind/**"], &[]))
        .expect("for_config for second commit");
    backend2.begin().unwrap();
    backend2
        .write(Write::Put(
            key("notes/a"),
            "# A\n\nno links here\n".to_string(),
        ))
        .unwrap();
    let permitted = backend2.commit();
    assert!(
        matches!(&permitted, Ok(())),
        "a permitted commit must succeed at scope None — schema validation is skipped: \
         {permitted:?}"
    );
    assert!(
        !matches!(
            &permitted,
            Err(CommitError::Other(ValidationFailure::Violations(_)))
        ),
        "permitted commit must not be refused with Violations at scope None: {permitted:?}"
    );
    assert_eq!(
        on_disk(temp.path(), "notes/a").as_deref(),
        Some("# A\n\nno links here\n"),
        "permitted commit content must land on disk — schema validation cost was skipped"
    );
}
