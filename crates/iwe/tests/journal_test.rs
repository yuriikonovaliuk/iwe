// A generic transaction journal: on committing a transaction, IWE appends
// one newline-delimited-JSON record of that transaction's affected keys to
// a configured journal path, for audit trails, undo, backup tooling, or
// external sync. IWE reports; it does not delegate, and nothing external
// approves a commit.
//
// Pinned format (config key `journal.path`, default unset):
//   {"seq": <u64 monotonic>, "tx": "<uuid>", "effects": [{"key": "<doc
//   key>", "effect": "create|update|delete"}]}
//
// Written from this task's acceptance criteria alone, without reading a
// Developer's implementation of the journal writer -- independence is the
// point (see `roles/delivery/test-builder`). Since `journal.path` is not
// yet a recognized `Configuration` field as of this writing, these tests
// are expected to fail against an unmodified `iwe` binary: config parsing
// itself will reject the `[journal]` table until the feature lands.

use diwe::config::Configuration;
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs::{create_dir_all, read_to_string, write};
use std::path::Path;
use std::process::{Command, Output};
use tempfile::TempDir;

// ---------------------------------------------------------------------
// Workspace setup
// ---------------------------------------------------------------------

/// A fresh workspace with `docs` already written to disk and a
/// `.iwe/config.toml` in place. `journal_path`, if given, is written as a
/// raw `[journal]\npath = "..."` TOML table appended to the serialized
/// `Configuration` -- not through the `Configuration` struct itself, since
/// the pinned config key is the contract here, independent of whatever
/// Rust field name a journal implementation ends up using.
fn setup(docs: Vec<(&str, &str)>, journal_path: Option<&str>) -> TempDir {
    let temp = TempDir::new().expect("tempdir");
    create_dir_all(temp.path().join(".iwe")).expect("mkdir .iwe");
    write_config(temp.path(), journal_path);
    for (key, content) in docs {
        let path = temp.path().join(format!("{}.md", key));
        if let Some(parent) = path.parent() {
            create_dir_all(parent).expect("mkdir doc parent");
        }
        write(path, content).expect("write doc");
    }
    temp
}

fn write_config(root: &Path, journal_path: Option<&str>) {
    let mut config = Configuration::default();
    config.library.path = "".to_string();
    config.markdown.refs_extension = "".to_string();
    let mut text = toml::to_string(&config).expect("config serializes");
    if let Some(journal_path) = journal_path {
        // `Configuration::default()` already serializes an empty `[journal]`
        // table (the journal implementation's own `JournalOptions` field
        // round-trips even with `path` unset) -- strip that empty table
        // before appending the pinned `journal.path` key below, or TOML
        // parsing rejects the file for a duplicate `[journal]` table.
        text = text.replace("[journal]\n\n", "");
        text.push_str(&format!("\n[journal]\npath = {:?}\n", journal_path));
    }
    write(root.join(".iwe").join("config.toml"), text).expect("write config");
}

fn run(work_dir: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(crate::common::get_iwe_binary_path());
    command.current_dir(work_dir);
    for arg in args {
        command.arg(arg);
    }
    command.output().expect("run iwe")
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

// ---------------------------------------------------------------------
// Journal file reading + shape assertions
// ---------------------------------------------------------------------

/// Every record in the journal at `root.join(rel_path)`, parsed as JSON,
/// in file order. An absent file is treated as zero records (the "no
/// record appended" case is common enough in these tests that forcing
/// every caller to check existence first would just be noise).
fn journal_records(root: &Path, rel_path: &str) -> Vec<Value> {
    let path = root.join(rel_path);
    if !path.exists() {
        return Vec::new();
    }
    let text = read_to_string(&path).expect("journal file is readable");
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| {
            panic!("journal line is not valid JSON: {e}\nline: {line}")
        }))
        .collect()
}

/// Asserts `record` has exactly the pinned top-level shape -- `seq`
/// (u64), `tx` (a valid UUID string), `effects` (array) -- and nothing
/// else, then returns the effects as `(key, effect)` pairs for the
/// caller to check against the expected set.
fn assert_pinned_shape(record: &Value) -> Vec<(String, String)> {
    let object = record.as_object().expect("record is a JSON object");
    let keys: BTreeSet<&str> = object.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        BTreeSet::from(["seq", "tx", "effects"]),
        "a journal record must carry exactly seq, tx, effects and nothing else, got: {record}"
    );

    assert!(
        record["seq"].is_u64(),
        "seq must be an unsigned integer, got: {:?}",
        record["seq"]
    );

    let tx = record["tx"].as_str().unwrap_or_else(|| {
        panic!("tx must be a string, got: {:?}", record["tx"])
    });
    uuid::Uuid::parse_str(tx)
        .unwrap_or_else(|e| panic!("tx must be a valid UUID, got {tx:?}: {e}"));

    let effects = record["effects"]
        .as_array()
        .unwrap_or_else(|| panic!("effects must be an array, got: {:?}", record["effects"]));

    effects
        .iter()
        .map(|effect| {
            let object = effect.as_object().expect("effect is a JSON object");
            let keys: BTreeSet<&str> = object.keys().map(String::as_str).collect();
            assert_eq!(
                keys,
                BTreeSet::from(["key", "effect"]),
                "an effect entry must carry exactly key, effect and nothing else, got: {effect}"
            );
            let key = effect["key"]
                .as_str()
                .unwrap_or_else(|| panic!("effect.key must be a string, got: {effect}"))
                .to_string();
            let kind = effect["effect"]
                .as_str()
                .unwrap_or_else(|| panic!("effect.effect must be a string, got: {effect}"))
                .to_string();
            assert!(
                ["create", "update", "delete"].contains(&kind.as_str()),
                "effect.effect must be create|update|delete, got: {kind:?}"
            );
            (key, kind)
        })
        .collect()
}

fn seq_of(record: &Value) -> u64 {
    record["seq"].as_u64().expect("seq is a u64")
}

// ---------------------------------------------------------------------
// AC1: successful commit -> exactly one record, effects covering every
// touched key, in the pinned shape.
// ---------------------------------------------------------------------

#[test]
fn create_of_a_new_document_appends_exactly_one_record() {
    let temp = setup(vec![], Some("journal.ndjson"));

    let output = run(
        temp.path(),
        &["create", "note", "--content", "# Note\n\nBody.\n"],
    );
    assert!(output.status.success(), "stderr: {}", stderr_of(&output));

    let records = journal_records(temp.path(), "journal.ndjson");
    assert_eq!(records.len(), 1, "records: {records:?}");
    let effects = assert_pinned_shape(&records[0]);
    assert_eq!(effects, vec![("note".to_string(), "create".to_string())]);
}

#[test]
fn update_of_an_existing_document_appends_exactly_one_record() {
    let temp = setup(
        vec![("note", "# Note\n\nOld body.\n")],
        Some("journal.ndjson"),
    );

    let output = run(
        temp.path(),
        &["update", "-k", "note", "-c", "# Note\n\nNew body.\n"],
    );
    assert!(output.status.success(), "stderr: {}", stderr_of(&output));

    let records = journal_records(temp.path(), "journal.ndjson");
    assert_eq!(records.len(), 1, "records: {records:?}");
    let effects = assert_pinned_shape(&records[0]);
    assert_eq!(effects, vec![("note".to_string(), "update".to_string())]);
}

#[test]
fn delete_of_an_existing_document_appends_exactly_one_record() {
    let temp = setup(
        vec![("note", "# Note\n\nBody.\n")],
        Some("journal.ndjson"),
    );

    let output = run(temp.path(), &["delete", "-k", "note"]);
    assert!(output.status.success(), "stderr: {}", stderr_of(&output));

    let records = journal_records(temp.path(), "journal.ndjson");
    assert_eq!(records.len(), 1, "records: {records:?}");
    let effects = assert_pinned_shape(&records[0]);
    assert_eq!(effects, vec![("note".to_string(), "delete".to_string())]);
}

// ---------------------------------------------------------------------
// AC2: an aborted or rejected transaction appends NO record. Two
// independent trigger paths, both real and already present in this
// codebase: deleting a frozen document (rejected on the removal path)
// and updating a frozen document's body (rejected on the write path,
// M2's failed-state-then-abort-only contract).
// ---------------------------------------------------------------------

const FROZEN_DOC: &str = "---\nfreeze: true\n---\n\n# Frozen\n\nOriginal body.\n";

#[test]
fn deleting_a_frozen_document_is_rejected_and_appends_no_record() {
    let temp = setup(vec![("frozen", FROZEN_DOC)], Some("journal.ndjson"));

    let output = run(temp.path(), &["delete", "-k", "frozen"]);
    assert!(
        !output.status.success(),
        "delete of a frozen document must be rejected"
    );
    assert!(
        temp.path().join("frozen.md").exists(),
        "frozen document must not have been removed"
    );
    assert_eq!(
        journal_records(temp.path(), "journal.ndjson"),
        Vec::<Value>::new(),
        "a rejected delete must append no journal record"
    );
}

#[test]
fn updating_a_frozen_documents_body_is_rejected_and_appends_no_record() {
    let temp = setup(vec![("frozen", FROZEN_DOC)], Some("journal.ndjson"));
    let before = read_to_string(temp.path().join("frozen.md")).unwrap();

    let output = run(
        temp.path(),
        &["update", "-k", "frozen", "-c", "# Frozen\n\nNew body.\n"],
    );
    assert!(
        !output.status.success(),
        "update of a frozen document's body must be rejected"
    );
    assert_eq!(
        read_to_string(temp.path().join("frozen.md")).unwrap(),
        before,
        "frozen document must be unchanged on disk"
    );
    assert_eq!(
        journal_records(temp.path(), "journal.ndjson"),
        Vec::<Value>::new(),
        "a permission-rejected write must append no journal record"
    );
}

// ---------------------------------------------------------------------
// AC3 (default unset -> no behavior change) is covered by
// scripts/journal_baseline_diff.sh and journal_baseline_diff_test.rs,
// which run the full existing regression suite twice (with and without
// the journal change present, journal.path unconfigured both times) and
// diff the output -- a single CLI-level test here can't establish "the
// entire rest of the suite is byte-identical" on its own.
//
// This test covers the narrower, still load-bearing half of AC3 that
// *can* live at this level: an ordinary command with no `.iwe/config.toml`
// `[journal]` table at all writes no journal file anywhere in the
// workspace, and behaves like an ordinary successful create.
// ---------------------------------------------------------------------

#[test]
fn no_journal_path_configured_writes_no_journal_file_and_behaves_normally() {
    let temp = setup(vec![], None);

    let output = run(
        temp.path(),
        &["create", "note", "--content", "# Note\n\nBody.\n"],
    );
    assert!(output.status.success(), "stderr: {}", stderr_of(&output));
    assert_eq!(
        read_to_string(temp.path().join("note.md")).unwrap(),
        "# Note\n\nBody.\n"
    );

    // No plausible journal file materialized anywhere under the
    // workspace when nothing configured one.
    let candidates = ["journal.ndjson", ".iwe/journal.ndjson", "journal.jsonl"];
    for candidate in candidates {
        assert!(
            !temp.path().join(candidate).exists(),
            "no journal file should exist when journal.path is unset: {candidate}"
        );
    }
}

// ---------------------------------------------------------------------
// AC4: journal.path pointing at an unwritable location -> the
// transaction still commits and succeeds from the caller's perspective;
// the failure is visible only through ordinary error/warning reporting
// (stderr, non-zero-*commit*-independent), never as a failed commit.
// ---------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn unwritable_journal_path_still_commits_the_transaction() {
    use std::os::unix::fs::PermissionsExt;

    let temp = setup(vec![], Some("readonly-dir/journal.ndjson"));
    create_dir_all(temp.path().join("readonly-dir")).unwrap();
    std::fs::set_permissions(
        temp.path().join("readonly-dir"),
        std::fs::Permissions::from_mode(0o555),
    )
    .unwrap();

    let result = std::panic::catch_unwind(|| {
        let output = run(
            temp.path(),
            &["create", "note", "--content", "# Note\n\nBody.\n"],
        );

        assert!(
            output.status.success(),
            "the transaction must still commit and succeed even though the \
             journal write fails; stderr: {}",
            stderr_of(&output)
        );
        assert_eq!(
            read_to_string(temp.path().join("note.md")).unwrap(),
            "# Note\n\nBody.\n",
            "the actual document write must have landed"
        );
        assert!(
            !stderr_of(&output).is_empty(),
            "an unwritable journal path must surface through IWE's ordinary \
             error/warning channel, not be silently swallowed"
        );
    });

    // Always restore permissions so TempDir can clean itself up, even if
    // an assertion above panicked.
    let _ = std::fs::set_permissions(
        temp.path().join("readonly-dir"),
        std::fs::Permissions::from_mode(0o755),
    );
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

// ---------------------------------------------------------------------
// AC5: a multi-document transaction (>=3 keys, a mix of create / update /
// delete) -> exactly ONE record, not one per key, with all three effect
// kinds represented in that one record's effects array. `iwe rename`
// gives exactly this in one invocation: it creates the document under
// its new key, removes it under the old key, and updates every document
// that referenced the old key -- all as one logical operation.
// ---------------------------------------------------------------------

#[test]
fn renaming_a_referenced_document_appends_exactly_one_record_with_all_three_effect_kinds() {
    let temp = setup(
        vec![
            ("old-name", "# Old Name\n\nBody.\n"),
            ("referrer", "# Referrer\n\nSee [Old Name](old-name).\n"),
        ],
        Some("journal.ndjson"),
    );

    let output = run(temp.path(), &["rename", "old-name", "new-name"]);
    assert!(output.status.success(), "stderr: {}", stderr_of(&output));

    // Sanity check on the filesystem outcome this record is supposed to
    // describe, independent of the journal itself.
    assert!(!temp.path().join("old-name.md").exists());
    assert!(temp.path().join("new-name.md").exists());

    let records = journal_records(temp.path(), "journal.ndjson");
    assert_eq!(
        records.len(),
        1,
        "one `iwe rename` invocation touching 3 keys must append exactly \
         one record, not one per key; records: {records:?}"
    );
    let mut effects = assert_pinned_shape(&records[0]);
    effects.sort();

    let mut expected = vec![
        ("new-name".to_string(), "create".to_string()),
        ("old-name".to_string(), "delete".to_string()),
        ("referrer".to_string(), "update".to_string()),
    ];
    expected.sort();
    assert_eq!(effects, expected);
}

// ---------------------------------------------------------------------
// AC6: seq monotonicity across multiple commits and process restarts.
// The `iwe` CLI already re-initializes fully on every invocation (it is
// not a long-lived daemon in these tests), so a sequence of separate
// `run()` calls already is a sequence of separate process lifetimes --
// exactly the "restart between commits" scenario.
// ---------------------------------------------------------------------

#[test]
fn seq_is_strictly_increasing_and_never_repeats_across_separate_process_invocations() {
    let temp = setup(vec![], Some("journal.ndjson"));

    // Three ordinary commits, each its own process.
    for i in 0..3 {
        let output = run(
            temp.path(),
            &[
                "create",
                &format!("doc-{i}"),
                "--content",
                &format!("# Doc {i}\n\nBody.\n"),
            ],
        );
        assert!(output.status.success(), "stderr: {}", stderr_of(&output));
    }

    // A rejected commit in between successful ones: whether or not it
    // consumes a seq value is left to the implementation (the pinned
    // contract only promises no repeat and no regression), but it must
    // not cause a later commit's seq to repeat or go backwards.
    let reject_temp_doc = temp.path().join("frozen-interloper.md");
    write(&reject_temp_doc, FROZEN_DOC).unwrap();
    let rejected = run(
        temp.path(),
        &[
            "update",
            "-k",
            "frozen-interloper",
            "-c",
            "# Frozen\n\nshould be rejected\n",
        ],
    );
    assert!(!rejected.status.success());

    // Two more ordinary commits, again each its own process.
    for i in 3..5 {
        let output = run(
            temp.path(),
            &[
                "create",
                &format!("doc-{i}"),
                "--content",
                &format!("# Doc {i}\n\nBody.\n"),
            ],
        );
        assert!(output.status.success(), "stderr: {}", stderr_of(&output));
    }

    let records = journal_records(temp.path(), "journal.ndjson");
    assert_eq!(
        records.len(),
        5,
        "5 successful commits, 1 rejected -> exactly 5 records; records: {records:?}"
    );

    let seqs: Vec<u64> = records.iter().map(seq_of).collect();
    let mut sorted_unique = seqs.clone();
    sorted_unique.sort_unstable();
    sorted_unique.dedup();
    assert_eq!(
        sorted_unique.len(),
        seqs.len(),
        "no seq value may repeat across commits: {seqs:?}"
    );
    for window in seqs.windows(2) {
        assert!(
            window[1] > window[0],
            "seq must never go backwards or stay flat across successive \
             commits: {seqs:?}"
        );
    }
}
