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

// ---- requires rules, $this anchors and [invariants] (IWE extensions) ----

fn write_config_with_invariants(
    path: &std::path::Path,
    schemas: HashMap<String, SchemaBinding>,
    invariants: HashMap<String, diwe::config::Invariant>,
) {
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
        invariants,
        ..Default::default()
    };
    let config_content = toml::to_string(&config).expect("Failed to serialize config");
    write(path.join(".iwe/config.toml"), config_content).unwrap();
}

fn setup_dialectic() -> TempDir {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let temp_path = temp_dir.path();
    create_dir_all(temp_path.join(".iwe/schemas")).unwrap();
    create_dir_all(temp_path.join("claims")).unwrap();
    create_dir_all(temp_path.join("facts")).unwrap();
    create_dir_all(temp_path.join("objections")).unwrap();

    let schemas = binding("objection", "objections/**");
    let mut invariants = HashMap::new();
    invariants.insert(
        "objection-past-stale".to_string(),
        diwe::config::Invariant {
            filter: "type: objection, state: open, stale_after: { $lt: $today }".to_string(),
            expect: toml::Value::Integer(0),
            description: Some("an open objection past its stale_after must be answered".into()),
        },
    );
    invariants.insert(
        "at-most-one-answered".to_string(),
        diwe::config::Invariant {
            filter: "type: objection, state: answered".to_string(),
            expect: toml::Value::String("{ $lte: 2 }".to_string()),
            description: None,
        },
    );
    write_config_with_invariants(temp_path, schemas, invariants);

    write(
        temp_path.join(".iwe/schemas/objection.yaml"),
        indoc! {"
            frontmatter:
              type: object
              properties:
                type: { const: objection }
            links:
              - within: Against
                min: 1
                max: 1
              - within: Undermines
                max: 1
                target: { $referencedBy: { match: { $key: $this.Against }, via: Rests on } }
                description: an undermined premise is one the attacked claim rests on
              - within: Against
                target: { $key: { $nin: [$this] } }
                description: an objection never attacks itself
            requires:
              - when: { kind: undermines }
                section: Undermines
                description: an undermining objection names the premise
              - when: { state: { $in: [answered, conceded] } }
                section: Answer
        "},
    )
    .unwrap();

    write(
        temp_path.join("facts/f.md"),
        "---\ntype: fact\n---\n\n# F\n\nA premise.\n",
    )
    .unwrap();
    write(
        temp_path.join("facts/g.md"),
        "---\ntype: fact\n---\n\n# G\n\nAnother premise.\n",
    )
    .unwrap();
    write(
        temp_path.join("claims/a.md"),
        "---\ntype: fact\n---\n\n# A\n\n## Rests on\n\n- [F](../facts/f)\n",
    )
    .unwrap();
    let objection = |kind: &str, state: &str, stale: &str, body: &str| {
        format!(
            "---\ntype: objection\nkind: {kind}\nstate: {state}\nstale_after: {stale}\n---\n\n# Objection\n\n{body}"
        )
    };
    write(
        temp_path.join("objections/good.md"),
        objection(
            "undermines",
            "open",
            "2999-01-01",
            "## Against\n\n- [A](../claims/a)\n\n## Undermines\n\n- [F](../facts/f)\n",
        ),
    )
    .unwrap();
    write(
        temp_path.join("objections/wrong-premise.md"),
        objection(
            "undermines",
            "open",
            "2999-01-01",
            "## Against\n\n- [A](../claims/a)\n\n## Undermines\n\n- [G](../facts/g)\n",
        ),
    )
    .unwrap();
    write(
        temp_path.join("objections/no-premise.md"),
        objection(
            "undermines",
            "open",
            "2999-01-01",
            "## Against\n\n- [A](../claims/a)\n",
        ),
    )
    .unwrap();
    write(
        temp_path.join("objections/silent-answer.md"),
        objection(
            "rebuts",
            "answered",
            "2999-01-01",
            "## Against\n\n- [A](../claims/a)\n",
        ),
    )
    .unwrap();
    write(
        temp_path.join("objections/answered.md"),
        objection(
            "rebuts",
            "answered",
            "2999-01-01",
            "## Against\n\n- [A](../claims/a)\n\n## Answer\n\nRevised.\n",
        ),
    )
    .unwrap();
    write(
        temp_path.join("objections/stale.md"),
        objection(
            "rebuts",
            "open",
            "2000-01-01",
            "## Against\n\n- [A](../claims/a)\n",
        ),
    )
    .unwrap();
    write(
        temp_path.join("objections/self.md"),
        objection(
            "rebuts",
            "open",
            "2999-01-01",
            "## Against\n\n- [Self](self)\n",
        ),
    )
    .unwrap();
    temp_dir
}

#[test]
fn requires_and_this_rules_pass_for_well_formed_documents() {
    let temp_dir = setup_dialectic();
    let output = run_validate(
        &temp_dir,
        &["-k", "objections/good", "-k", "objections/answered"],
    );
    let stdout = String::from_utf8(output.stdout).expect("Valid UTF-8 output");
    assert_eq!(stdout, "");
    assert_eq!(output.status.code(), Some(0));
}

#[test]
fn requires_reports_a_missing_conditional_section() {
    let temp_dir = setup_dialectic();
    let output = run_validate(
        &temp_dir,
        &[
            "-k",
            "objections/no-premise",
            "-k",
            "objections/silent-answer",
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).expect("Valid UTF-8 output");
    let expect = |line: &str| assert!(stdout.contains(line), "missing {line:?} in:\n{stdout}");
    expect("objections/no-premise › Undermines: required section \"Undermines\" is missing when { kind: undermines }");
    expect("  hint: an undermining objection names the premise");
    expect("objections/silent-answer › Answer: required section \"Answer\" is missing when { state: { $in: [answered, conceded] } }");
}

#[test]
fn this_anchors_resolve_against_the_validated_document() {
    let temp_dir = setup_dialectic();
    let output = run_validate(
        &temp_dir,
        &["-k", "objections/wrong-premise", "-k", "objections/self"],
    );
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).expect("Valid UTF-8 output");
    let expect = |line: &str| assert!(stdout.contains(line), "missing {line:?} in:\n{stdout}");
    expect("objections/wrong-premise › Undermines: link to 'facts/g' within 'Undermines' does not satisfy the target filter");
    expect("  hint: an undermined premise is one the attacked claim rests on");
    expect("objections/self › Against: link to 'objections/self' within 'Against' does not satisfy the target filter");
    expect("  hint: an objection never attacks itself");
}

#[test]
fn requires_reports_json_with_the_requires_keyword() {
    let temp_dir = setup_dialectic();
    let output = run_validate(&temp_dir, &["-f", "json", "-k", "objections/no-premise"]);
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).expect("Valid UTF-8 output");
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("Valid JSON");
    assert_eq!(parsed[0]["violations"][0]["keyword"], "requires");
    assert_eq!(parsed[0]["violations"][0]["schemaPath"], "/requires/0");
    assert_eq!(parsed[0]["violations"][0]["breadcrumb"][0], "Undermines");
}

#[test]
fn invariants_run_on_a_whole_graph_validation() {
    let temp_dir = setup_dialectic();
    let output = run_validate(&temp_dir, &[]);
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).expect("Valid UTF-8 output");
    let expect = |line: &str| assert!(stdout.contains(line), "missing {line:?} in:\n{stdout}");
    expect("invariants/objection-past-stale: 1 document matches, expected 0: objections/stale");
    expect("  hint: an open objection past its stale_after must be answered");
    assert!(
        !stdout.contains("at-most-one-answered"),
        "a satisfied invariant was reported:\n{stdout}"
    );
}

#[test]
fn invariants_are_skipped_when_validating_a_selection() {
    let temp_dir = setup_dialectic();
    let output = run_validate(&temp_dir, &["-k", "objections/stale"]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).expect("Valid UTF-8 output");
    assert_eq!(stdout, "");
}

#[test]
fn invariants_report_json_under_a_synthetic_key() {
    let temp_dir = setup_dialectic();
    let output = run_validate(&temp_dir, &["-f", "json"]);
    let stdout = String::from_utf8(output.stdout).expect("Valid UTF-8 output");
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("Valid JSON");
    let report = parsed
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["key"] == "invariants/objection-past-stale")
        .expect("invariant report present");
    assert_eq!(report["schema"], "config");
    assert_eq!(report["violations"][0]["keyword"], "invariants");
    assert_eq!(
        report["violations"][0]["schemaPath"],
        "/invariants/objection-past-stale"
    );
}

#[test]
fn malformed_invariants_and_requires_are_load_errors() {
    let temp_dir = setup_dialectic();
    let mut invariants = HashMap::new();
    invariants.insert(
        "broken".to_string(),
        diwe::config::Invariant {
            filter: "type: objection".to_string(),
            expect: toml::Value::String("{ $between: 3 }".to_string()),
            description: None,
        },
    );
    write_config_with_invariants(
        temp_dir.path(),
        binding("objection", "objections/**"),
        invariants,
    );
    let output = run_validate(&temp_dir, &[]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invariant 'broken': expect: unknown comparison '$between'"),
        "{stderr}"
    );

    write(
        temp_dir.path().join(".iwe/schemas/objection.yaml"),
        indoc! {"
            requires:
              - section: Answer
              - when: { state: answered }
              - when: { state: answered }
                section: Answer
                wobble: true
            links:
              - within: Undermines
                target: { $key: { $nin: $this.Against, $bogus: 1 } }
        "},
    )
    .unwrap();
    let output = run_validate(&temp_dir, &["-k", "objections/good"]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("requires[0]: missing 'when'"), "{stderr}");
    assert!(
        stderr.contains("requires[1]: missing 'section'"),
        "{stderr}"
    );
    assert!(
        stderr.contains("requires[2]: unknown keyword 'wobble'"),
        "{stderr}"
    );
    assert!(stderr.contains("links[0]: target:"), "{stderr}");
}

// ---- circular grounds: the objection rule and a $standing invariant ----

fn setup_circular() -> TempDir {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let temp_path = temp_dir.path();
    create_dir_all(temp_path.join(".iwe/schemas")).unwrap();
    create_dir_all(temp_path.join("claims")).unwrap();
    create_dir_all(temp_path.join("objections")).unwrap();
    create_dir_all(temp_path.join("disputes")).unwrap();

    let schemas = binding("objection", "objections/**");
    let mut invariants = HashMap::new();
    invariants.insert(
        "stance-not-defeated".to_string(),
        diwe::config::Invariant {
            filter: "type: stance, $standing: out".to_string(),
            expect: toml::Value::Integer(0),
            description: Some("a defeated stance is demoted or revised, not kept".into()),
        },
    );
    write_config_with_invariants(temp_path, schemas, invariants);

    write(
        temp_path.join(".iwe/schemas/objection.yaml"),
        indoc! {"
            frontmatter:
              type: object
              properties:
                type: { const: objection }
            links:
              - within: Against
                min: 1
                max: 1
              - within: Rests on
                target:
                  $key: { $nin: [$this.Against] }
                  $nor:
                    - $referencedBy:
                        via: Antithesis
                        match:
                          type: dispute
                          $references: { via: Thesis, match: { $key: $this.Against } }
                    - $referencedBy:
                        via: Thesis
                        match:
                          type: dispute
                          $references: { via: Antithesis, match: { $key: $this.Against } }
                description: an objection's ground is independent of the dispute it enters
        "},
    )
    .unwrap();

    let claim = |name: &str, kind: &str| {
        write(
            temp_path.join(format!("claims/{name}.md")),
            format!("---\ntype: {kind}\n---\n\n# {name}\n"),
        )
        .unwrap();
    };
    claim("t", "stance");
    claim("a", "conjecture");
    claim("e", "fact");
    write(
        temp_path.join("disputes/d.md"),
        "---\ntype: dispute\nstate: open\n---\n\n# D\n\n## Thesis\n\n- [T](../claims/t)\n\n## Antithesis\n\n- [A](../claims/a)\n",
    )
    .unwrap();
    let objection = |name: &str, against: &str, ground: &str| {
        write(
            temp_path.join(format!("objections/{name}.md")),
            format!(
                "---\ntype: objection\nkind: rebuts\nstate: open\n---\n\n# {name}\n\n## Against\n\n- [x](../claims/{against})\n\n## Rests on\n\n- [g](../claims/{ground})\n"
            ),
        )
        .unwrap();
    };
    objection("independent", "t", "e");
    objection("circular", "t", "a");
    objection("self-grounded", "a", "a");
    temp_dir
}

#[test]
fn an_objection_may_not_rest_on_the_other_side_of_its_dispute() {
    let temp_dir = setup_circular();
    let output = run_validate(
        &temp_dir,
        &[
            "-k",
            "objections/circular",
            "-k",
            "objections/self-grounded",
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).expect("Valid UTF-8 output");
    let expect = |line: &str| assert!(stdout.contains(line), "missing {line:?} in:\n{stdout}");
    expect("objections/circular › Rests on: link to 'claims/a' within 'Rests on' does not satisfy the target filter");
    expect("objections/self-grounded › Rests on: link to 'claims/a' within 'Rests on' does not satisfy the target filter");
    expect("  hint: an objection's ground is independent of the dispute it enters");
}

#[test]
fn an_independently_grounded_objection_passes_the_rule() {
    let temp_dir = setup_circular();
    let output = run_validate(&temp_dir, &["-k", "objections/independent"]);
    let stdout = String::from_utf8(output.stdout).expect("Valid UTF-8 output");
    assert_eq!(stdout, "");
    assert_eq!(output.status.code(), Some(0));
}

#[test]
fn invariants_can_filter_on_computed_standing() {
    let temp_dir = setup_circular();
    // The independent objection stands (its ground E is unattacked), so the
    // stance T is out: the $standing invariant trips on the whole graph.
    let output = run_validate(&temp_dir, &[]);
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).expect("Valid UTF-8 output");
    let expect = |line: &str| assert!(stdout.contains(line), "missing {line:?} in:\n{stdout}");
    expect("invariants/stance-not-defeated: 1 document matches, expected 0: claims/t");
    expect("  hint: a defeated stance is demoted or revised, not kept");
}

// ---- when: on links rules ----

fn setup_when() -> TempDir {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let temp_path = temp_dir.path();
    create_dir_all(temp_path.join(".iwe/schemas")).unwrap();
    create_dir_all(temp_path.join("claims")).unwrap();
    create_dir_all(temp_path.join("objections")).unwrap();
    write_config_with_invariants(
        temp_path,
        binding("objection", "objections/**"),
        HashMap::new(),
    );
    write(
        temp_path.join(".iwe/schemas/objection.yaml"),
        indoc! {"
            frontmatter:
              type: object
              properties:
                type: { const: objection }
            links:
              - when: { quantity: particular, kind: { $in: [rebuts, undermines] } }
                within: Against
                target: { quantity: { $in: [universal, particular] } }
                description: a particular denies a universal or another particular, never a generic
              - when: { mood: normative }
                within: Against
                target: { mood: normative }
                description: only an ought attacks an ought
              - when: { mood: descriptive }
                within: Against
                target: { mood: descriptive }
                description: an is does not refute an ought
        "},
    )
    .unwrap();
    let claim = |name: &str, quantity: &str, mood: &str| {
        write(
            temp_path.join(format!("claims/{name}.md")),
            format!("---\ntype: fact\nquantity: {quantity}\nmood: {mood}\n---\n\n# {name}\n"),
        )
        .unwrap();
    };
    claim("generic", "generic", "descriptive");
    claim("universal", "universal", "descriptive");
    claim("ought", "generic", "normative");
    let objection = |name: &str, kind: &str, quantity: &str, mood: &str, against: &str| {
        write(
            temp_path.join(format!("objections/{name}.md")),
            format!(
                "---\ntype: objection\nkind: {kind}\nstate: open\nquantity: {quantity}\nmood: {mood}\n---\n\n# {name}\n\n## Against\n\n- [x](../claims/{against})\n"
            ),
        )
        .unwrap();
    };
    objection(
        "edge-case",
        "rebuts",
        "particular",
        "descriptive",
        "generic",
    );
    objection(
        "counter-instance",
        "rebuts",
        "particular",
        "descriptive",
        "universal",
    );
    objection(
        "particular-undercut",
        "undercuts",
        "particular",
        "descriptive",
        "generic",
    );
    objection(
        "is-against-ought",
        "rebuts",
        "generic",
        "descriptive",
        "ought",
    );
    objection(
        "ought-against-ought",
        "rebuts",
        "generic",
        "normative",
        "ought",
    );
    temp_dir
}

#[test]
fn links_rules_with_when_apply_only_to_matching_documents() {
    let temp_dir = setup_when();
    let output = run_validate(
        &temp_dir,
        &[
            "-k",
            "objections/counter-instance",
            "-k",
            "objections/particular-undercut",
            "-k",
            "objections/ought-against-ought",
        ],
    );
    let stdout = String::from_utf8(output.stdout).expect("Valid UTF-8 output");
    assert_eq!(stdout, "");
    assert_eq!(output.status.code(), Some(0));

    let output = run_validate(
        &temp_dir,
        &[
            "-k",
            "objections/edge-case",
            "-k",
            "objections/is-against-ought",
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).expect("Valid UTF-8 output");
    let expect = |line: &str| assert!(stdout.contains(line), "missing {line:?} in:\n{stdout}");
    expect("objections/edge-case › Against: link to 'claims/generic' within 'Against' (when { quantity: particular, kind: { $in: [rebuts, undermines] } }) does not satisfy the target filter");
    expect("  hint: a particular denies a universal or another particular, never a generic");
    expect("objections/is-against-ought › Against: link to 'claims/ought' within 'Against' (when { mood: descriptive }) does not satisfy the target filter");
    expect("  hint: an is does not refute an ought");
    assert!(!stdout.contains("ought-against-ought"), "{stdout}");
}

// ---- $this.frontmatter.<path> anchors ----

#[test]
fn this_frontmatter_anchors_compare_the_documents_own_fields() {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let temp_path = temp_dir.path();
    create_dir_all(temp_path.join(".iwe/schemas")).unwrap();
    create_dir_all(temp_path.join("claims")).unwrap();
    create_dir_all(temp_path.join("objections")).unwrap();
    write_config_with_invariants(
        temp_path,
        binding("objection", "objections/**"),
        HashMap::new(),
    );
    write(
        temp_path.join(".iwe/schemas/objection.yaml"),
        indoc! {"
            frontmatter:
              type: object
              properties:
                type: { const: objection }
            links:
              - when: { kind: rebuts }
                within: Against
                target:
                  proposition.subject: $this.frontmatter.proposition.subject
                  proposition.predicate: $this.frontmatter.proposition.predicate
                  proposition.polarity: { $ne: $this.frontmatter.proposition.polarity }
                description: a rebuttal asserts the contrary of what it attacks
        "},
    )
    .unwrap();
    write(
        temp_path.join("claims/f.md"),
        "---\ntype: fact\nproposition:\n  subject: defect\n  predicate: scales-with\n  polarity: affirm\n---\n\n# F\n",
    )
    .unwrap();
    let objection = |name: &str, subject: &str, polarity: &str| {
        write(
            temp_path.join(format!("objections/{name}.md")),
            format!(
                "---\ntype: objection\nkind: rebuts\nproposition:\n  subject: {subject}\n  predicate: scales-with\n  polarity: {polarity}\n---\n\n# {name}\n\n## Against\n\n- [F](../claims/f)\n"
            ),
        )
        .unwrap();
    };
    objection("contrary", "defect", "deny");
    objection("about-something-else", "liability", "deny");
    objection("same-polarity", "defect", "affirm");

    let output = run_validate(&temp_dir, &["-k", "objections/contrary"]);
    let stdout = String::from_utf8(output.stdout).expect("Valid UTF-8 output");
    assert_eq!(stdout, "");
    assert_eq!(output.status.code(), Some(0));

    let output = run_validate(
        &temp_dir,
        &[
            "-k",
            "objections/about-something-else",
            "-k",
            "objections/same-polarity",
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).expect("Valid UTF-8 output");
    let expect = |line: &str| assert!(stdout.contains(line), "missing {line:?} in:\n{stdout}");
    expect("objections/about-something-else › Against: link to 'claims/f' within 'Against' (when { kind: rebuts }) does not satisfy the target filter");
    expect("objections/same-polarity › Against: link to 'claims/f' within 'Against' (when { kind: rebuts }) does not satisfy the target filter");
    expect("  hint: a rebuttal asserts the contrary of what it attacks");
}

// ---- asserts rules ----

#[test]
fn asserts_compare_a_documents_own_fields() {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let temp_path = temp_dir.path();
    create_dir_all(temp_path.join(".iwe/schemas")).unwrap();
    create_dir_all(temp_path.join("disputes")).unwrap();
    write_config_with_invariants(temp_path, binding("dispute", "disputes/**"), HashMap::new());
    write(
        temp_path.join(".iwe/schemas/dispute.yaml"),
        indoc! {"
            frontmatter:
              type: object
              properties:
                type: { const: dispute }
            asserts:
              - that: { stale_after: { $gt: $this.frontmatter.opened_at } }
                description: a dispute goes stale after it opens, not before
        "},
    )
    .unwrap();
    write(
        temp_path.join("disputes/sane.md"),
        "---\ntype: dispute\nopened_at: 2026-08-29\nstale_after: 2027-02-28\n---\n\n# Sane\n",
    )
    .unwrap();
    write(
        temp_path.join("disputes/backwards.md"),
        "---\ntype: dispute\nopened_at: 2026-08-29\nstale_after: 2026-01-01\n---\n\n# Backwards\n",
    )
    .unwrap();
    let output = run_validate(&temp_dir, &["-k", "disputes/sane"]);
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "");
    assert_eq!(output.status.code(), Some(0));
    let output = run_validate(&temp_dir, &["-k", "disputes/backwards"]);
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).expect("Valid UTF-8 output");
    let expect = |line: &str| assert!(stdout.contains(line), "missing {line:?} in:\n{stdout}");
    expect("disputes/backwards: assertion fails: { stale_after: { $gt: $this.frontmatter.opened_at } }");
    expect("  hint: a dispute goes stale after it opens, not before");
}
