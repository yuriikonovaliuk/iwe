use std::collections::{HashMap, HashSet};
use std::fs::read_to_string;
use std::path::Path;

use globset::{GlobBuilder, GlobMatcher};
use serde::ser::{Serialize, SerializeStruct, Serializer};

use liwe::graph::Graph;
use liwe::markdown::MarkdownReader;
use liwe::model::Key;
use liwe::operations::Changes;
use liwe::query::block::{parse_block_predicate, BlockPredicate};
use liwe::query::block_eval::BlockIndex;
use liwe::query::{build_filter_value, evaluate, Filter, ViaWalk};
use liwe::schema::{build_document, compile_schema, CompiledSchema, Crumb, Violation};

use crate::config::{schemas_dir, Configuration, SchemaBinding};
use crate::tokens::count_tokens;

#[derive(Debug)]
pub struct SchemaBindings {
    rules: Vec<(String, Vec<(GlobMatcher, bool)>)>,
}

impl SchemaBindings {
    pub fn compile(schemas: &HashMap<String, SchemaBinding>) -> Result<Self, Vec<String>> {
        let mut names: Vec<&String> = schemas.keys().collect();
        names.sort();

        let mut rules = Vec::new();
        let mut errors = Vec::new();

        for name in names {
            let patterns = compile_patterns(name, schemas[name].r#match.as_slice(), &mut errors);
            rules.push((name.clone(), patterns));
        }

        if errors.is_empty() {
            Ok(SchemaBindings { rules })
        } else {
            Err(errors)
        }
    }

    pub fn schemas_for(&self, key: &str) -> Vec<&str> {
        self.rules
            .iter()
            .filter(|(_, patterns)| {
                patterns.iter().fold(false, |bound, (matcher, negated)| {
                    if matcher.is_match(key) {
                        !negated
                    } else {
                        bound
                    }
                })
            })
            .map(|(name, _)| name.as_str())
            .collect()
    }
}

fn compile_patterns(
    name: &str,
    patterns: &[String],
    errors: &mut Vec<String>,
) -> Vec<(GlobMatcher, bool)> {
    let mut matchers = Vec::new();
    for pattern in patterns {
        let (body, negated) = if let Some(rest) = pattern.strip_prefix('!') {
            (rest, true)
        } else if pattern.starts_with("\\!") {
            (&pattern[1..], false)
        } else {
            (pattern.as_str(), false)
        };
        let anchored = body.strip_prefix('/').unwrap_or(body);
        match GlobBuilder::new(anchored).literal_separator(true).build() {
            Ok(glob) => matchers.push((glob.compile_matcher(), negated)),
            Err(error) => errors.push(format!(
                "schema '{name}': invalid pattern '{pattern}': {error}"
            )),
        }
    }
    matchers
}

#[derive(Debug)]
pub struct KeyReport {
    pub key: Key,
    pub schema: String,
    pub violations: Vec<Violation>,
}

impl Serialize for KeyReport {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("KeyReport", 3)?;
        state.serialize_field("key", &self.key.to_string())?;
        state.serialize_field("schema", &self.schema)?;
        state.serialize_field("violations", &self.violations)?;
        state.end()
    }
}

/// One entry of a schema's `links` list — an IWE extension to the document
/// schema language, checked against the graph rather than the document alone:
/// which section's links are in scope, how many distinct targets there may
/// be, what every target (or at least one) must satisfy, and which document
/// the scoped links must reach transitively.
#[derive(Debug, Clone)]
pub struct LinkRule {
    within: Option<BlockPredicate>,
    within_label: Option<String>,
    min: u64,
    max: Option<u64>,
    target: Option<Filter>,
    some: Option<Filter>,
    reach: Option<Key>,
    description: Option<String>,
}

/// A schema file compiled in two halves: the document-schema part (handled by
/// the validator crate) and the graph-level `links` rules IWE checks itself.
pub struct CompiledSchemaSet {
    pub schema: CompiledSchema,
    pub links: Vec<LinkRule>,
}

/// Split a schema source into the part the document validator understands
/// and the `links` rules, which it does not. A source without `links` is
/// passed through untouched.
pub fn split_links(source: &str) -> Result<(String, Vec<LinkRule>), Vec<String>> {
    let mut value: serde_yaml::Value = match serde_yaml::from_str(source) {
        Ok(value) => value,
        Err(_) => return Ok((source.to_string(), Vec::new())),
    };
    let mapping = match value.as_mapping_mut() {
        Some(mapping) => mapping,
        None => return Ok((source.to_string(), Vec::new())),
    };
    let links = match mapping.remove(serde_yaml::Value::String("links".to_string())) {
        Some(links) => links,
        None => return Ok((source.to_string(), Vec::new())),
    };
    let rules = parse_link_rules(&links)?;
    let rest = serde_yaml::to_string(&value)
        .map_err(|error| vec![format!("links: cannot re-serialize schema: {error}")])?;
    Ok((rest, rules))
}

fn parse_link_rules(value: &serde_yaml::Value) -> Result<Vec<LinkRule>, Vec<String>> {
    let entries = match value {
        serde_yaml::Value::Sequence(entries) => entries,
        _ => return Err(vec!["links: expected a list of rules".to_string()]),
    };
    let mut rules = Vec::new();
    let mut errors = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        match parse_link_rule(entry) {
            Ok(rule) => rules.push(rule),
            Err(message) => errors.push(format!("links[{index}]: {message}")),
        }
    }
    if errors.is_empty() {
        Ok(rules)
    } else {
        Err(errors)
    }
}

fn parse_link_rule(entry: &serde_yaml::Value) -> Result<LinkRule, String> {
    use serde_yaml::Value;
    let mapping = entry.as_mapping().ok_or("expected a mapping")?;
    let mut rule = LinkRule {
        within: None,
        within_label: None,
        min: 0,
        max: None,
        target: None,
        some: None,
        reach: None,
        description: None,
    };
    for (key, value) in mapping {
        let keyword = key.as_str().ok_or("keys must be strings")?;
        match keyword {
            "within" => match value {
                Value::String(section) => {
                    rule.within = Some(BlockPredicate::empty().within_section(section));
                    rule.within_label = Some(section.clone());
                }
                Value::Mapping(_) => {
                    rule.within = Some(
                        parse_block_predicate(value, "within")
                            .map_err(|error| format!("within: {error}"))?,
                    );
                }
                _ => return Err("within: expected a section name or a block predicate".into()),
            },
            "min" => rule.min = non_negative(value, "min")?,
            "max" => rule.max = Some(non_negative(value, "max")?),
            "target" => {
                rule.target =
                    Some(build_filter_value(value).map_err(|error| format!("target: {error}"))?)
            }
            "some" => {
                rule.some =
                    Some(build_filter_value(value).map_err(|error| format!("some: {error}"))?)
            }
            "reach" => {
                rule.reach = Some(Key::name(
                    value.as_str().ok_or("reach: expected a document key")?,
                ))
            }
            "description" => {
                rule.description = Some(
                    value
                        .as_str()
                        .ok_or("description: expected a string")?
                        .to_string(),
                )
            }
            other => return Err(format!("unknown keyword '{other}'")),
        }
    }
    if let (Some(min), Some(max)) = (Some(rule.min), rule.max) {
        if min > max {
            return Err("min is greater than max".into());
        }
    }
    Ok(rule)
}

fn non_negative(value: &serde_yaml::Value, keyword: &str) -> Result<u64, String> {
    value
        .as_u64()
        .ok_or_else(|| format!("{keyword}: expected a non-negative integer"))
}

/// Cache of evaluated `target`/`some` filters, keyed by schema name, rule
/// index and which of the two filters — each is evaluated once per run.
type FilterCache = HashMap<(String, usize, &'static str), HashSet<Key>>;

fn filter_set<'c>(
    cache: &'c mut FilterCache,
    schema: &str,
    index: usize,
    which: &'static str,
    filter: &Filter,
    graph: &Graph,
) -> &'c HashSet<Key> {
    cache
        .entry((schema.to_string(), index, which))
        .or_insert_with(|| evaluate(filter, graph).into_iter().collect())
}

/// Check one document against a schema's `links` rules. With `full` false
/// (a partial graph, as when validating unsaved buffers) only the counts are
/// checked — whether a target exists or what it is cannot be known.
fn check_links(
    graph: &Graph,
    key: &Key,
    schema: &str,
    rules: &[LinkRule],
    full: bool,
    cache: &mut FilterCache,
) -> Vec<Violation> {
    if rules.is_empty() {
        return Vec::new();
    }
    let index = BlockIndex::build(graph, key);
    let mut violations = Vec::new();
    for (i, rule) in rules.iter().enumerate() {
        let scope = rule.within.clone().unwrap_or_default();
        let targets = index.targets_within(&scope);
        let where_ = match &rule.within_label {
            Some(label) => format!(" within '{label}'"),
            None => String::new(),
        };
        let mut report = |message: String| {
            violations.push(Violation {
                breadcrumb: rule
                    .within_label
                    .as_ref()
                    .map(|label| vec![Crumb::Header(label.clone())])
                    .unwrap_or_default(),
                message,
                hint: rule.description.clone(),
                schema_pointer: format!("/links/{i}"),
                keyword: "links".to_string(),
            })
        };
        let count = targets.len() as u64;
        if count < rule.min {
            report(format!(
                "{count} link{}{where_}, fewer than the minimum of {}",
                if count == 1 { "" } else { "s" },
                rule.min
            ));
        }
        if let Some(max) = rule.max {
            if count > max {
                report(format!(
                    "{count} links{where_}, greater than the maximum of {max}"
                ));
            }
        }
        if !full {
            continue;
        }
        if let Some(target) = &rule.target {
            let allowed = filter_set(cache, schema, i, "target", target, graph);
            for t in &targets {
                if graph.maybe_key(t).is_none() {
                    report(format!("link to '{t}'{where_}: no such document"));
                } else if !allowed.contains(t) {
                    report(format!(
                        "link to '{t}'{where_} does not satisfy the target filter"
                    ));
                }
            }
        }
        if let Some(some) = &rule.some {
            let allowed = filter_set(cache, schema, i, "some", some, graph);
            if !targets.iter().any(|t| allowed.contains(t)) {
                report(format!("no link{where_} satisfies the 'some' filter"));
            }
        }
        if let Some(reach) = &rule.reach {
            if key != reach {
                let walk = ViaWalk::new(graph, &scope).outbound(key, u32::MAX);
                if !walk.contains_key(reach) {
                    report(format!("no chain of links{where_} reaches '{reach}'"));
                }
            }
        }
    }
    violations
}

#[derive(Debug)]
pub struct ValidationRun {
    pub reports: Vec<KeyReport>,
    pub documents: usize,
    pub schemas: usize,
}

pub fn validate_documents(
    config: &Configuration,
    graph: &Graph,
    keys: &[Key],
) -> Result<ValidationRun, Vec<String>> {
    let dir = schemas_dir().map_err(|error| vec![error])?;
    validate_documents_in(&dir, config, graph, keys, true)
}

pub fn validate_documents_against_file(
    graph: &Graph,
    keys: &[Key],
    schema_path: &Path,
) -> Result<ValidationRun, Vec<String>> {
    let label = schema_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("schema")
        .to_string();

    let source = read_to_string(schema_path)
        .map_err(|_| vec![format!("schema file not found: {}", schema_path.display())])?;
    let (source, links) = split_links(&source).map_err(|errors| {
        errors
            .into_iter()
            .map(|e| format!("schema '{label}' {e}"))
            .collect::<Vec<_>>()
    })?;

    let compiled = compile_schema(&source).map_err(|schema_errors| {
        schema_errors
            .into_iter()
            .map(|error| {
                if error.pointer.is_empty() {
                    format!("schema '{label}': {}", error.message)
                } else {
                    format!("schema '{label}' {}: {}", error.pointer, error.message)
                }
            })
            .collect::<Vec<_>>()
    })?;

    let mut reports = Vec::new();
    let mut cache = FilterCache::new();
    for key in keys {
        let document = build_document(graph, key, count_tokens);
        let mut violations = compiled.validate(&document);
        violations.extend(check_links(graph, key, &label, &links, true, &mut cache));
        if !violations.is_empty() {
            reports.push(KeyReport {
                key: key.clone(),
                schema: label.clone(),
                violations,
            });
        }
    }
    Ok(ValidationRun {
        reports,
        documents: keys.len(),
        schemas: 1,
    })
}

pub fn explain_documents(
    config: &Configuration,
    graph: &Graph,
    keys: &[Key],
) -> Result<String, Vec<String>> {
    let dir = schemas_dir().map_err(|error| vec![error])?;
    let bindings = SchemaBindings::compile(&config.schemas)?;
    let compiled = compile_schemas(&dir, &config.schemas)?;

    let mut out = String::new();
    for key in keys {
        let names = bindings.schemas_for(&key.to_string());
        if names.is_empty() {
            out.push_str(&format!("{key}  (no schema)\n\n"));
            continue;
        }
        let document = build_document(graph, key, count_tokens);
        for name in names {
            out.push_str(&format!("{key}  [schema: {name}]\n"));
            out.push_str(&compiled[name].schema.explain(&document));
            out.push('\n');
        }
    }
    Ok(out)
}

pub fn explain_documents_against_file(
    graph: &Graph,
    keys: &[Key],
    schema_path: &Path,
) -> Result<String, Vec<String>> {
    let label = schema_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("schema")
        .to_string();

    let source = read_to_string(schema_path)
        .map_err(|_| vec![format!("schema file not found: {}", schema_path.display())])?;
    let (source, _links) = split_links(&source).map_err(|errors| {
        errors
            .into_iter()
            .map(|e| format!("schema '{label}' {e}"))
            .collect::<Vec<_>>()
    })?;

    let compiled = compile_schema(&source).map_err(|schema_errors| {
        schema_errors
            .into_iter()
            .map(|error| {
                if error.pointer.is_empty() {
                    format!("schema '{label}': {}", error.message)
                } else {
                    format!("schema '{label}' {}: {}", error.pointer, error.message)
                }
            })
            .collect::<Vec<_>>()
    })?;

    let mut out = String::new();
    for key in keys {
        let document = build_document(graph, key, count_tokens);
        out.push_str(&format!("{key}  [schema: {label}]\n"));
        out.push_str(&compiled.explain(&document));
        out.push('\n');
    }
    Ok(out)
}

pub fn render_reports_text(reports: &[KeyReport]) -> String {
    let mut out = String::new();
    for report in reports {
        for violation in &report.violations {
            let breadcrumb = violation.breadcrumb_text();
            if breadcrumb.is_empty() {
                out.push_str(&format!("{}: {}\n", report.key, violation.message));
            } else {
                out.push_str(&format!(
                    "{} › {}: {}\n",
                    report.key, breadcrumb, violation.message
                ));
            }
            if let Some(hint) = &violation.hint {
                out.push_str(&format!("  hint: {}\n", hint));
            }
        }
    }
    out
}

pub fn pending_from_changes(changes: &Changes) -> Vec<(Key, String)> {
    changes
        .creates
        .iter()
        .chain(changes.updates.iter())
        .cloned()
        .collect()
}

pub fn validate_pending_documents(
    config: &Configuration,
    docs: &[(Key, String)],
) -> Result<ValidationRun, Vec<String>> {
    let dir = schemas_dir().map_err(|error| vec![error])?;
    validate_pending_documents_in(&dir, config, docs)
}

pub fn validate_pending_documents_in(
    dir: &Path,
    config: &Configuration,
    docs: &[(Key, String)],
) -> Result<ValidationRun, Vec<String>> {
    let mut graph = Graph::new_with_options(config.format_options());
    for (key, content) in docs {
        graph.from_markdown(key.clone(), content, MarkdownReader::new());
    }
    let keys: Vec<Key> = docs.iter().map(|(key, _)| key.clone()).collect();
    validate_documents_in(dir, config, &graph, &keys, false)
}

fn validate_documents_in(
    dir: &Path,
    config: &Configuration,
    graph: &Graph,
    keys: &[Key],
    full: bool,
) -> Result<ValidationRun, Vec<String>> {
    let bindings = SchemaBindings::compile(&config.schemas)?;
    let compiled = compile_schemas(dir, &config.schemas)?;

    let mut reports = Vec::new();
    let mut documents = 0;
    let mut schemas_used = HashSet::new();
    let mut cache = FilterCache::new();
    for key in keys {
        let names = bindings.schemas_for(&key.to_string());
        if names.is_empty() {
            continue;
        }
        documents += 1;
        let document = build_document(graph, key, count_tokens);
        for name in names {
            schemas_used.insert(name);
            let set = &compiled[name];
            let mut violations = set.schema.validate(&document);
            violations.extend(check_links(graph, key, name, &set.links, full, &mut cache));
            if !violations.is_empty() {
                reports.push(KeyReport {
                    key: key.clone(),
                    schema: name.to_string(),
                    violations,
                });
            }
        }
    }
    Ok(ValidationRun {
        reports,
        documents,
        schemas: schemas_used.len(),
    })
}

fn compile_schemas(
    dir: &Path,
    schemas: &HashMap<String, SchemaBinding>,
) -> Result<HashMap<String, CompiledSchemaSet>, Vec<String>> {
    let mut names: Vec<&String> = schemas.keys().collect();
    names.sort();

    let mut compiled = HashMap::new();
    let mut errors = Vec::new();

    for name in names {
        let path = dir.join(format!("{name}.yaml"));
        let source = match read_to_string(&path) {
            Ok(source) => source,
            Err(_) => {
                errors.push(format!(
                    "schema '{name}': .iwe/schemas/{name}.yaml not found"
                ));
                continue;
            }
        };
        let (source, links) = match split_links(&source) {
            Ok(split) => split,
            Err(link_errors) => {
                for error in link_errors {
                    errors.push(format!("schema '{name}' {error}"));
                }
                continue;
            }
        };
        match compile_schema(&source) {
            Ok(schema) => {
                compiled.insert(name.clone(), CompiledSchemaSet { schema, links });
            }
            Err(schema_errors) => {
                for error in schema_errors {
                    if error.pointer.is_empty() {
                        errors.push(format!("schema '{name}': {}", error.message));
                    } else {
                        errors.push(format!(
                            "schema '{name}' {}: {}",
                            error.pointer, error.message
                        ));
                    }
                }
            }
        }
    }

    if errors.is_empty() {
        Ok(compiled)
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs::{create_dir_all, write};

    use liwe::schema::Crumb;
    use tempfile::TempDir;

    use crate::config::Patterns;

    fn bindings(entries: &[(&str, Patterns)]) -> SchemaBindings {
        let schemas = entries
            .iter()
            .map(|(name, patterns)| {
                (
                    name.to_string(),
                    SchemaBinding {
                        r#match: patterns.clone(),
                    },
                )
            })
            .collect();
        SchemaBindings::compile(&schemas).expect("compiles")
    }

    #[test]
    fn negated_patterns_unbind_keys_earlier_patterns_matched() {
        let bindings = bindings(&[(
            "note",
            Patterns::Many(vec![
                "data/**".to_string(),
                "!data/index".to_string(),
                "!data/**/index".to_string(),
                "!data/log".to_string(),
                "!data/**/log".to_string(),
            ]),
        )]);
        assert_eq!(bindings.schemas_for("data/product"), vec!["note"]);
        assert_eq!(bindings.schemas_for("data/people/alice"), vec!["note"]);
        assert_eq!(bindings.schemas_for("data/index"), Vec::<&str>::new());
        assert_eq!(bindings.schemas_for("data/log"), Vec::<&str>::new());
        assert_eq!(
            bindings.schemas_for("data/people/index"),
            Vec::<&str>::new()
        );
        assert_eq!(bindings.schemas_for("data/people/log"), Vec::<&str>::new());
    }

    #[test]
    fn negation_narrows_by_prefix_within_a_directory() {
        let bindings = bindings(&[(
            "person",
            Patterns::Many(vec!["people/**".to_string(), "!people/role-*".to_string()]),
        )]);
        assert_eq!(bindings.schemas_for("people/alice"), vec!["person"]);
        assert_eq!(bindings.schemas_for("people/rita"), vec!["person"]);
        assert_eq!(
            bindings.schemas_for("people/role-contact"),
            Vec::<&str>::new()
        );
    }

    #[test]
    fn last_matching_pattern_wins_so_a_later_pattern_reincludes() {
        let bindings = bindings(&[(
            "note",
            Patterns::Many(vec![
                "**".to_string(),
                "!drafts/**".to_string(),
                "drafts/keep".to_string(),
            ]),
        )]);
        assert_eq!(bindings.schemas_for("pages/one"), vec!["note"]);
        assert_eq!(bindings.schemas_for("drafts/scratch"), Vec::<&str>::new());
        assert_eq!(bindings.schemas_for("drafts/keep"), vec!["note"]);
    }

    #[test]
    fn escaped_leading_bang_is_a_literal_pattern() {
        let bindings = bindings(&[("note", Patterns::One("\\!special".to_string()))]);
        assert_eq!(bindings.schemas_for("!special"), vec!["note"]);
        assert_eq!(bindings.schemas_for("special"), Vec::<&str>::new());
    }

    #[test]
    fn invalid_negated_patterns_report_with_their_bang_prefix() {
        let schemas = HashMap::from([(
            "broken".to_string(),
            SchemaBinding {
                r#match: Patterns::Many(vec!["data/**".to_string(), "!data/[".to_string()]),
            },
        )]);
        let errors = SchemaBindings::compile(&schemas).unwrap_err();
        assert_eq!(
            errors,
            vec![
                "schema 'broken': invalid pattern '!data/[': error parsing glob 'data/[': unclosed character class; missing ']'".to_string(),
            ]
        );
    }

    #[test]
    fn negated_patterns_round_trip_through_toml() {
        let source = "\
[schemas.note]
match = [\"data/**\", \"!data/index\"]
";
        let config: Configuration = toml::from_str(source).expect("parses");
        assert_eq!(
            config.schemas["note"],
            SchemaBinding {
                r#match: Patterns::Many(vec!["data/**".to_string(), "!data/index".to_string()]),
            }
        );

        let rendered = toml::to_string(&config).expect("serializes");
        let reparsed: Configuration = toml::from_str(&rendered).expect("reparses");
        assert_eq!(reparsed.schemas, config.schemas);
    }

    #[test]
    fn single_glob_matches_by_prefix() {
        let bindings = bindings(&[("person", Patterns::One("people/**".to_string()))]);
        assert_eq!(bindings.schemas_for("people/alice"), vec!["person"]);
        assert_eq!(bindings.schemas_for("teams/core"), Vec::<&str>::new());
    }

    #[test]
    fn list_form_matches_any_pattern() {
        let bindings = bindings(&[(
            "session",
            Patterns::Many(vec!["journal/*".to_string(), "meetings/**".to_string()]),
        )]);
        assert_eq!(bindings.schemas_for("journal/monday"), vec!["session"]);
        assert_eq!(
            bindings.schemas_for("meetings/2026/standup"),
            vec!["session"]
        );
        assert_eq!(
            bindings.schemas_for("journal/2026/monday"),
            Vec::<&str>::new()
        );
    }

    #[test]
    fn single_star_stops_at_separator_double_star_crosses() {
        let single = bindings(&[("one", Patterns::One("notes/*".to_string()))]);
        assert_eq!(single.schemas_for("notes/today"), vec!["one"]);
        assert_eq!(single.schemas_for("notes/2026/today"), Vec::<&str>::new());

        let double = bindings(&[("all", Patterns::One("notes/**".to_string()))]);
        assert_eq!(double.schemas_for("notes/today"), vec!["all"]);
        assert_eq!(double.schemas_for("notes/2026/today"), vec!["all"]);
    }

    #[test]
    fn leading_slash_in_pattern_is_stripped() {
        let bindings = bindings(&[("person", Patterns::One("/people/**".to_string()))]);
        assert_eq!(bindings.schemas_for("people/alice"), vec!["person"]);
    }

    #[test]
    fn overlapping_schemas_both_apply_sorted_by_name() {
        let bindings = bindings(&[
            ("zeta", Patterns::One("people/**".to_string())),
            ("alpha", Patterns::One("people/*".to_string())),
        ]);
        assert_eq!(bindings.schemas_for("people/alice"), vec!["alpha", "zeta"]);
    }

    #[test]
    fn invalid_globs_report_every_bad_pattern() {
        let schemas = HashMap::from([(
            "broken".to_string(),
            SchemaBinding {
                r#match: Patterns::Many(vec!["[".to_string(), "people/[".to_string()]),
            },
        )]);
        let errors = SchemaBindings::compile(&schemas).unwrap_err();
        assert_eq!(
            errors,
            vec![
                "schema 'broken': invalid pattern '[': error parsing glob '[': unclosed character class; missing ']'".to_string(),
                "schema 'broken': invalid pattern 'people/[': error parsing glob 'people/[': unclosed character class; missing ']'".to_string(),
            ]
        );
    }

    #[test]
    fn binding_round_trips_through_toml_as_string_and_list() {
        let source = "\
[schemas.person]
match = \"people/**\"

[schemas.session]
match = [\"journal/*\", \"meetings/**\"]
";
        let config: Configuration = toml::from_str(source).expect("parses");
        assert_eq!(
            config.schemas["person"],
            SchemaBinding {
                r#match: Patterns::One("people/**".to_string()),
            }
        );
        assert_eq!(
            config.schemas["session"],
            SchemaBinding {
                r#match: Patterns::Many(vec!["journal/*".to_string(), "meetings/**".to_string()]),
            }
        );

        let reparsed: Configuration =
            toml::from_str(&toml::to_string(&config).expect("serializes")).expect("reparses");
        assert_eq!(reparsed.schemas, config.schemas);
    }

    fn graph_with(entries: &[(&str, &str)]) -> Graph {
        let mut graph = Graph::new();
        for (key, content) in entries {
            graph.from_markdown(Key::name(key), content, MarkdownReader::new());
        }
        graph
    }

    fn config_with(entries: &[(&str, Patterns)]) -> Configuration {
        Configuration {
            schemas: entries
                .iter()
                .map(|(name, patterns)| {
                    (
                        name.to_string(),
                        SchemaBinding {
                            r#match: patterns.clone(),
                        },
                    )
                })
                .collect(),
            ..Default::default()
        }
    }

    fn write_schema(dir: &Path, name: &str, source: &str) {
        let schemas = dir.join(".iwe").join("schemas");
        create_dir_all(&schemas).unwrap();
        write(schemas.join(format!("{name}.yaml")), source).unwrap();
    }

    #[test]
    fn validate_documents_reports_per_schema_in_key_and_name_order() {
        let temp = TempDir::new().unwrap();
        write_schema(
            temp.path(),
            "person",
            "sections:\n  - header: { const: Summary }\n  - header: { const: Tasks }\n",
        );
        write_schema(
            temp.path(),
            "audited",
            "sections:\n  - header: { const: Review }\n",
        );

        let graph = graph_with(&[("people/alice", "# Summary\n"), ("teams/core", "# Team\n")]);
        let config = config_with(&[
            ("person", Patterns::One("people/**".to_string())),
            ("audited", Patterns::One("people/**".to_string())),
        ]);
        let keys = vec![Key::name("people/alice"), Key::name("teams/core")];

        let reports = validate_documents_in(
            temp.path().join(".iwe").join("schemas").as_path(),
            &config,
            &graph,
            &keys,
            true,
        )
        .expect("no config errors")
        .reports;

        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].key, Key::name("people/alice"));
        assert_eq!(reports[0].schema, "audited");
        assert_eq!(
            reports[0].violations,
            vec![Violation {
                breadcrumb: vec![],
                message: "required section \"Review\" is missing".to_string(),
                hint: None,
                schema_pointer: "/sections/0/minContains".to_string(),
                keyword: "minContains".to_string(),
            }]
        );
        assert_eq!(reports[1].key, Key::name("people/alice"));
        assert_eq!(reports[1].schema, "person");
        assert_eq!(
            reports[1].violations,
            vec![Violation {
                breadcrumb: vec![],
                message: "required section \"Tasks\" is missing".to_string(),
                hint: None,
                schema_pointer: "/sections/1/minContains".to_string(),
                keyword: "minContains".to_string(),
            }]
        );
    }

    #[test]
    fn validation_run_counts_bound_documents_and_distinct_schemas() {
        let temp = TempDir::new().unwrap();
        write_schema(
            temp.path(),
            "person",
            "sections:\n  - header: { const: Summary }\n",
        );
        write_schema(
            temp.path(),
            "audited",
            "sections:\n  - header: { const: Review }\n",
        );

        let graph = graph_with(&[("people/alice", "# Summary\n"), ("teams/core", "# Team\n")]);
        let config = config_with(&[
            ("person", Patterns::One("people/**".to_string())),
            ("audited", Patterns::One("people/**".to_string())),
        ]);
        let keys = vec![Key::name("people/alice"), Key::name("teams/core")];

        let run = validate_documents_in(
            temp.path().join(".iwe").join("schemas").as_path(),
            &config,
            &graph,
            &keys,
            true,
        )
        .expect("no config errors");

        assert_eq!(run.documents, 1);
        assert_eq!(run.schemas, 2);
    }

    #[test]
    fn validation_run_reports_zero_counts_when_nothing_is_bound() {
        let temp = TempDir::new().unwrap();
        write_schema(
            temp.path(),
            "person",
            "sections:\n  - header: { const: Summary }\n",
        );

        let graph = graph_with(&[("teams/core", "# Team\n")]);
        let config = config_with(&[("person", Patterns::One("people/**".to_string()))]);
        let keys = vec![Key::name("teams/core")];

        let run = validate_documents_in(
            temp.path().join(".iwe").join("schemas").as_path(),
            &config,
            &graph,
            &keys,
            true,
        )
        .expect("no config errors");

        assert!(run.reports.is_empty());
        assert_eq!(run.documents, 0);
        assert_eq!(run.schemas, 0);
    }

    #[test]
    fn clean_document_yields_no_report() {
        let temp = TempDir::new().unwrap();
        write_schema(
            temp.path(),
            "person",
            "sections:\n  - header: { const: Summary }\n",
        );
        let graph = graph_with(&[("people/alice", "# Summary\n\ntext\n")]);
        let config = config_with(&[("person", Patterns::One("people/**".to_string()))]);
        let keys = vec![Key::name("people/alice")];

        let reports = validate_documents_in(
            temp.path().join(".iwe").join("schemas").as_path(),
            &config,
            &graph,
            &keys,
            true,
        )
        .expect("no config errors")
        .reports;
        assert!(reports.is_empty());
    }

    #[test]
    fn nested_breadcrumb_survives_into_report() {
        let temp = TempDir::new().unwrap();
        write_schema(
            temp.path(),
            "person",
            "sections:\n  - header: { const: Summary }\nadditionalSections: false\n",
        );
        let graph = graph_with(&[("people/alice", "# Summary\n\n# Extra\n")]);
        let config = config_with(&[("person", Patterns::One("people/**".to_string()))]);
        let keys = vec![Key::name("people/alice")];

        let reports = validate_documents_in(
            temp.path().join(".iwe").join("schemas").as_path(),
            &config,
            &graph,
            &keys,
            true,
        )
        .expect("no config errors")
        .reports;
        assert_eq!(reports.len(), 1);
        assert_eq!(
            reports[0].violations,
            vec![Violation {
                breadcrumb: vec![Crumb::Header("Extra".to_string())],
                message: "unexpected section".to_string(),
                hint: None,
                schema_pointer: "/additionalSections".to_string(),
                keyword: "additionalSections".to_string(),
            }]
        );
    }

    #[test]
    fn validate_against_file_reports_under_the_file_stem() {
        let temp = TempDir::new().unwrap();
        let schema_path = temp.path().join("person.yaml");
        write(
            &schema_path,
            "sections:\n  - header: { const: Summary }\n  - header: { const: Tasks }\n",
        )
        .unwrap();
        let graph = graph_with(&[("people/alice", "# Summary\n")]);
        let keys = vec![Key::name("people/alice")];

        let reports = validate_documents_against_file(&graph, &keys, &schema_path)
            .expect("no schema errors")
            .reports;

        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].key, Key::name("people/alice"));
        assert_eq!(reports[0].schema, "person");
        assert_eq!(
            reports[0].violations,
            vec![Violation {
                breadcrumb: vec![],
                message: "required section \"Tasks\" is missing".to_string(),
                hint: None,
                schema_pointer: "/sections/1/minContains".to_string(),
                keyword: "minContains".to_string(),
            }]
        );
    }

    #[test]
    fn validate_against_file_passes_clean_document() {
        let temp = TempDir::new().unwrap();
        let schema_path = temp.path().join("person.yaml");
        write(&schema_path, "sections:\n  - header: { const: Summary }\n").unwrap();
        let graph = graph_with(&[("people/alice", "# Summary\n\ntext\n")]);
        let keys = vec![Key::name("people/alice")];

        let reports = validate_documents_against_file(&graph, &keys, &schema_path)
            .expect("no schema errors")
            .reports;
        assert!(reports.is_empty());
    }

    #[test]
    fn validate_against_missing_file_is_an_error() {
        let temp = TempDir::new().unwrap();
        let schema_path = temp.path().join("ghost.yaml");
        let graph = graph_with(&[("people/alice", "# Summary\n")]);
        let keys = vec![Key::name("people/alice")];

        let errors = validate_documents_against_file(&graph, &keys, &schema_path).unwrap_err();
        assert_eq!(
            errors,
            vec![format!("schema file not found: {}", schema_path.display())]
        );
    }

    #[test]
    fn validate_against_uncompilable_file_surfaces_schema_error() {
        let temp = TempDir::new().unwrap();
        let schema_path = temp.path().join("person.yaml");
        write(&schema_path, "sections:\n  - minContains: -1\n").unwrap();
        let graph = graph_with(&[("people/alice", "# Summary\n")]);
        let keys = vec![Key::name("people/alice")];

        let errors = validate_documents_against_file(&graph, &keys, &schema_path).unwrap_err();
        assert_eq!(
            errors,
            vec![
                "schema 'person' /sections/0/minContains: minContains must not be negative"
                    .to_string()
            ]
        );
    }

    #[test]
    fn block_violations_ride_the_key_report_path() {
        let temp = TempDir::new().unwrap();
        write_schema(
            temp.path(),
            "person",
            "sections:\n  - header: { const: Notes }\n    blocks:\n      - type: paragraph\n    additionalBlocks: false\n",
        );
        let graph = graph_with(&[("people/alice", "# Notes\n\na paragraph\n\n- a list item\n")]);
        let config = config_with(&[("person", Patterns::One("people/**".to_string()))]);
        let keys = vec![Key::name("people/alice")];

        let reports = validate_documents_in(
            temp.path().join(".iwe").join("schemas").as_path(),
            &config,
            &graph,
            &keys,
            true,
        )
        .expect("no config errors")
        .reports;

        assert_eq!(reports.len(), 1);
        assert_eq!(
            reports[0].violations,
            vec![Violation {
                breadcrumb: vec![Crumb::Header("Notes".to_string()), Crumb::Block(1)],
                message: "unexpected block".to_string(),
                hint: None,
                schema_pointer: "/sections/0/additionalBlocks".to_string(),
                keyword: "additionalBlocks".to_string(),
            }]
        );
    }

    #[test]
    fn missing_schema_file_is_a_config_error() {
        let temp = TempDir::new().unwrap();
        create_dir_all(temp.path().join(".iwe").join("schemas")).unwrap();
        let graph = graph_with(&[("people/alice", "# Summary\n")]);
        let config = config_with(&[("person", Patterns::One("people/**".to_string()))]);
        let keys = vec![Key::name("people/alice")];

        let errors = validate_documents_in(
            temp.path().join(".iwe").join("schemas").as_path(),
            &config,
            &graph,
            &keys,
            true,
        )
        .unwrap_err();
        assert_eq!(
            errors,
            vec!["schema 'person': .iwe/schemas/person.yaml not found".to_string()]
        );
    }

    #[test]
    fn uncompilable_schema_surfaces_schema_error_text() {
        let temp = TempDir::new().unwrap();
        write_schema(temp.path(), "person", "sections:\n  - minContains: -1\n");
        let graph = graph_with(&[("people/alice", "# Summary\n")]);
        let config = config_with(&[("person", Patterns::One("people/**".to_string()))]);
        let keys = vec![Key::name("people/alice")];

        let errors = validate_documents_in(
            temp.path().join(".iwe").join("schemas").as_path(),
            &config,
            &graph,
            &keys,
            true,
        )
        .unwrap_err();
        assert_eq!(
            errors,
            vec![
                "schema 'person' /sections/0/minContains: minContains must not be negative"
                    .to_string()
            ]
        );
    }

    #[test]
    fn validate_pending_reports_violations_without_a_loaded_graph() {
        let temp = TempDir::new().unwrap();
        write_schema(
            temp.path(),
            "person",
            "sections:\n  - header: { const: Summary }\n  - header: { const: Tasks }\n",
        );
        let config = config_with(&[("person", Patterns::One("people/**".to_string()))]);
        let docs = vec![(Key::name("people/alice"), "# Summary\n".to_string())];

        let reports = validate_pending_documents_in(
            temp.path().join(".iwe").join("schemas").as_path(),
            &config,
            &docs,
        )
        .expect("no config errors")
        .reports;

        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].key, Key::name("people/alice"));
        assert_eq!(reports[0].schema, "person");
        assert_eq!(
            reports[0].violations,
            vec![Violation {
                breadcrumb: vec![],
                message: "required section \"Tasks\" is missing".to_string(),
                hint: None,
                schema_pointer: "/sections/1/minContains".to_string(),
                keyword: "minContains".to_string(),
            }]
        );
    }

    #[test]
    fn validate_pending_passes_clean_content() {
        let temp = TempDir::new().unwrap();
        write_schema(
            temp.path(),
            "person",
            "sections:\n  - header: { const: Summary }\n",
        );
        let config = config_with(&[("person", Patterns::One("people/**".to_string()))]);
        let docs = vec![(Key::name("people/alice"), "# Summary\n\ntext\n".to_string())];

        let reports = validate_pending_documents_in(
            temp.path().join(".iwe").join("schemas").as_path(),
            &config,
            &docs,
        )
        .expect("no config errors")
        .reports;
        assert!(reports.is_empty());
    }

    #[test]
    fn validate_pending_surfaces_config_errors() {
        let temp = TempDir::new().unwrap();
        create_dir_all(temp.path().join(".iwe").join("schemas")).unwrap();
        let config = config_with(&[("person", Patterns::One("people/**".to_string()))]);
        let docs = vec![(Key::name("people/alice"), "# Summary\n".to_string())];

        let errors = validate_pending_documents_in(
            temp.path().join(".iwe").join("schemas").as_path(),
            &config,
            &docs,
        )
        .unwrap_err();
        assert_eq!(
            errors,
            vec!["schema 'person': .iwe/schemas/person.yaml not found".to_string()]
        );
    }
}
