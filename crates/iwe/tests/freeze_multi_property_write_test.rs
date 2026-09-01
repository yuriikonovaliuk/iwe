// T21 — the second axis T13's enforcement-mode matrix was missing.
//
// `efforts/knowledge-compositor/m2/design-freeze-semantics` (the ruling this
// file is written against, in full):
//
// > R15 makes freeze dominate: a frozen document rejects every write,
// > including to properties the schema marks mutable. That guarantee has a
// > demonstrated single-call bypass.
// >
// > The bypass. The write-permission predicate is outgoing-content-shaped.
// > For a create or an update it validates the content about to be written,
// > with no comparison against the document as it stands. So a single call
// > that sets `freeze` to false and changes another field is evaluated
// > against the resulting content, which is unfrozen — and both changes
// > land, exit zero.
// >
// > The rule. A write to a document that is frozen as it stands before the
// > write is rejected, unless the write's sole effect is lifting freeze.
// >
// > Sole effect is defined by comparison rather than by intent: the
// > resulting document must be identical to the prior one in every
// > frontmatter property and in the body, except that it is no longer
// > frozen. Anything else carried in the same write is a rejection.
// >
// > Effective state, not literal property. Lifting freeze means the
// > document passes from effectively frozen to effectively unfrozen,
// > whether by setting the marker false or by removing it outright.
// >
// > Freezing is not restricted. A write that sets freeze on an unfrozen
// > document may carry other changes, because the document is not frozen
// > when the predicate runs and no guarantee is engaged.
//
// T13's matrix (`enforcement_mode_matrix_test.rs`) exercised *which*
// property a write touched (body vs. one frontmatter field) but never *how
// many* properties a single call touched — exactly the axis the bypass
// lives on. This file adds that axis: every case below is a single write
// call, and the thing under test is whether that one call's *other* effects
// ride through alongside a freeze-state change.
//
// # What was actually observed against this worktree's pre-fix code
// (`crates/diwe/src/permissions.rs` at this branch's parent,
// `knowledge-compositor-m2-t13-dev`)
//
// `check_write_permission_for_content[_in]` builds its `Document` straight
// from the *outgoing* content string and asks only "is `freeze` true in
// this content" — never reading the document as it exists on disk before
// the write. That single fact predicts every result below, including one
// beyond the ruling's own headline example:
//
//   - Case 1/2 (bypass, freeze-lift + another property, one call):
//     outgoing content's `freeze` is false/absent, so the check passes —
//     REJECT expected, ALLOWED observed. Bug confirmed, as documented.
//   - Case 3/4 (solitary unfreeze): outgoing content's `freeze` is
//     false/absent, check passes — matches the desired behavior already,
//     coincidentally, since there is nothing here for "outgoing-shaped" to
//     get wrong. ALLOW expected, ALLOWED observed.
//   - Case 5 (freezing an unfrozen document + another property, one call):
//     outgoing content's `freeze` is true, so the check REJECTS — even
//     though the document was not frozen beforehand and the ruling's
//     "freezing is not restricted" clause says this must succeed. ALLOW
//     expected, REJECTED observed. This is the same root cause as the
//     headline bypass (the predicate cannot see prior state), showing up
//     as over-rejection instead of under-rejection depending on which
//     direction the freeze transition runs. Not previously named in the
//     ruling's own bypass narrative, but it is the same bug and the same
//     fix (compare against prior state) closes it too.
//   - Case 6 (two separate calls): each call is independently evaluated
//     against its own outgoing content, so this was never going to trip
//     the "outgoing-shaped" bug regardless of fix status. ALLOW expected,
//     ALLOWED observed both before and after.
//   - Case 7 (freeze-lift + body change, one call, via full-content
//     overwrite): same shape as case 1, just with the body standing in for
//     "another property." REJECT expected, ALLOWED observed.
//
// So this file's tests do not uniformly fail before the fix and pass after
// — cases 3/4/6 already pass today, because the current bug's failure mode
// happens to coincide with correct behavior there. Cases 1/2/5/7 are the
// ones that currently fail and are expected to start passing once the fix
// (compare outgoing content against the prior on-disk document, per the
// ruling's "the rule" text) lands. All are kept in one file/table because
// `m2/design-freeze-semantics`'s rule is a single comparison, and a matrix
// that only recorded the cases that currently fail would hide exactly the
// kind of asymmetry (case 5) this task's brief warns against assuming away.
//
// # Enforcement mode (C13 taxonomy, same three modes T13's matrix uses)
//
// Every case below reaches `diwe::permissions::check_write_permission_for_
// content` via the same WP-04/WP-05 call sites T13 already source-traced as
// identical between CLI ordinary and CLI `--strict` (`update_body` /
// `write_changed_documents` in `crates/iwe/src/main.rs`). No new call site
// is introduced here, so the code-path-identity argument T13 already made
// applies unchanged: once fixed, every case is expected at mode one
// (rejected — or allowed, per the case — unconditionally, on every
// invocation, no flag) on both CLI ordinary and CLI `--strict`. Each test
// function name below is suffixed `_ordinary` or `_strict`; run both to
// treat "mode one, both paths" as observed rather than assumed. MCP
// coverage for the same cases lives in
// `crates/iwec/tests/freeze_multi_property_write_test.rs`.
//
// # Matrix shape (extend this table, don't restructure it, for M5's later
// rejection sets per the task brief)
//
// | case | shape                                              | expected  | CLI ordinary | CLI --strict | observed pre-fix |
// |------|-----------------------------------------------------|-----------|--------------|--------------|-------------------|
// | 1    | frozen, `freeze=false` + 1 other property, one call  | REJECT    | see below    | see below    | ALLOWED (bug)     |
// | 2    | frozen, `--unset freeze` + 1 other property, one call| REJECT    | see below    | see below    | ALLOWED (bug)     |
// | 3    | frozen, `freeze=false` alone, one call               | ALLOW     | see below    | see below    | ALLOWED           |
// | 4    | frozen, `--unset freeze` alone, one call             | ALLOW     | see below    | see below    | ALLOWED           |
// | 5    | unfrozen, `freeze=true` + 1 other property, one call | ALLOW     | see below    | see below    | REJECTED (bug)    |
// | 6    | frozen, unfreeze then separate write, two calls      | ALLOW     | see below    | see below    | ALLOWED           |
// | 7    | frozen, `freeze=false`/unset + body change, one call | REJECT    | see below    | see below    | ALLOWED (bug)     |
//
// Layer-free fixtures only: no `origin:`/`mint:`/package vocabulary.

use diwe::config::{Configuration, LibraryOptions, MarkdownOptions};
use diwe::permissions::FREEZE_FIELD;
use indoc::indoc;
use liwe::model::{parse_leading_frontmatter, split_raw_frontmatter};
use serde_yaml::{Mapping, Value};
use std::fs::{create_dir_all, read_to_string, write};
use std::path::Path;
use std::process::{Command, Output};
use tempfile::TempDir;

const FROZEN_DOC: &str = indoc! {"
    ---
    freeze: true
    status: draft
    ---

    # Frozen Document

    Original body.
"};

const UNFROZEN_DOC: &str = indoc! {"
    ---
    status: draft
    ---

    # Unfrozen Document

    Original body.
"};

// ---------------------------------------------------------------------
// Case 1 — the exact demonstrated bypass: freeze=false + another property,
// one call. MUST be rejected.
// ---------------------------------------------------------------------

#[test]
fn case1_freeze_false_plus_other_property_is_rejected_ordinary() {
    let temp = setup(FROZEN_DOC);
    let before = read_to_string(temp.path().join("doc.md")).unwrap();

    let output = run_iwe(
        temp.path(),
        &[
            "update",
            "-k",
            "doc",
            "--set",
            "freeze=false",
            "--set",
            "status=reviewed",
        ],
    );

    assert!(
        !output.status.success(),
        "a single write that lifts freeze AND changes another property must \
         be rejected wholesale, not merely have the freeze-lift honored: \
         stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_rejected_and_names_frozen(&output, "doc");
    assert_eq!(
        read_to_string(temp.path().join("doc.md")).unwrap(),
        before,
        "frozen document must be unchanged on disk — neither the freeze-lift \
         nor the bundled status change may land"
    );
}

#[test]
fn case1_freeze_false_plus_other_property_is_rejected_strict() {
    let temp = setup(FROZEN_DOC);
    let before = read_to_string(temp.path().join("doc.md")).unwrap();

    let output = run_iwe(
        temp.path(),
        &[
            "update",
            "-k",
            "doc",
            "--set",
            "freeze=false",
            "--set",
            "status=reviewed",
            "--strict",
            "--expect",
            "1",
        ],
    );

    assert!(!output.status.success());
    assert_rejected_and_names_frozen(&output, "doc");
    assert_eq!(read_to_string(temp.path().join("doc.md")).unwrap(), before);
}

// ---------------------------------------------------------------------
// Case 2 — the marker-removal variant of the same bypass: `--unset freeze`
// (not merely `freeze=false`) + another property, one call. MUST be
// rejected — "effective state, not literal property" per the ruling.
// ---------------------------------------------------------------------

#[test]
fn case2_freeze_unset_plus_other_property_is_rejected_ordinary() {
    let temp = setup(FROZEN_DOC);
    let before = read_to_string(temp.path().join("doc.md")).unwrap();

    let output = run_iwe(
        temp.path(),
        &[
            "update",
            "-k",
            "doc",
            "--unset",
            "freeze",
            "--set",
            "status=reviewed",
        ],
    );

    assert!(
        !output.status.success(),
        "removing the freeze key entirely is the same bypass shape as \
         setting it false; bundling another property change must still be \
         rejected: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_rejected_and_names_frozen(&output, "doc");
    assert_eq!(read_to_string(temp.path().join("doc.md")).unwrap(), before);
}

#[test]
fn case2_freeze_unset_plus_other_property_is_rejected_strict() {
    let temp = setup(FROZEN_DOC);
    let before = read_to_string(temp.path().join("doc.md")).unwrap();

    let output = run_iwe(
        temp.path(),
        &[
            "update",
            "-k",
            "doc",
            "--unset",
            "freeze",
            "--set",
            "status=reviewed",
            "--strict",
            "--expect",
            "1",
        ],
    );

    assert!(!output.status.success());
    assert_rejected_and_names_frozen(&output, "doc");
    assert_eq!(read_to_string(temp.path().join("doc.md")).unwrap(), before);
}

// ---------------------------------------------------------------------
// Case 3 — solitary unfreeze via `freeze=false`, nothing else touched.
// MUST succeed, and the result must be identical to the prior document
// except no longer frozen (the ruling's own definition of "sole effect").
// ---------------------------------------------------------------------

#[test]
fn case3_solitary_unfreeze_via_set_false_succeeds_ordinary() {
    let temp = setup(FROZEN_DOC);
    let before = read_to_string(temp.path().join("doc.md")).unwrap();

    let output = run_iwe(
        temp.path(),
        &["update", "-k", "doc", "--set", "freeze=false"],
    );

    assert!(
        output.status.success(),
        "solitary unfreeze must succeed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let after = read_to_string(temp.path().join("doc.md")).unwrap();
    assert!(
        is_solitary_freeze_lift(&before, &after),
        "the only difference between before and after must be freeze's \
         effective state — before={before:?} after={after:?}"
    );
}

#[test]
fn case3_solitary_unfreeze_via_set_false_succeeds_strict() {
    let temp = setup(FROZEN_DOC);
    let before = read_to_string(temp.path().join("doc.md")).unwrap();

    let output = run_iwe(
        temp.path(),
        &[
            "update",
            "-k",
            "doc",
            "--set",
            "freeze=false",
            "--strict",
            "--expect",
            "1",
        ],
    );

    assert!(
        output.status.success(),
        "solitary unfreeze must succeed under --strict too: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let after = read_to_string(temp.path().join("doc.md")).unwrap();
    assert!(is_solitary_freeze_lift(&before, &after));
}

// ---------------------------------------------------------------------
// Case 4 — solitary unfreeze via marker removal (`--unset freeze` alone).
// MUST succeed, same "sole effect" comparison as case 3.
// ---------------------------------------------------------------------

#[test]
fn case4_solitary_unfreeze_via_unset_succeeds_ordinary() {
    let temp = setup(FROZEN_DOC);
    let before = read_to_string(temp.path().join("doc.md")).unwrap();

    let output = run_iwe(temp.path(), &["update", "-k", "doc", "--unset", "freeze"]);

    assert!(
        output.status.success(),
        "solitary unfreeze by key removal must succeed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let after = read_to_string(temp.path().join("doc.md")).unwrap();
    assert!(
        is_solitary_freeze_lift(&before, &after),
        "before={before:?} after={after:?}"
    );
    assert!(
        !frontmatter(&after).contains_key(Value::String(FREEZE_FIELD.to_string())),
        "marker removal means the key is gone, not merely false: {after:?}"
    );
}

#[test]
fn case4_solitary_unfreeze_via_unset_succeeds_strict() {
    let temp = setup(FROZEN_DOC);
    let before = read_to_string(temp.path().join("doc.md")).unwrap();

    let output = run_iwe(
        temp.path(),
        &[
            "update", "-k", "doc", "--unset", "freeze", "--strict", "--expect", "1",
        ],
    );

    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let after = read_to_string(temp.path().join("doc.md")).unwrap();
    assert!(is_solitary_freeze_lift(&before, &after));
}

// ---------------------------------------------------------------------
// Case 5 — freezing an *unfrozen* document plus another property change,
// one call. MUST succeed: "freezing is not restricted" is a deliberate
// asymmetry with cases 1/2, not a mirror of them.
// ---------------------------------------------------------------------

#[test]
fn case5_freezing_unfrozen_document_plus_other_property_succeeds_ordinary() {
    let temp = setup(UNFROZEN_DOC);

    let output = run_iwe(
        temp.path(),
        &[
            "update",
            "-k",
            "doc",
            "--set",
            "freeze=true",
            "--set",
            "status=reviewed",
        ],
    );

    assert!(
        output.status.success(),
        "freezing an unfrozen document may carry other changes in the same \
         call — the document was not frozen when the predicate ran, so no \
         guarantee is engaged; do not assume this must be rejected the way \
         cases 1/2 are: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let after = read_to_string(temp.path().join("doc.md")).unwrap();
    let fm = frontmatter(&after);
    assert_eq!(
        fm.get(Value::String(FREEZE_FIELD.to_string())),
        Some(&Value::Bool(true)),
        "document must now carry freeze: true: {after:?}"
    );
    assert_eq!(
        fm.get(Value::String("status".to_string())),
        Some(&Value::String("reviewed".to_string())),
        "the bundled status change must have landed too: {after:?}"
    );
    assert_eq!(
        body(&after),
        body(UNFROZEN_DOC),
        "body must be unchanged: {after:?}"
    );
}

#[test]
fn case5_freezing_unfrozen_document_plus_other_property_succeeds_strict() {
    let temp = setup(UNFROZEN_DOC);

    let output = run_iwe(
        temp.path(),
        &[
            "update",
            "-k",
            "doc",
            "--set",
            "freeze=true",
            "--set",
            "status=reviewed",
            "--strict",
            "--expect",
            "1",
        ],
    );

    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let after = read_to_string(temp.path().join("doc.md")).unwrap();
    let fm = frontmatter(&after);
    assert_eq!(
        fm.get(Value::String(FREEZE_FIELD.to_string())),
        Some(&Value::Bool(true))
    );
    assert_eq!(
        fm.get(Value::String("status".to_string())),
        Some(&Value::String("reviewed".to_string()))
    );
}

// ---------------------------------------------------------------------
// Case 6 — two separate calls: unfreeze alone (succeeds), then a distinct
// write that changes another property (succeeds too, against what is now
// an ordinary unfrozen document). Confirms the rule is about one call, not
// a ban on ever touching the document again after unfreezing.
// ---------------------------------------------------------------------

#[test]
fn case6_two_step_unfreeze_then_separate_write_succeeds_ordinary() {
    let temp = setup(FROZEN_DOC);
    let before = read_to_string(temp.path().join("doc.md")).unwrap();

    let unfreeze = run_iwe(
        temp.path(),
        &["update", "-k", "doc", "--set", "freeze=false"],
    );
    assert!(
        unfreeze.status.success(),
        "step 1 (solitary unfreeze) must succeed: stderr={}",
        String::from_utf8_lossy(&unfreeze.stderr)
    );
    let after_unfreeze = read_to_string(temp.path().join("doc.md")).unwrap();
    assert!(is_solitary_freeze_lift(&before, &after_unfreeze));

    let second = run_iwe(
        temp.path(),
        &["update", "-k", "doc", "--set", "status=reviewed"],
    );
    assert!(
        second.status.success(),
        "step 2, a separate call against the now-unfrozen document, must \
         succeed: stderr={}",
        String::from_utf8_lossy(&second.stderr)
    );
    let after_second = read_to_string(temp.path().join("doc.md")).unwrap();
    let fm = frontmatter(&after_second);
    assert_eq!(
        fm.get(Value::String("status".to_string())),
        Some(&Value::String("reviewed".to_string()))
    );
    assert!(!effectively_frozen(&after_second));
}

#[test]
fn case6_two_step_unfreeze_then_separate_write_succeeds_strict() {
    let temp = setup(FROZEN_DOC);

    let unfreeze = run_iwe(
        temp.path(),
        &[
            "update",
            "-k",
            "doc",
            "--set",
            "freeze=false",
            "--strict",
            "--expect",
            "1",
        ],
    );
    assert!(
        unfreeze.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&unfreeze.stderr)
    );

    let second = run_iwe(
        temp.path(),
        &[
            "update",
            "-k",
            "doc",
            "--set",
            "status=reviewed",
            "--strict",
            "--expect",
            "1",
        ],
    );
    assert!(
        second.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&second.stderr)
    );
    let after_second = read_to_string(temp.path().join("doc.md")).unwrap();
    assert!(!effectively_frozen(&after_second));
}

// ---------------------------------------------------------------------
// Case 7 — body-vs-frontmatter interaction: a frozen document, one call
// that clears freeze AND changes the body. MUST be rejected — the body
// counts as part of "every frontmatter property and the body" the sole-
// effect comparison covers, exactly as much as any named frontmatter
// field does. Driven through body-overwrite mode (`-c`/`--content`),
// which is the only CLI mechanism that can touch the body and frontmatter
// in the same call (`--content` cannot be combined with `--set`/`--unset`
// — see `update_command`'s mode split in `crates/iwe/src/main.rs`).
// ---------------------------------------------------------------------

const FROZEN_DOC_FALSE_PLUS_NEW_BODY: &str = "\
---
freeze: false
status: draft
---

# Frozen Document

New body.
";

const FROZEN_DOC_REMOVED_PLUS_NEW_BODY: &str = "\
---
status: draft
---

# Frozen Document

New body.
";

#[test]
fn case7_body_change_plus_freeze_false_is_rejected_ordinary() {
    let temp = setup(FROZEN_DOC);
    let before = read_to_string(temp.path().join("doc.md")).unwrap();

    let output = run_iwe(
        temp.path(),
        &["update", "-k", "doc", "-c", FROZEN_DOC_FALSE_PLUS_NEW_BODY],
    );

    assert!(
        !output.status.success(),
        "clearing freeze (set false) and changing the body in the same \
         call must be rejected — the body is not exempt from the sole- \
         effect comparison: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_rejected_and_names_frozen(&output, "doc");
    assert_eq!(read_to_string(temp.path().join("doc.md")).unwrap(), before);
}

#[test]
fn case7_body_change_plus_freeze_false_is_rejected_strict() {
    let temp = setup(FROZEN_DOC);
    let before = read_to_string(temp.path().join("doc.md")).unwrap();

    let output = run_iwe(
        temp.path(),
        &[
            "update",
            "-k",
            "doc",
            "-c",
            FROZEN_DOC_FALSE_PLUS_NEW_BODY,
            "--strict",
        ],
    );

    assert!(!output.status.success());
    assert_rejected_and_names_frozen(&output, "doc");
    assert_eq!(read_to_string(temp.path().join("doc.md")).unwrap(), before);
}

#[test]
fn case7_body_change_plus_freeze_removed_is_rejected_ordinary() {
    let temp = setup(FROZEN_DOC);
    let before = read_to_string(temp.path().join("doc.md")).unwrap();

    let output = run_iwe(
        temp.path(),
        &[
            "update",
            "-k",
            "doc",
            "-c",
            FROZEN_DOC_REMOVED_PLUS_NEW_BODY,
        ],
    );

    assert!(
        !output.status.success(),
        "removing the freeze key entirely and changing the body in the \
         same call must be rejected too, same as the set-false variant: \
         stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_rejected_and_names_frozen(&output, "doc");
    assert_eq!(read_to_string(temp.path().join("doc.md")).unwrap(), before);
}

#[test]
fn case7_body_change_plus_freeze_removed_is_rejected_strict() {
    let temp = setup(FROZEN_DOC);
    let before = read_to_string(temp.path().join("doc.md")).unwrap();

    let output = run_iwe(
        temp.path(),
        &[
            "update",
            "-k",
            "doc",
            "-c",
            FROZEN_DOC_REMOVED_PLUS_NEW_BODY,
            "--strict",
        ],
    );

    assert!(!output.status.success());
    assert_rejected_and_names_frozen(&output, "doc");
    assert_eq!(read_to_string(temp.path().join("doc.md")).unwrap(), before);
}

// ---------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------

fn setup(doc_content: &str) -> TempDir {
    let temp_dir = TempDir::new().expect("tempdir");
    let temp_path = temp_dir.path();
    create_dir_all(temp_path.join(".iwe")).expect("mkdir .iwe");
    let config = Configuration {
        library: LibraryOptions {
            path: "".to_string(),
            ..Default::default()
        },
        markdown: MarkdownOptions {
            refs_extension: "".to_string(),
            ..Default::default()
        },
        ..Default::default()
    };
    write(
        temp_path.join(".iwe").join("config.toml"),
        toml::to_string(&config).expect("config"),
    )
    .expect("write config");
    write(temp_path.join("doc.md"), doc_content).expect("write doc");
    temp_dir
}

fn run_iwe(work_dir: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(crate::common::get_iwe_binary_path());
    command.current_dir(work_dir);
    for arg in args {
        command.arg(arg);
    }
    command.output().expect("run iwe")
}

fn assert_rejected_and_names_frozen(output: &Output, key: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(key) && stderr.contains("frozen"),
        "rejection must name the document and attribute the rejection to \
         freeze: got {stderr}"
    );
}

fn frontmatter(content: &str) -> Mapping {
    parse_leading_frontmatter(content).unwrap_or_default()
}

fn body(content: &str) -> String {
    split_raw_frontmatter(content).1.to_string()
}

fn effectively_frozen(content: &str) -> bool {
    frontmatter(content)
        .get(Value::String(FREEZE_FIELD.to_string()))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// The ruling's own definition of "sole effect": `after` is identical to
/// `before` in every frontmatter property and the body, except that
/// `before` was effectively frozen and `after` is not.
fn is_solitary_freeze_lift(before: &str, after: &str) -> bool {
    if !effectively_frozen(before) || effectively_frozen(after) {
        return false;
    }
    let mut before_fm = frontmatter(before);
    let mut after_fm = frontmatter(after);
    before_fm.remove(Value::String(FREEZE_FIELD.to_string()));
    after_fm.remove(Value::String(FREEZE_FIELD.to_string()));
    before_fm == after_fm && body(before) == body(after)
}
