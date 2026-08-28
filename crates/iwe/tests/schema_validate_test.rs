use diwe::config::{Configuration, LibraryOptions, MarkdownOptions, Patterns, SchemaBinding};
use indoc::indoc;
use serde_json::json;
use std::collections::HashMap;
use std::fs::{create_dir_all, write};
use std::process::Command;
use tempfile::TempDir;

#[test]
fn validate_text_reports_violations_and_hint() {
    let temp_dir = setup_basic();
    let output = run_validate(&temp_dir, &[]);

    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).expect("Valid UTF-8 output");
    let expected = indoc! {"
        docs/one: required section \"Two\" is missing
          hint: keep two after one
        docs/one › Extra: unexpected section
    "};
    assert_eq!(stdout, expected);
}

#[test]
fn validate_stays_quiet_when_documents_were_validated() {
    let temp_dir = setup_basic();
    let output = run_validate(&temp_dir, &[]);

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).expect("Valid UTF-8 output");
    assert_eq!(stderr, "");
}

#[test]
fn validate_summary_makes_an_unbound_run_visible() {
    let temp_dir = setup_basic();
    write_config(temp_dir.path(), binding("alpha", "nowhere/**"));
    let output = run_validate(&temp_dir, &[]);

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).expect("Valid UTF-8 output");
    assert_eq!(stdout, "");
    let stderr = String::from_utf8(output.stderr).expect("Valid UTF-8 output");
    assert_eq!(stderr, "validated 0 document(s) against 0 schema(s)\n");
}

#[test]
fn validate_json_reports_violations() {
    let temp_dir = setup_basic();
    let output = run_validate(&temp_dir, &["-f", "json"]);

    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).expect("Valid UTF-8 output");
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("Valid JSON");
    assert_eq!(
        parsed,
        json!([
            {
                "key": "docs/one",
                "schema": "alpha",
                "violations": [
                    {
                        "breadcrumb": [],
                        "message": "required section \"Two\" is missing",
                        "hint": "keep two after one",
                        "schemaPath": "/sections/1/minContains",
                        "keyword": "minContains"
                    },
                    {
                        "breadcrumb": ["Extra"],
                        "message": "unexpected section",
                        "hint": null,
                        "schemaPath": "/additionalSections",
                        "keyword": "additionalSections"
                    }
                ]
            }
        ])
    );
}

#[test]
fn validate_clean_document_produces_no_output() {
    let temp_dir = setup_basic();
    let output = run_validate(&temp_dir, &["-k", "docs/clean"]);

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).expect("Valid UTF-8 output");
    assert_eq!(stdout, "");
}

#[test]
fn validate_unbound_document_produces_no_output() {
    let temp_dir = setup_basic();
    let output = run_validate(&temp_dir, &["-k", "other"]);

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).expect("Valid UTF-8 output");
    assert_eq!(stdout, "");
}

#[test]
fn validate_document_bound_to_two_schemas_reports_each() {
    let temp_dir = setup_two_schemas();
    let output = run_validate(&temp_dir, &["-f", "json"]);

    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).expect("Valid UTF-8 output");
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("Valid JSON");
    assert_eq!(
        parsed,
        json!([
            {
                "key": "docs/one",
                "schema": "alpha",
                "violations": [
                    {
                        "breadcrumb": [],
                        "message": "required section \"One\" is missing",
                        "hint": null,
                        "schemaPath": "/sections/0/minContains",
                        "keyword": "minContains"
                    }
                ]
            },
            {
                "key": "docs/one",
                "schema": "beta",
                "violations": [
                    {
                        "breadcrumb": [],
                        "message": "required section \"Two\" is missing",
                        "hint": null,
                        "schemaPath": "/sections/0/minContains",
                        "keyword": "minContains"
                    }
                ]
            }
        ])
    );
}

#[test]
fn validate_missing_schema_file_exits_two() {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let temp_path = temp_dir.path();
    create_dir_all(temp_path.join(".iwe/schemas")).unwrap();
    write_config(temp_path, binding("ghost", "docs/**"));
    create_dir_all(temp_path.join("docs")).unwrap();
    write(temp_path.join("docs/one.md"), "# Body\n").unwrap();

    let output = run_validate(&temp_dir, &[]);

    assert_eq!(output.status.code(), Some(2));
    let stdout = String::from_utf8(output.stdout).expect("Valid UTF-8 output");
    let stderr = String::from_utf8(output.stderr).expect("Valid UTF-8 output");
    assert_eq!(stdout, "");
    assert_eq!(
        stderr,
        "error: schema 'ghost': .iwe/schemas/ghost.yaml not found\n"
    );
}

#[test]
fn validate_uncompilable_schema_exits_two() {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let temp_path = temp_dir.path();
    create_dir_all(temp_path.join(".iwe/schemas")).unwrap();
    write_config(temp_path, binding("alpha", "docs/**"));
    write(
        temp_path.join(".iwe/schemas/alpha.yaml"),
        "sections:\n  - minContains: -1\n",
    )
    .unwrap();
    create_dir_all(temp_path.join("docs")).unwrap();
    write(temp_path.join("docs/one.md"), "# Body\n").unwrap();

    let output = run_validate(&temp_dir, &[]);

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).expect("Valid UTF-8 output");
    assert_eq!(
        stderr,
        "error: schema 'alpha' /sections/0/minContains: minContains must not be negative\n"
    );
}

#[test]
fn validate_without_schemas_prints_only_the_summary() {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let temp_path = temp_dir.path();
    create_dir_all(temp_path.join(".iwe")).unwrap();
    write_config(temp_path, HashMap::new());
    write(temp_path.join("other.md"), "# Body\n").unwrap();

    let output = run_validate(&temp_dir, &[]);

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).expect("Valid UTF-8 output");
    assert_eq!(stdout, "");
    let stderr = String::from_utf8(output.stderr).expect("Valid UTF-8 output");
    assert_eq!(stderr, "validated 0 document(s) against 0 schema(s)\n");
}

#[test]
fn validate_reports_block_violation() {
    let temp_dir = setup_blocks();
    let output = run_validate(&temp_dir, &[]);

    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).expect("Valid UTF-8 output");
    let expected = indoc! {"
        docs/one › Notes › blocks[1]: unexpected block
    "};
    assert_eq!(stdout, expected);
}

#[test]
fn validate_against_explicit_schema_file_bypasses_config() {
    let temp_dir = setup_explicit_schema();
    let output = run_validate(
        &temp_dir,
        &["-k", "docs/one", "--schema-file", "myschema.yaml"],
    );

    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).expect("Valid UTF-8 output");
    let expected = indoc! {"
        docs/one: required section \"Two\" is missing
        docs/one › Extra: unexpected section
    "};
    assert_eq!(stdout, expected);
}

#[test]
fn explain_prints_the_binding_trace() {
    let temp_dir = setup_explicit_schema();
    let output = run_validate(
        &temp_dir,
        &[
            "-k",
            "docs/one",
            "--schema-file",
            "myschema.yaml",
            "--explain",
        ],
    );

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).expect("Valid UTF-8 output");
    let expected = indoc! {"
        docs/one  [schema: myschema]
        # One  ->  sections[0]
          paragraph \"text\"  ->  additional
        # Extra  ->  additional

    "};
    assert_eq!(stdout, expected);
}

#[test]
fn validate_against_explicit_schema_file_json_uses_file_stem() {
    let temp_dir = setup_explicit_schema();
    let output = run_validate(
        &temp_dir,
        &[
            "-k",
            "docs/one",
            "--schema-file",
            "myschema.yaml",
            "-f",
            "json",
        ],
    );

    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).expect("Valid UTF-8 output");
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("Valid JSON");
    assert_eq!(
        parsed,
        json!([
            {
                "key": "docs/one",
                "schema": "myschema",
                "violations": [
                    {
                        "breadcrumb": [],
                        "message": "required section \"Two\" is missing",
                        "hint": null,
                        "schemaPath": "/sections/1/minContains",
                        "keyword": "minContains"
                    },
                    {
                        "breadcrumb": ["Extra"],
                        "message": "unexpected section",
                        "hint": null,
                        "schemaPath": "/additionalSections",
                        "keyword": "additionalSections"
                    }
                ]
            }
        ])
    );
}

#[test]
fn validate_against_missing_schema_file_exits_two() {
    let temp_dir = setup_explicit_schema();
    let output = run_validate(
        &temp_dir,
        &["-k", "docs/one", "--schema-file", "ghost.yaml"],
    );

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).expect("Valid UTF-8 output");
    assert_eq!(stderr, "error: schema file not found: ghost.yaml\n");
}

fn setup_explicit_schema() -> TempDir {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let temp_path = temp_dir.path();
    create_dir_all(temp_path.join(".iwe")).unwrap();
    create_dir_all(temp_path.join("docs")).unwrap();

    write_config(temp_path, HashMap::new());

    write(
        temp_path.join("myschema.yaml"),
        indoc! {"
            sections:
              - header: { const: One }
              - header: { const: Two }
            additionalSections: false
        "},
    )
    .unwrap();

    write(
        temp_path.join("docs/one.md"),
        indoc! {"
            # One

            text

            # Extra
        "},
    )
    .unwrap();

    temp_dir
}

fn setup_blocks() -> TempDir {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let temp_path = temp_dir.path();
    create_dir_all(temp_path.join(".iwe/schemas")).unwrap();
    create_dir_all(temp_path.join("docs")).unwrap();

    write_config(temp_path, binding("alpha", "docs/**"));

    write(
        temp_path.join(".iwe/schemas/alpha.yaml"),
        indoc! {"
            sections:
              - header: { const: Notes }
                blocks:
                  - type: paragraph
                additionalBlocks: false
        "},
    )
    .unwrap();

    write(
        temp_path.join("docs/one.md"),
        indoc! {"
            # Notes

            a paragraph

            - a list item
        "},
    )
    .unwrap();

    temp_dir
}

fn setup_basic() -> TempDir {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let temp_path = temp_dir.path();
    create_dir_all(temp_path.join(".iwe/schemas")).unwrap();
    create_dir_all(temp_path.join("docs")).unwrap();

    write_config(temp_path, binding("alpha", "docs/**"));

    write(
        temp_path.join(".iwe/schemas/alpha.yaml"),
        indoc! {"
            sections:
              - header: { const: One }
              - header: { const: Two }
                description: keep two after one
            additionalSections: false
        "},
    )
    .unwrap();

    write(
        temp_path.join("docs/one.md"),
        indoc! {"
            # One

            text

            # Extra
        "},
    )
    .unwrap();

    write(
        temp_path.join("docs/clean.md"),
        indoc! {"
            # One

            # Two
        "},
    )
    .unwrap();

    write(temp_path.join("other.md"), "# Body\n").unwrap();

    temp_dir
}

fn setup_two_schemas() -> TempDir {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let temp_path = temp_dir.path();
    create_dir_all(temp_path.join(".iwe/schemas")).unwrap();
    create_dir_all(temp_path.join("docs")).unwrap();

    let mut schemas = HashMap::new();
    schemas.insert(
        "alpha".to_string(),
        SchemaBinding {
            r#match: Patterns::One("docs/**".to_string()),
        },
    );
    schemas.insert(
        "beta".to_string(),
        SchemaBinding {
            r#match: Patterns::One("docs/**".to_string()),
        },
    );
    write_config(temp_path, schemas);

    write(
        temp_path.join(".iwe/schemas/alpha.yaml"),
        "sections:\n  - header: { const: One }\n",
    )
    .unwrap();
    write(
        temp_path.join(".iwe/schemas/beta.yaml"),
        "sections:\n  - header: { const: Two }\n",
    )
    .unwrap();

    write(temp_path.join("docs/one.md"), "# Extra\n").unwrap();

    temp_dir
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

fn write_config(path: &std::path::Path, schemas: HashMap<String, SchemaBinding>) {
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
    let config_content = toml::to_string(&config).expect("Failed to serialize config");
    write(path.join(".iwe/config.toml"), config_content).unwrap();
}

fn run_validate(temp_dir: &TempDir, args: &[&str]) -> std::process::Output {
    let binary_path = crate::common::get_iwe_binary_path();
    let mut cmd = Command::new(binary_path);
    cmd.current_dir(temp_dir.path())
        .arg("schema")
        .arg("validate");
    for arg in args {
        cmd.arg(arg);
    }
    cmd.output()
        .expect("Failed to execute schema validate command")
}

// ---- links rules (IWE extension) ----

fn setup_links() -> TempDir {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let temp_path = temp_dir.path();
    create_dir_all(temp_path.join(".iwe/schemas")).unwrap();
    create_dir_all(temp_path.join("concepts")).unwrap();
    create_dir_all(temp_path.join("facts")).unwrap();
    create_dir_all(temp_path.join("root")).unwrap();
    create_dir_all(temp_path.join("docs")).unwrap();

    let mut schemas = binding("concept", "concepts/**");
    schemas.extend(binding("fact", "facts/**"));
    write_config(temp_path, schemas);

    write(
        temp_path.join(".iwe/schemas/concept.yaml"),
        indoc! {"
            frontmatter:
              type: object
              properties:
                type: { const: concept }
            links:
              - within: Is a
                min: 1
                max: 1
                target: { type: concept }
                reach: root/entity
                description: the genus is one concept and leads to entity
        "},
    )
    .unwrap();
    write(
        temp_path.join(".iwe/schemas/fact.yaml"),
        indoc! {"
            links:
              - some: { type: concept }
                description: a fact is about a concept
        "},
    )
    .unwrap();

    let concept = |body: &str| format!("---\ntype: concept\n---\n\n{body}");
    write(temp_path.join("root/entity.md"), concept("# Entity\n")).unwrap();
    write(
        temp_path.join("concepts/thing.md"),
        concept("# Thing\n\n## Is a\n\n- [Entity](../root/entity)\n"),
    )
    .unwrap();
    write(
        temp_path.join("concepts/species.md"),
        concept("# Species\n\nA [thing](thing) of a kind.\n\n## Is a\n\n- [Thing](thing)\n"),
    )
    .unwrap();
    write(
        temp_path.join("concepts/bad-target.md"),
        concept("# Bad target\n\n## Is a\n\n- [Note](../docs/note)\n"),
    )
    .unwrap();
    write(
        temp_path.join("concepts/no-genus.md"),
        concept("# No genus\n\nprose\n"),
    )
    .unwrap();
    write(
        temp_path.join("concepts/two.md"),
        concept("# Two\n\n## Is a\n\n- [Thing](thing)\n- [Species](species)\n"),
    )
    .unwrap();
    write(
        temp_path.join("concepts/island.md"),
        concept("# Island\n\n## Is a\n\n- [Isle](isle)\n"),
    )
    .unwrap();
    write(
        temp_path.join("concepts/isle.md"),
        concept("# Isle\n\n## Is a\n\n- [Island](island)\n"),
    )
    .unwrap();
    write(
        temp_path.join("concepts/missing.md"),
        concept("# Missing\n\n## Is a\n\n- [Ghost](ghost)\n"),
    )
    .unwrap();
    write(
        temp_path.join("docs/note.md"),
        "---\ntype: note\n---\n\n# Note\n",
    )
    .unwrap();
    write(
        temp_path.join("facts/good.md"),
        "---\ntype: fact\n---\n\n# Good\n\nEvery [thing](../concepts/thing) has a kind.\n",
    )
    .unwrap();
    write(
        temp_path.join("facts/bare.md"),
        "---\ntype: fact\n---\n\n# Bare\n\nA claim about nothing defined, see [note](../docs/note).\n",
    )
    .unwrap();
    temp_dir
}

#[test]
fn links_rules_pass_for_well_formed_documents() {
    let temp_dir = setup_links();
    let output = run_validate(
        &temp_dir,
        &[
            "-k",
            "concepts/thing",
            "-k",
            "concepts/species",
            "-k",
            "facts/good",
        ],
    );
    let stdout = String::from_utf8(output.stdout).expect("Valid UTF-8 output");
    assert_eq!(stdout, "");
    assert_eq!(output.status.code(), Some(0));
}

#[test]
fn links_rules_report_count_target_reach_and_some_violations() {
    let temp_dir = setup_links();
    let output = run_validate(&temp_dir, &[]);
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).expect("Valid UTF-8 output");

    let expect = |line: &str| assert!(stdout.contains(line), "missing {line:?} in:\n{stdout}");
    expect("concepts/no-genus › Is a: 0 links within 'Is a', fewer than the minimum of 1");
    expect("  hint: the genus is one concept and leads to entity");
    expect("concepts/two › Is a: 2 links within 'Is a', greater than the maximum of 1");
    expect("concepts/bad-target › Is a: link to 'docs/note' within 'Is a' does not satisfy the target filter");
    expect("concepts/bad-target › Is a: no chain of links within 'Is a' reaches 'root/entity'");
    expect("concepts/island › Is a: no chain of links within 'Is a' reaches 'root/entity'");
    expect("concepts/isle › Is a: no chain of links within 'Is a' reaches 'root/entity'");
    expect("concepts/missing › Is a: link to 'concepts/ghost' within 'Is a': no such document");
    expect("facts/bare: no link satisfies the 'some' filter");
    expect("  hint: a fact is about a concept");

    for good in ["concepts/thing", "concepts/species", "facts/good"] {
        assert!(
            !stdout.contains(&format!("{good} ")),
            "{good} reported in:\n{stdout}"
        );
        assert!(
            !stdout.contains(&format!("{good}:")),
            "{good} reported in:\n{stdout}"
        );
    }
}

#[test]
fn links_rules_report_json_with_the_links_keyword() {
    let temp_dir = setup_links();
    let output = run_validate(&temp_dir, &["-f", "json", "-k", "concepts/no-genus"]);
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).expect("Valid UTF-8 output");
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("Valid JSON");
    assert_eq!(parsed[0]["schema"], "concept");
    assert_eq!(parsed[0]["violations"][0]["keyword"], "links");
    assert_eq!(parsed[0]["violations"][0]["schemaPath"], "/links/0");
    assert_eq!(parsed[0]["violations"][0]["breadcrumb"][0], "Is a");
}

#[test]
fn links_rule_errors_are_reported_at_load() {
    let temp_dir = setup_links();
    write(
        temp_dir.path().join(".iwe/schemas/fact.yaml"),
        indoc! {"
            links:
              - within: 3
              - min: 2
                max: 1
              - wobble: true
        "},
    )
    .unwrap();
    let output = run_validate(&temp_dir, &["-k", "facts/good"]);
    assert_ne!(output.status.code(), Some(0));
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        text.contains("links[0]: within: expected a section name or a block predicate"),
        "{text}"
    );
    assert!(text.contains("links[1]: min is greater than max"), "{text}");
    assert!(
        text.contains("links[2]: unknown keyword 'wobble'"),
        "{text}"
    );
}

#[test]
fn explain_ignores_links_rules() {
    let temp_dir = setup_links();
    let output = run_validate(&temp_dir, &["--explain", "-k", "concepts/thing"]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).expect("Valid UTF-8 output");
    assert!(
        stdout.contains("concepts/thing  [schema: concept]"),
        "{stdout}"
    );
}
