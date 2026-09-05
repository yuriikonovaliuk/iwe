// The write gate at the CLI: with `[transactions] validate = "full"` every
// `iwe` write command commits through the same validating backend the MCP
// server uses (`diwe::validating_transaction::ValidatingTransaction`), and
// a write that would leave the store worse than it found it is refused —
// the store-level enforcement a store that is not a git repository (the
// compositor's materialized tree) needs, whichever binary an agent writes
// with. Without the section the same writes land, dangling links and all.

use std::collections::HashMap;
use std::fs::{create_dir_all, read_to_string, write};
use std::path::Path;
use std::process::{Command, Output};

use diwe::config::{
    Configuration, LibraryOptions, MarkdownOptions, Patterns, SchemaBinding, TransactionOptions,
    ValidationScope,
};
use tempfile::TempDir;

const HUB: &str = "---\ntype: note\n---\n# Hub\n\nSee [Leaf](leaf).\n";
const LEAF: &str = "---\ntype: note\n---\n# Leaf\n\nBack to [Hub](hub).\n";

/// `notes/**`: every link must resolve to a note. A rule with a `target`
/// filter is one the pending-document shape check cannot evaluate — it
/// is the store-level gate's alone.
fn store(gated: bool) -> TempDir {
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    create_dir_all(base.join(".iwe/schemas")).unwrap();
    let mut schemas = HashMap::new();
    schemas.insert(
        "note".to_string(),
        SchemaBinding {
            r#match: Patterns::One("notes/**".to_string()),
        },
    );
    let scope = if gated {
        ValidationScope::Full
    } else {
        ValidationScope::None
    };
    let config = Configuration {
        library: LibraryOptions {
            path: "".to_string(),
            ..Default::default()
        },
        markdown: MarkdownOptions {
            refs_extension: "".to_string(),
            ..Default::default()
        },
        schemas,
        transactions: TransactionOptions {
            validate: scope,
            ..Default::default()
        },
        ..Default::default()
    };
    write(base.join(".iwe/config.toml"), toml::to_string(&config).unwrap()).unwrap();
    write(
        base.join(".iwe/schemas/note.yaml"),
        "links:\n  - target: { type: note }\n",
    )
    .unwrap();
    create_dir_all(base.join("notes")).unwrap();
    write(base.join("notes/hub.md"), HUB).unwrap();
    write(base.join("notes/leaf.md"), LEAF).unwrap();
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

#[test]
fn full_scope_refuses_a_create_with_a_dangling_link_and_leaves_disk_untouched() {
    let dir = store(true);
    let output = iwe(
        dir.path(),
        &[
            "create",
            "notes/new",
            "--content",
            "---\ntype: note\n---\n# New\n\nSee [Nowhere](nowhere).\n",
        ],
    );

    assert!(!output.status.success(), "the dangling link must be refused");
    let message = stderr(&output);
    assert!(
        message.contains("notes/new") && message.contains("nowhere"),
        "the refusal names the document and the missing target, got: {message}"
    );
    assert!(!dir.path().join("notes/new.md").exists());
}

#[test]
fn full_scope_refuses_an_update_that_breaks_an_untouched_referrer() {
    // Retyping `leaf` is a clean write on its own; it is `hub`, never
    // written, whose link no longer satisfies the target filter. Only a
    // store-level check sees that.
    let dir = store(true);
    let output = iwe(
        dir.path(),
        &[
            "update",
            "-k",
            "notes/leaf",
            "--content",
            "---\ntype: draft\n---\n# Leaf\n\nBack to [Hub](hub).\n",
        ],
    );

    assert!(!output.status.success(), "breaking the referrer must be refused");
    let message = stderr(&output);
    assert!(
        message.contains("notes/hub"),
        "the referrer is named, got: {message}"
    );
    assert_eq!(read_to_string(dir.path().join("notes/leaf.md")).unwrap(), LEAF);
    assert_eq!(read_to_string(dir.path().join("notes/hub.md")).unwrap(), HUB);
}

#[test]
fn full_scope_commits_a_rename_as_one_transaction() {
    // A rename removes the old key and rewrites its referrers: judged one
    // write at a time the removal dangles `hub`; judged as one final
    // state it is clean.
    let dir = store(true);
    let output = iwe(dir.path(), &["rename", "notes/leaf", "notes/renamed"]);
    assert!(output.status.success(), "{}", stderr(&output));

    assert!(!dir.path().join("notes/leaf.md").exists());
    assert!(dir.path().join("notes/renamed.md").exists());
    let hub = read_to_string(dir.path().join("notes/hub.md")).unwrap();
    assert!(hub.contains("(renamed)"), "the referrer was rewritten: {hub}");
}

#[test]
fn full_scope_accepts_a_clean_write() {
    let dir = store(true);
    let output = iwe(
        dir.path(),
        &[
            "create",
            "notes/new",
            "--content",
            "---\ntype: note\n---\n# New\n\nSee [Hub](hub).\n",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    // `iwe create` normalizes what it writes (a blank line after the
    // frontmatter); the gate changes nothing about that.
    assert_eq!(
        read_to_string(dir.path().join("notes/new.md")).unwrap(),
        "---\ntype: note\n---\n\n# New\n\nSee [Hub](hub).\n"
    );
}

#[test]
fn without_the_section_the_same_dangling_write_lands() {
    let dir = store(false);
    let output = iwe(
        dir.path(),
        &[
            "create",
            "notes/new",
            "--content",
            "---\ntype: note\n---\n# New\n\nSee [Nowhere](nowhere).\n",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(dir.path().join("notes/new.md").exists());
}
