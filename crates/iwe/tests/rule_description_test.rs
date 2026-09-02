// Knowledge-compositor M5 re-pass, AC6 (R14): a `mutable:` entry or the
// `deletable:` keyword may carry a `description`, and the rejection line
// IWE prints appends it verbatim -- so a schema (generated or hand-written)
// can say what WOULD be required, not only that the write was refused.
// Before this, the two rules were bare booleans and the message was fixed
// (`rule 'mutable: false', property '$content'`), which meant the
// compositor's override-mechanism message could not ride on the IWE side
// of a rejection at all.
//
// The bare-boolean form is unchanged in every respect: same parse, same
// rejection text, no trailing description.

use diwe::config::{Configuration, LibraryOptions, MarkdownOptions, Patterns, SchemaBinding};
use std::collections::HashMap;
use std::fs::{create_dir_all, read_to_string, write};
use std::path::Path;
use std::process::{Command, Output};
use tempfile::TempDir;

const CLEAN: &str = "# Reference\n\noriginal body\n";
const OVERRIDE: &str = "an explicit override via the deferred-override mechanism";
const UPSTREAM: &str = "the asset can only be updated in the package's own upstream repository";

#[test]
fn body_write_rejection_appends_the_mutable_entrys_description() {
    let temp = setup(&format!(
        "mutable:\n  $content:\n    mutable: false\n    description: {OVERRIDE:?}\n"
    ));
    let output = run(temp.path(), &["update", "-k", "notes/one", "--content", "# Reference\n\nchanged body\n"]);

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).expect("valid UTF-8 output");
    assert!(stderr.contains("notes/one"), "{stderr}");
    assert!(stderr.contains("rule 'mutable: false'"), "{stderr}");
    assert!(stderr.contains("'$content' (the document body)"), "{stderr}");
    assert!(
        stderr.contains(&format!("(the document body): {OVERRIDE}")),
        "the description must follow the fixed text; stderr was:\n{stderr}"
    );
    assert_eq!(read_to_string(temp.path().join("notes/one.md")).unwrap(), CLEAN);
}

#[test]
fn frontmatter_write_rejection_appends_the_description_too() {
    let temp = setup(&format!(
        "mutable:\n  archived:\n    mutable: false\n    description: {OVERRIDE:?}\n"
    ));
    write(
        temp.path().join("notes/one.md"),
        "---\narchived: false\n---\n\n# Reference\n\noriginal body\n",
    )
    .unwrap();
    let output = run(
        temp.path(),
        &["update", "-k", "notes/one", "--content", "---\narchived: true\n---\n\n# Reference\n\noriginal body\n"],
    );

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).expect("valid UTF-8 output");
    assert!(stderr.contains(&format!("property 'archived': {OVERRIDE}")), "{stderr}");
}

#[test]
fn delete_rejection_appends_the_deletable_keywords_description() {
    let temp = setup(&format!(
        "deletable:\n  deletable: false\n  description: {UPSTREAM:?}\n"
    ));
    let output = run(temp.path(), &["delete", "-k", "notes/one"]);

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).expect("valid UTF-8 output");
    assert!(stderr.contains("rule 'deletable: false'"), "{stderr}");
    assert!(
        stderr.contains(&format!("(this document cannot be deleted): {UPSTREAM}")),
        "{stderr}"
    );
    assert_eq!(read_to_string(temp.path().join("notes/one.md")).unwrap(), CLEAN);
}

/// The bare-boolean forms keep their exact pre-existing rejection text --
/// nothing is appended when no description was given.
#[test]
fn bare_boolean_rules_reject_with_the_unchanged_text() {
    let temp = setup("mutable:\n  $content: false\ndeletable: false\n");

    let update = run(temp.path(), &["update", "-k", "notes/one", "--content", "# Reference\n\nchanged body\n"]);
    assert_eq!(update.status.code(), Some(1));
    let stderr = String::from_utf8(update.stderr).expect("valid UTF-8 output");
    assert!(
        stderr.lines().any(|line| line.ends_with("property '$content' (the document body)")),
        "{stderr}"
    );

    let delete = run(temp.path(), &["delete", "-k", "notes/one"]);
    assert_eq!(delete.status.code(), Some(1));
    let stderr = String::from_utf8(delete.stderr).expect("valid UTF-8 output");
    assert!(
        stderr.lines().any(|line| line.ends_with("(this document cannot be deleted)")),
        "{stderr}"
    );
}

/// `deletable: false` from any bound schema wins over another's explicit
/// `true`, and the description that travels is the refusing schema's.
#[test]
fn the_refusing_schemas_description_is_the_one_reported() {
    let temp = TempDir::new().expect("tempdir");
    create_dir_all(temp.path().join(".iwe/schemas")).unwrap();
    create_dir_all(temp.path().join("notes")).unwrap();
    let mut schemas = HashMap::new();
    schemas.extend(binding("a-permissive", "notes/**"));
    schemas.extend(binding("b-guard", "notes/**"));
    write_config(temp.path(), schemas);
    write(temp.path().join(".iwe/schemas/a-permissive.yaml"), "deletable: true\n").unwrap();
    write(
        temp.path().join(".iwe/schemas/b-guard.yaml"),
        format!("deletable:\n  deletable: false\n  description: {UPSTREAM:?}\n"),
    )
    .unwrap();
    write(temp.path().join("notes/one.md"), CLEAN).unwrap();

    let output = run(temp.path(), &["delete", "-k", "notes/one"]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).expect("valid UTF-8 output");
    assert!(stderr.contains(&format!("cannot be deleted): {UPSTREAM}")), "{stderr}");
    assert!(temp.path().join("notes/one.md").exists());
}

fn setup(schema_source: &str) -> TempDir {
    let temp = TempDir::new().expect("tempdir");
    create_dir_all(temp.path().join(".iwe/schemas")).unwrap();
    create_dir_all(temp.path().join("notes")).unwrap();
    write_config(temp.path(), binding("reference", "notes/**"));
    write(temp.path().join(".iwe/schemas/reference.yaml"), schema_source).unwrap();
    write(temp.path().join("notes/one.md"), CLEAN).unwrap();
    temp
}

fn binding(name: &str, pattern: &str) -> HashMap<String, SchemaBinding> {
    let mut schemas = HashMap::new();
    schemas.insert(
        name.to_string(),
        SchemaBinding {
            r#match: Patterns::One(pattern.to_string()),
        },
    );
    schemas
}

fn write_config(path: &Path, schemas: HashMap<String, SchemaBinding>) {
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
        ..Default::default()
    };
    write(path.join(".iwe/config.toml"), toml::to_string(&config).expect("config")).unwrap();
}

fn run(work_dir: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(crate::common::get_iwe_binary_path());
    command.current_dir(work_dir);
    for arg in args {
        command.arg(arg);
    }
    command.output().expect("run iwe")
}
