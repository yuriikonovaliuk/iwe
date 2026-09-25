// `[integrity]` at the CLI, in real scratch stores: every write command
// commits through the one gate (`diwe::validating_transaction`), which
// refuses a commit that adds (`no-new`) or leaves (`strict`) a broken link
// or an orphan — a document not reachable from `index` — and changes
// nothing on disk when it refuses. `iwe schema validate` reports the same
// thing with the same code: `strict` debt fails it, `no-new` debt is a
// warning. `off` (the default) changes nothing.

use std::fs::{create_dir_all, read_to_string, write};
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

const INDEX: &str = "# Index\n\n- [Hub](hub)\n";
const HUB: &str = "# Hub\n\n- [Leaf](leaf)\n";
const LEAF: &str = "# Leaf\n";
/// Standing debt: an orphan carrying a broken link.
const OLD: &str = "# Old\n\nSee [Gone](gone).\n";

const MODES: [&str; 2] = ["no-new", "strict"];

fn store(mode: &str, with_debt: bool) -> TempDir {
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    create_dir_all(base.join(".iwe")).unwrap();
    write(
        base.join(".iwe/config.toml"),
        format!(
            "version = 3\n\n[library]\npath = \"\"\n\n[markdown]\nrefs_extension = \"\"\n\n\
             [integrity]\nlinks = \"{mode}\"\norphans = \"{mode}\"\nroot = \"index\"\n"
        ),
    )
    .unwrap();
    write(base.join("index.md"), INDEX).unwrap();
    write(base.join("hub.md"), HUB).unwrap();
    write(base.join("leaf.md"), LEAF).unwrap();
    if with_debt {
        write(base.join("old.md"), OLD).unwrap();
    }
    dir
}

fn iwe(work_dir: &Path, args: &[&str]) -> Output {
    Command::new(crate::common::get_iwe_binary_path())
        .args(args)
        .current_dir(work_dir)
        .output()
        .expect("run iwe")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn read(dir: &TempDir, key: &str) -> String {
    read_to_string(dir.path().join(format!("{key}.md"))).unwrap()
}

fn assert_refused(output: &Output, needles: &[&str]) {
    assert!(!output.status.success(), "expected a refusal");
    let message = stderr(output);
    assert!(message.contains("link integrity:"), "{message}");
    for needle in needles {
        assert!(message.contains(needle), "missing {needle:?} in: {message}");
    }
}

#[test]
fn a_new_page_with_no_link_is_refused() {
    for mode in MODES {
        let dir = store(mode, false);
        let output = iwe(dir.path(), &["create", "page", "--content", "# Page\n"]);
        assert_refused(
            &output,
            &["orphan: page", "link_from=", "iwe_tx_begin … iwe_tx_commit"],
        );
        assert!(!dir.path().join("page.md").exists(), "{mode}: nothing lands");
    }
}

#[test]
fn a_new_page_with_link_from_is_accepted_in_one_commit() {
    for mode in MODES {
        let dir = store(mode, false);
        let output = iwe(
            dir.path(),
            &["create", "notes/page", "--content", "# Page\n", "--link-from", "hub"],
        );
        assert!(output.status.success(), "{mode}: {}", stderr(&output));
        assert_eq!(read(&dir, "notes/page"), "# Page\n");
        assert_eq!(read(&dir, "hub"), "# Hub\n\n- [Leaf](leaf)\n- [Page](notes/page)\n");
        let validate = iwe(dir.path(), &["schema", "validate"]);
        assert!(validate.status.success(), "{mode}: {}", stderr(&validate));
    }
}

#[test]
fn a_missing_link_from_is_an_error_and_writes_nothing() {
    for mode in MODES {
        let dir = store(mode, false);
        let output = iwe(
            dir.path(),
            &["create", "page", "--content", "# Page\n", "--link-from", "nowhere"],
        );
        assert!(!output.status.success());
        assert!(stderr(&output).contains("'nowhere' does not exist"), "{}", stderr(&output));
        assert!(!dir.path().join("page.md").exists());
    }
}

#[test]
fn adding_a_link_to_a_missing_key_is_refused() {
    for mode in MODES {
        let dir = store(mode, false);
        let output = iwe(
            dir.path(),
            &["update", "-k", "leaf", "--content", "# Leaf\n\nSee [Missing](missing).\n"],
        );
        assert_refused(&output, &["broken link: leaf → missing"]);
        assert_eq!(read(&dir, "leaf"), LEAF, "{mode}: nothing lands");
    }
}

#[test]
fn deleting_the_only_path_to_a_page_is_refused() {
    for mode in MODES {
        let dir = store(mode, false);
        let output = iwe(dir.path(), &["delete", "hub"]);
        assert_refused(&output, &["orphan: leaf"]);
        assert_eq!(read(&dir, "hub"), HUB);
        assert_eq!(read(&dir, "index"), INDEX, "{mode}: the link cleanup did not land either");
    }
}

#[test]
fn deleting_a_linked_leaf_cleans_its_links_and_is_accepted() {
    // `iwe delete` removes every link to the deleted page in the same
    // commit, so deleting a leaf leaves no broken link and no orphan.
    for mode in MODES {
        let dir = store(mode, false);
        let output = iwe(dir.path(), &["delete", "leaf"]);
        assert!(output.status.success(), "{mode}: {}", stderr(&output));
        assert!(!dir.path().join("leaf.md").exists());
        assert!(!read(&dir, "hub").contains("leaf"));
    }
}

#[test]
fn rename_keeps_integrity() {
    for mode in MODES {
        let dir = store(mode, false);
        let output = iwe(dir.path(), &["rename", "hub", "topics/hub"]);
        assert!(output.status.success(), "{mode}: {}", stderr(&output));
        assert!(read(&dir, "index").contains("(topics/hub)"));
        let validate = iwe(dir.path(), &["schema", "validate"]);
        assert!(validate.status.success(), "{mode}: {}", stderr(&validate));
    }
}

#[test]
fn no_new_tolerates_standing_debt_and_allows_reducing_it() {
    let dir = store("no-new", true);
    // An unrelated write lands despite the standing orphan and broken link.
    let output = iwe(dir.path(), &["update", "-k", "leaf", "--content", "# Leaf\n\nMore.\n"]);
    assert!(output.status.success(), "{}", stderr(&output));
    // The debt may stay: rewriting the orphan keeps it an orphan.
    let output = iwe(
        dir.path(),
        &["update", "-k", "old", "--content", "# Old\n\nStill see [Gone](gone).\n"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    // … and shrink: dropping the broken link.
    let output = iwe(dir.path(), &["update", "-k", "old", "--content", "# Old\n"]);
    assert!(output.status.success(), "{}", stderr(&output));
    // … but not grow.
    let output = iwe(
        dir.path(),
        &["update", "-k", "old", "--content", "# Old\n\n[Again](gone)\n"],
    );
    assert_refused(&output, &["broken link: old → gone"]);
    assert_eq!(read(&dir, "old"), "# Old\n");
}

#[test]
fn strict_refuses_any_debt() {
    let dir = store("strict", true);
    let output = iwe(dir.path(), &["update", "-k", "leaf", "--content", "# Leaf\n\nMore.\n"]);
    assert_refused(&output, &["orphan: old", "broken link: old → gone"]);
    assert_eq!(read(&dir, "leaf"), LEAF);

    // Paying the debt in one commit is accepted: link the orphan and fix its link.
    let output = iwe(
        dir.path(),
        &["update", "-k", "old", "--content", "# Old\n\nSee [Leaf](leaf).\n"],
    );
    assert_refused(&output, &["orphan: old"]);
    let output = iwe(dir.path(), &["delete", "old"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let validate = iwe(dir.path(), &["schema", "validate"]);
    assert!(validate.status.success(), "{}", stderr(&validate));
}

#[test]
fn off_leaves_behaviour_unchanged() {
    let dir = store("off", true);
    let output = iwe(dir.path(), &["create", "page", "--content", "# Page\n\n[M](missing)\n"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(dir.path().join("page.md").exists());
    let output = iwe(dir.path(), &["delete", "hub"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let validate = iwe(dir.path(), &["schema", "validate"]);
    assert!(validate.status.success(), "{}", stderr(&validate));
    assert!(!stderr(&validate).contains("orphan"));
    let stats = iwe(dir.path(), &["stats", "-f", "json"]);
    let json: serde_json::Value = serde_json::from_slice(&stats.stdout).unwrap();
    assert!(json.get("integrity").is_none(), "no integrity block when off");
}

#[test]
fn schema_validate_agrees_with_the_commit_gate() {
    // strict: a page the gate would refuse, placed on disk behind iwe's
    // back (as a kc drain may find it), fails validation — whole store and
    // selected, keyed to the orphan and to the broken link's source.
    let dir = store("strict", false);
    write(dir.path().join("stray.md"), "# Stray\n\n[M](missing)\n").unwrap();
    let whole = iwe(dir.path(), &["schema", "validate", "-f", "json"]);
    assert_eq!(whole.status.code(), Some(1), "{}", stderr(&whole));
    let reports: serde_json::Value = serde_json::from_slice(&whole.stdout).unwrap();
    let keywords: Vec<(String, String, String)> = reports
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|report| {
            let key = report["key"].as_str().unwrap().to_string();
            let schema = report["schema"].as_str().unwrap().to_string();
            report["violations"]
                .as_array()
                .unwrap()
                .iter()
                .map(move |v| (key.clone(), schema.clone(), v["keyword"].as_str().unwrap().to_string()))
                .collect::<Vec<_>>()
        })
        .collect();
    assert!(keywords.contains(&("stray".into(), "integrity".into(), "broken-link".into())));
    assert!(keywords.contains(&("stray".into(), "integrity".into(), "orphan".into())));
    let selected = iwe(dir.path(), &["schema", "validate", "-k", "leaf"]);
    assert!(selected.status.success(), "a selection keeps only its own reports");
    let selected = iwe(dir.path(), &["schema", "validate", "-k", "stray"]);
    assert_eq!(selected.status.code(), Some(1));
    // … and the gate refuses the very same state for any commit.
    let output = iwe(dir.path(), &["update", "-k", "leaf", "--content", "# Leaf\n\nMore.\n"]);
    assert_refused(&output, &["orphan: stray", "broken link: stray → missing"]);

    // no-new: standing debt is reported as a warning and never fails the
    // run — so a drain never quarantines a write over old debt — while the
    // gate still refuses growth.
    let dir = store("no-new", true);
    let validate = iwe(dir.path(), &["schema", "validate"]);
    assert!(validate.status.success(), "{}", stderr(&validate));
    let warnings = stderr(&validate);
    assert!(warnings.contains("warning: old: orphan"), "{warnings}");
    assert!(warnings.contains("warning: old: broken link"), "{warnings}");
    let output = iwe(dir.path(), &["create", "page", "--content", "# Page\n"]);
    assert_refused(&output, &["orphan: page"]);
}

#[test]
fn whole_store_normalize_goes_through_the_gate() {
    // no-new: normalizing adds no debt, so it lands despite standing debt.
    let dir = store("no-new", true);
    write(dir.path().join("leaf.md"), "# Leaf\n\n\n\nText.\n").unwrap();
    let output = iwe(dir.path(), &["normalize"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(read(&dir, "leaf"), "# Leaf\n\nText.\n");

    // strict: the store holds debt, so the whole-store rewrite is refused
    // and nothing is rewritten.
    let dir = store("strict", true);
    write(dir.path().join("leaf.md"), "# Leaf\n\n\n\nText.\n").unwrap();
    let output = iwe(dir.path(), &["normalize"]);
    assert_refused(&output, &["orphan: old"]);
    assert_eq!(read(&dir, "leaf"), "# Leaf\n\n\n\nText.\n");
}

#[test]
fn stats_report_the_integrity_debt() {
    let dir = store("no-new", true);
    let stats = iwe(dir.path(), &["stats", "-f", "json"]);
    let json: serde_json::Value = serde_json::from_slice(&stats.stdout).unwrap();
    let integrity = &json["integrity"];
    assert_eq!(integrity["brokenLinkCount"], 1);
    assert_eq!(integrity["unreachableDocuments"], 1);
    assert_eq!(integrity["unreachable"], serde_json::json!(["old"]));
    assert_eq!(integrity["links"], "no-new");
}

#[test]
fn integrity_composes_with_affected_set_validation() {
    // The live store's shape: `validate = "affected-set-with-checkers"`
    // plus `[integrity]`. Both run at the same commit over the same
    // parsed state; either can refuse.
    let dir = store("no-new", true);
    let config = dir.path().join(".iwe/config.toml");
    let text = read_to_string(&config).unwrap();
    write(
        &config,
        format!("{text}\n[transactions]\nvalidate = \"affected-set-with-checkers\"\n"),
    )
    .unwrap();
    let output = iwe(dir.path(), &["create", "page", "--content", "# Page\n"]);
    assert_refused(&output, &["orphan: page"]);
    let output = iwe(
        dir.path(),
        &["create", "page", "--content", "# Page\n", "--link-from", "index"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    // A selected validation of a key carrying old debt is not a failure
    // under no-new, so kc's pending-keys drain never quarantines over it.
    let validate = iwe(dir.path(), &["schema", "validate", "-k", "old"]);
    assert!(validate.status.success(), "{}", stderr(&validate));
}
