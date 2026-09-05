//! Acceptance-criteria tests for
//! `efforts/multi-agent-orchestration/implementation/mind-write-separation/t1-scope-schema`.
//!
//! Written from the task's persisted contract only (Shared surface:
//! `TransactionOptions.deny: Vec<String>`, `TransactionOptions.allow:
//! Vec<String>`, both `#[serde(default)]`, and `pub fn write_permitted(deny:
//! &[String], allow: &[String], key: &Key) -> bool`, all in
//! `crates/diwe/src/config.rs`) -- without reading or waiting on Developer's
//! implementation of the same task, per the Test-builder role's independence
//! requirement.
//!
//! Kept as a separate integration-test file (rather than added inline to
//! `crates/diwe/src/config.rs`) to avoid clobbering the same file a
//! Developer agent is concurrently editing for this task.

use diwe::config::{write_permitted, Configuration, TransactionOptions};
use liwe::model::Key;

fn key(s: &str) -> Key {
    Key::from_stripped(s)
}

// ---------------------------------------------------------------------
// Deserialization: existing config fixtures with no deny/allow keys still
// deserialize correctly, both defaulting to empty vecs.
// ---------------------------------------------------------------------

#[test]
fn transaction_options_default_to_empty_deny_and_allow() {
    let options = TransactionOptions::default();
    assert_eq!(options.deny, Vec::<String>::new());
    assert_eq!(options.allow, Vec::<String>::new());
}

#[test]
fn existing_config_fixture_without_deny_allow_keys_still_parses() {
    // No [transactions] section at all -- the pre-existing shape.
    let source = "version = 3\n";
    let parsed: Configuration = toml::from_str(source).expect("must still parse");
    assert_eq!(parsed.transactions.deny, Vec::<String>::new());
    assert_eq!(parsed.transactions.allow, Vec::<String>::new());
}

#[test]
fn existing_transactions_section_without_deny_allow_keys_still_parses() {
    // A [transactions] section that only sets the pre-existing `validate`
    // field must still parse, with deny/allow defaulting to empty.
    let source = "version = 3\n\n[transactions]\nvalidate = \"full\"\n";
    let parsed: Configuration = toml::from_str(source).expect("must still parse");
    assert_eq!(parsed.transactions.deny, Vec::<String>::new());
    assert_eq!(parsed.transactions.allow, Vec::<String>::new());
}

#[test]
fn transactions_section_can_set_deny_and_allow() {
    let source = indoc::indoc! {r#"
        version = 3

        [transactions]
        deny = ["secrets/**"]
        allow = ["mind/**"]
    "#};
    let parsed: Configuration = toml::from_str(source).expect("must parse");
    assert_eq!(parsed.transactions.deny, vec!["secrets/**".to_string()]);
    assert_eq!(parsed.transactions.allow, vec!["mind/**".to_string()]);
}

// ---------------------------------------------------------------------
// write_permitted: both empty -> true (unrestricted).
// ---------------------------------------------------------------------

#[test]
fn both_empty_is_unrestricted() {
    let deny: Vec<String> = vec![];
    let allow: Vec<String> = vec![];
    assert!(write_permitted(&deny, &allow, &key("anything/at/all")));
}

// ---------------------------------------------------------------------
// write_permitted: deny-only.
// ---------------------------------------------------------------------

#[test]
fn deny_only_matching_key_is_denied() {
    let deny = vec!["mind/**".to_string()];
    let allow: Vec<String> = vec![];
    assert!(!write_permitted(&deny, &allow, &key("mind/foo/bar")));
}

#[test]
fn deny_only_non_matching_key_is_permitted() {
    let deny = vec!["mind/**".to_string()];
    let allow: Vec<String> = vec![];
    assert!(write_permitted(&deny, &allow, &key("other/foo")));
}

// ---------------------------------------------------------------------
// write_permitted: allow-only.
// ---------------------------------------------------------------------

#[test]
fn allow_only_matching_key_is_permitted() {
    let deny: Vec<String> = vec![];
    let allow = vec!["mind/**".to_string()];
    assert!(write_permitted(&deny, &allow, &key("mind/foo/bar")));
}

#[test]
fn allow_only_non_matching_key_is_denied() {
    let deny: Vec<String> = vec![];
    let allow = vec!["mind/**".to_string()];
    assert!(!write_permitted(&deny, &allow, &key("other/foo")));
}

// ---------------------------------------------------------------------
// Pattern semantics must match SchemaBinding::r#match/Patterns' existing
// glob syntax (see crates/diwe/src/schema.rs's `compile_patterns` /
// `SchemaBindings`, e.g. `single_glob_matches_by_prefix`): `mind/**`
// matches everything under `mind/`, and does not match a sibling prefix.
// ---------------------------------------------------------------------

#[test]
fn double_star_pattern_matches_nested_keys_under_prefix_only() {
    let allow = vec!["mind/**".to_string()];
    let deny: Vec<String> = vec![];

    assert!(
        write_permitted(&deny, &allow, &key("mind/foo/bar")),
        "mind/** must match a nested key under mind/"
    );
    assert!(
        !write_permitted(&deny, &allow, &key("other/foo")),
        "mind/** must not match a key outside mind/"
    );
}

// ---------------------------------------------------------------------
// allow takes precedence over deny when both are non-empty (per contract:
// "allow non-empty -> true iff key matches >=1 allow pattern; else
// deny non-empty -> ..."). Not separately enumerated by the contract's
// five required cases, but pinned by the Shared surface's precedence
// rule, so it is exercised here too.
// ---------------------------------------------------------------------

#[test]
fn allow_takes_precedence_over_deny_when_both_set() {
    let deny = vec!["mind/**".to_string()];
    let allow = vec!["mind/**".to_string()];
    assert!(write_permitted(&deny, &allow, &key("mind/foo")));

    let deny = vec!["mind/**".to_string()];
    let allow = vec!["other/**".to_string()];
    assert!(!write_permitted(&deny, &allow, &key("mind/foo")));
}
