// The rename and delete cascade at the CLI: a frontmatter field that a bound
// schema reads as a document key (`$key: $this.frontmatter.<path>`) follows a
// rename of its target in the same transaction, and a delete of a target so
// named is refused with the referrer's name. Under full validation the rename
// passes the gate because links and frontmatter change together.

use std::collections::HashMap;
use std::fs::{create_dir_all, read_to_string, write};
use std::path::Path;
use std::process::{Command, Output};

use diwe::config::{
    Configuration, MarkdownOptions, Patterns, SchemaBinding, TransactionOptions, ValidationScope,
    WorkspaceOptions,
};
use tempfile::TempDir;

const CLAIM_SCHEMA: &str = "\
links:
  - some: { type: concept, $key: $this.frontmatter.proposition.subject }
  - covers: { $key: $this.frontmatter.proposition.qualifiers.term, type: concept }
";

const CLAIM: &str = "\
---
type: claim
proposition:
  subject: concepts/cat
  predicate: relations/chases
  qualifiers:
  - relation: relations/in
    term: concepts/garden
---

# Cats chase mice

A [cat](../concepts/cat) chases a mouse in a [garden](../concepts/garden).
";

fn store() -> TempDir {
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    create_dir_all(base.join(".iwe/schemas")).unwrap();
    let mut schemas = HashMap::new();
    schemas.insert(
        "claim".to_string(),
        SchemaBinding {
            r#match: Patterns::One("claims/**".to_string()),
        },
    );
    let config = Configuration {
        workspace: WorkspaceOptions {
            path: "".to_string(),
            ..Default::default()
        },
        markdown: MarkdownOptions {
            refs_extension: "".to_string(),
            ..Default::default()
        },
        schemas,
        transactions: TransactionOptions {
            validate: ValidationScope::Full,
            ..Default::default()
        },
        ..Default::default()
    };
    write(base.join(".iwe/config.toml"), toml::to_string(&config).unwrap()).unwrap();
    write(base.join(".iwe/schemas/claim.yaml"), CLAIM_SCHEMA).unwrap();
    create_dir_all(base.join("claims")).unwrap();
    create_dir_all(base.join("concepts")).unwrap();
    write(base.join("claims/cats.md"), CLAIM).unwrap();
    write(base.join("concepts/cat.md"), "---\ntype: concept\n---\n# Cat\n").unwrap();
    write(base.join("concepts/garden.md"), "---\ntype: concept\n---\n# Garden\n").unwrap();
    dir
}

fn iwe(work_dir: &Path, args: &[&str]) -> Output {
    Command::new(crate::common::get_iwe_binary_path())
        .args(args)
        .current_dir(work_dir)
        .output()
        .expect("run iwe")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn rename_rewrites_the_frontmatter_reference_with_the_link_and_passes_the_gate() {
    let dir = store();
    let output = iwe(dir.path(), &["rename", "concepts/cat", "concepts/feline"]);
    assert!(output.status.success(), "{}", text(&output.stderr));
    let stdout = text(&output.stdout);
    assert!(
        stdout.contains("Rewrote 1 frontmatter reference(s)") && stdout.contains("claims/cats (proposition.subject)"),
        "the cascade is reported: {stdout}"
    );
    let claim = read_to_string(dir.path().join("claims/cats.md")).unwrap();
    assert!(claim.contains("subject: concepts/feline"), "{claim}");
    assert!(claim.contains("(../concepts/feline)"), "{claim}");
    assert!(claim.contains("term: concepts/garden"), "an unrelated key field stays: {claim}");
    assert!(!dir.path().join("concepts/cat.md").exists());
    assert!(dir.path().join("concepts/feline.md").exists());
}

#[test]
fn dry_run_reports_the_cascade_and_writes_nothing() {
    let dir = store();
    let output = iwe(dir.path(), &["rename", "--dry-run", "concepts/garden", "concepts/yard"]);
    assert!(output.status.success(), "{}", text(&output.stderr));
    let stdout = text(&output.stdout);
    assert!(
        stdout.contains("Would rewrite 1 frontmatter reference(s)")
            && stdout.contains("claims/cats (proposition.qualifiers.term)"),
        "{stdout}"
    );
    assert_eq!(read_to_string(dir.path().join("claims/cats.md")).unwrap(), CLAIM);
    assert!(dir.path().join("concepts/garden.md").exists());
}

#[test]
fn delete_of_a_target_named_in_frontmatter_is_refused_and_names_the_referrer() {
    let dir = store();
    let output = iwe(dir.path(), &["delete", "concepts/cat"]);
    assert!(!output.status.success(), "the delete must be refused");
    let stderr = text(&output.stderr);
    assert!(
        stderr.contains("delete refused") && stderr.contains("claims/cats (proposition.subject)"),
        "{stderr}"
    );
    assert!(dir.path().join("concepts/cat.md").exists());
    assert_eq!(read_to_string(dir.path().join("claims/cats.md")).unwrap(), CLAIM);
}

#[test]
fn delete_proceeds_when_the_referrer_goes_too() {
    let dir = store();
    let output = iwe(dir.path(), &["delete", "--filter", "type: { $in: [claim, concept] }"]);
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert!(!dir.path().join("claims/cats.md").exists());
    assert!(!dir.path().join("concepts/cat.md").exists());
}
