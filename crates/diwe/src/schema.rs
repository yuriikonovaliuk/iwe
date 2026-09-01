use std::collections::{HashMap, HashSet, VecDeque};
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
use liwe::query::{build_filter_value, evaluate, evaluate_within, parse_filter_expression, Filter};
use liwe::schema::{build_document, compile_schema, CompiledSchema, Crumb, Violation};
use serde_yaml::Value;

use crate::config::{schemas_dir, Configuration, Invariant, SchemaBinding};
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
/// the scoped links must reach transitively. A `target`/`some` filter may
/// anchor on the validated document itself with `$this` (its key) and
/// `$this.<Section>` (the distinct link targets inside that section); such a
/// filter is re-resolved per document.
#[derive(Debug, Clone)]
pub struct LinkRule {
    /// The rule applies only to documents satisfying this filter (with its
    /// rendering, for messages).
    when: Option<(Filter, String)>,
    within: Option<BlockPredicate>,
    within_label: Option<String>,
    min: u64,
    max: Option<u64>,
    target: Option<Filter>,
    some: Option<Filter>,
    target_this: Option<Value>,
    some_this: Option<Value>,
    /// `covers`: a filter with a `$this.frontmatter.<path>` list anchor; for
    /// every value of that list there must be a link (in scope) whose key is
    /// that value and which satisfies the rest of the filter.
    covers: Option<(Value, String)>,
    reach: Option<Key>,
    description: Option<String>,
}

/// One entry of a schema's `requires` list — a section that must be present
/// (`min` times, at most `max`) whenever the document satisfies `when`, a
/// query-language filter over the document's own frontmatter and content.
#[derive(Debug, Clone)]
pub struct RequireRule {
    when: Filter,
    when_text: String,
    section: String,
    min: u64,
    max: Option<u64>,
    description: Option<String>,
}

/// A schema file compiled in two halves: the document-schema part (handled by
/// the validator crate) and the graph-level `links` and `requires` rules IWE
/// checks itself.
pub struct CompiledSchemaSet {
    pub schema: CompiledSchema,
    pub links: Vec<LinkRule>,
    pub requires: Vec<RequireRule>,
    pub asserts: Vec<AssertRule>,
}

/// One entry of a schema's `asserts` list — a condition the document itself
/// must satisfy, written as a filter that may compare the document's own
/// fields through `$this.frontmatter.<path>` (`stale_after: { $gt:
/// $this.frontmatter.opened_at }`).
#[derive(Debug, Clone)]
pub struct AssertRule {
    that: Value,
    that_text: String,
    description: Option<String>,
}

/// A schema source split into what the document validator understands and
/// the IWE extensions it does not.
pub struct SchemaExtensions {
    pub source: String,
    pub links: Vec<LinkRule>,
    pub requires: Vec<RequireRule>,
    pub asserts: Vec<AssertRule>,
}

/// Split a schema source into the part the document validator understands
/// and the `links` rules, which it does not (`requires` rules are stripped
/// too). A source without either is passed through untouched.
pub fn split_links(source: &str) -> Result<(String, Vec<LinkRule>), Vec<String>> {
    let extensions = split_extensions(source)?;
    Ok((extensions.source, extensions.links))
}

/// Split a schema source into the document-schema part and IWE's own
/// top-level keywords, `links` and `requires`.
pub fn split_extensions(source: &str) -> Result<SchemaExtensions, Vec<String>> {
    let passthrough = || SchemaExtensions {
        source: source.to_string(),
        links: Vec::new(),
        requires: Vec::new(),
        asserts: Vec::new(),
    };
    let mut value: Value = match serde_yaml::from_str(source) {
        Ok(value) => value,
        Err(_) => return Ok(passthrough()),
    };
    let mapping = match value.as_mapping_mut() {
        Some(mapping) => mapping,
        None => return Ok(passthrough()),
    };
    let links = mapping.remove(Value::String("links".to_string()));
    let requires = mapping.remove(Value::String("requires".to_string()));
    let asserts = mapping.remove(Value::String("asserts".to_string()));
    if links.is_none() && requires.is_none() && asserts.is_none() {
        return Ok(passthrough());
    }
    let mut errors = Vec::new();
    let links = match links {
        Some(links) => parse_link_rules(&links).unwrap_or_else(|e| {
            errors.extend(e);
            Vec::new()
        }),
        None => Vec::new(),
    };
    let requires = match requires {
        Some(requires) => parse_require_rules(&requires).unwrap_or_else(|e| {
            errors.extend(e);
            Vec::new()
        }),
        None => Vec::new(),
    };
    let asserts = match asserts {
        Some(asserts) => parse_assert_rules(&asserts).unwrap_or_else(|e| {
            errors.extend(e);
            Vec::new()
        }),
        None => Vec::new(),
    };
    if !errors.is_empty() {
        return Err(errors);
    }
    let rest = serde_yaml::to_string(&value)
        .map_err(|error| vec![format!("links: cannot re-serialize schema: {error}")])?;
    Ok(SchemaExtensions {
        source: rest,
        links,
        requires,
        asserts,
    })
}

fn parse_assert_rules(value: &Value) -> Result<Vec<AssertRule>, Vec<String>> {
    let list = match value {
        Value::Sequence(list) => list,
        _ => return Err(vec!["asserts: expected a list of rules".to_string()]),
    };
    let mut rules = Vec::new();
    let mut errors = Vec::new();
    for (index, entry) in list.iter().enumerate() {
        match parse_assert_rule(entry) {
            Ok(rule) => rules.push(rule),
            Err(message) => errors.push(format!("asserts[{index}]: {message}")),
        }
    }
    if errors.is_empty() {
        Ok(rules)
    } else {
        Err(errors)
    }
}

fn parse_assert_rule(entry: &Value) -> Result<AssertRule, String> {
    let mapping = entry.as_mapping().ok_or("expected a mapping")?;
    let mut that = None;
    let mut description = None;
    for (key, value) in mapping {
        let keyword = key.as_str().ok_or("keys must be strings")?;
        match keyword {
            "that" => {
                if !value.is_mapping() {
                    return Err("that: expected a filter mapping".into());
                }
                check_this_filter(value, "that")?;
                that = Some((value.clone(), flow(value)));
            }
            "description" => {
                description = Some(
                    value
                        .as_str()
                        .ok_or("description: expected a string")?
                        .to_string(),
                )
            }
            other => return Err(format!("unknown keyword '{other}'")),
        }
    }
    let (that, that_text) = that.ok_or("missing 'that'")?;
    Ok(AssertRule {
        that,
        that_text,
        description,
    })
}

/// Check one document against a schema's `asserts` rules: the document
/// must satisfy each `that` filter once its own `$this` anchors are
/// resolved. Document-local.
fn check_asserts(
    graph: &Graph,
    key: &Key,
    rules: &[AssertRule],
    index: &BlockIndex,
) -> Vec<Violation> {
    let mut violations = Vec::new();
    for (i, rule) in rules.iter().enumerate() {
        let mut report = |message: String| {
            violations.push(Violation {
                breadcrumb: Vec::new(),
                message,
                hint: rule.description.clone(),
                schema_pointer: format!("/asserts/{i}"),
                keyword: "asserts".to_string(),
            })
        };
        match this_filter_set(&rule.that, key, index, std::slice::from_ref(key), graph) {
            Ok(set) => {
                if !set.contains(key) {
                    report(format!("assertion fails: {}", rule.that_text));
                }
            }
            Err(error) => report(format!("assertion cannot be resolved: {error}")),
        }
    }
    violations
}

fn parse_require_rules(value: &Value) -> Result<Vec<RequireRule>, Vec<String>> {
    let entries = match value {
        Value::Sequence(entries) => entries,
        _ => return Err(vec!["requires: expected a list of rules".to_string()]),
    };
    let mut rules = Vec::new();
    let mut errors = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        match parse_require_rule(entry) {
            Ok(rule) => rules.push(rule),
            Err(message) => errors.push(format!("requires[{index}]: {message}")),
        }
    }
    if errors.is_empty() {
        Ok(rules)
    } else {
        Err(errors)
    }
}

fn parse_require_rule(entry: &Value) -> Result<RequireRule, String> {
    let mapping = entry.as_mapping().ok_or("expected a mapping")?;
    let mut when = None;
    let mut section = None;
    let mut min = 1;
    let mut max = None;
    let mut description = None;
    for (key, value) in mapping {
        let keyword = key.as_str().ok_or("keys must be strings")?;
        match keyword {
            "when" => {
                if !value.is_mapping() {
                    return Err("when: expected a filter mapping".into());
                }
                let filter = build_filter_value(value).map_err(|error| format!("when: {error}"))?;
                when = Some((filter, flow(value)));
            }
            "section" => {
                section = Some(
                    value
                        .as_str()
                        .ok_or("section: expected a header text")?
                        .to_string(),
                )
            }
            "min" => min = non_negative(value, "min")?,
            "max" => max = Some(non_negative(value, "max")?),
            "description" => {
                description = Some(
                    value
                        .as_str()
                        .ok_or("description: expected a string")?
                        .to_string(),
                )
            }
            other => return Err(format!("unknown keyword '{other}'")),
        }
    }
    let (when, when_text) = when.ok_or("missing 'when'")?;
    let section = section.ok_or("missing 'section'")?;
    if let Some(max) = max {
        if min > max {
            return Err("min is greater than max".into());
        }
    }
    Ok(RequireRule {
        when,
        when_text,
        section,
        min,
        max,
        description,
    })
}

/// Render a YAML value in flow style, for messages.
fn flow(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        Value::Sequence(items) => {
            format!(
                "[{}]",
                items.iter().map(flow).collect::<Vec<_>>().join(", ")
            )
        }
        Value::Mapping(map) => format!(
            "{{ {} }}",
            map.iter()
                .map(|(k, v)| format!("{}: {}", flow(k), flow(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Tagged(tagged) => flow(&tagged.value),
    }
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
        when: None,
        within: None,
        within_label: None,
        min: 0,
        max: None,
        target: None,
        some: None,
        target_this: None,
        some_this: None,
        covers: None,
        reach: None,
        description: None,
    };
    for (key, value) in mapping {
        let keyword = key.as_str().ok_or("keys must be strings")?;
        match keyword {
            "when" => {
                if !value.is_mapping() {
                    return Err("when: expected a filter mapping".into());
                }
                let filter = build_filter_value(value).map_err(|error| format!("when: {error}"))?;
                rule.when = Some((filter, flow(value)));
            }
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
                if contains_this(value) {
                    check_this_filter(value, "target")?;
                    rule.target_this = Some(value.clone());
                } else {
                    rule.target = Some(
                        build_filter_value(value).map_err(|error| format!("target: {error}"))?,
                    );
                }
            }
            "some" => {
                if contains_this(value) {
                    check_this_filter(value, "some")?;
                    rule.some_this = Some(value.clone());
                } else {
                    rule.some =
                        Some(build_filter_value(value).map_err(|error| format!("some: {error}"))?);
                }
            }
            "covers" => {
                if !value.is_mapping() {
                    return Err("covers: expected a filter mapping".into());
                }
                if !contains_this(value) {
                    return Err("covers: expected a $this.frontmatter.<path> anchor".into());
                }
                check_this_filter(value, "covers")?;
                rule.covers = Some((value.clone(), flow(value)));
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

/// Memo shared across every document one validate run checks. A rule's
/// `target`/`some`/`when` filters and a `reach` rule's scoped link targets
/// depend only on the graph and the rule, never on the document under
/// check, so each is computed once per run instead of once per document.
#[derive(Default)]
struct RunCache {
    /// Evaluated key sets, keyed by schema name, rule index and which
    /// filter of the rule ("target", "some", "when", "requires-when").
    filters: HashMap<(String, usize, &'static str), HashSet<Key>>,
    /// For each `reach` rule, every document's link targets inside the
    /// rule's scope — the edges of the reach walk, shared by all chains.
    via_targets: HashMap<(String, usize), HashMap<Key, Vec<Key>>>,
}

impl RunCache {
    fn new() -> Self {
        Self::default()
    }
}

fn filter_set<'c>(
    cache: &'c mut RunCache,
    schema: &str,
    index: usize,
    which: &'static str,
    filter: &Filter,
    graph: &Graph,
) -> &'c HashSet<Key> {
    cache
        .filters
        .entry((schema.to_string(), index, which))
        .or_insert_with(|| evaluate(filter, graph).into_iter().collect())
}

/// Whether `start` reaches `goal` through links inside `scope` — a
/// breadth-first walk over the cached per-document scoped targets, stopping
/// as soon as the goal appears.
fn reaches(
    graph: &Graph,
    start: &Key,
    goal: &Key,
    scope: &BlockPredicate,
    targets: &mut HashMap<Key, Vec<Key>>,
) -> bool {
    let mut visited: HashSet<Key> = HashSet::new();
    let mut queue: VecDeque<Key> = VecDeque::new();
    visited.insert(start.clone());
    queue.push_back(start.clone());
    while let Some(current) = queue.pop_front() {
        let next = targets.entry(current).or_insert_with_key(|key| {
            if graph.maybe_key(key).is_some() {
                BlockIndex::build(graph, key).targets_within(scope)
            } else {
                Vec::new()
            }
        });
        for neighbor in next.clone() {
            if neighbor == *goal {
                return true;
            }
            if visited.insert(neighbor.clone()) {
                queue.push_back(neighbor);
            }
        }
    }
    false
}

const THIS: &str = "$this";
const THIS_PREFIX: &str = "$this.";
const THIS_FRONTMATTER_PREFIX: &str = "$this.frontmatter.";
const LIST_OPERATORS: [&str; 3] = ["$in", "$nin", "$all"];
/// Stands in for an empty `$this.<Section>` list: `$in` of it matches
/// nothing, `$nin` of it matches everything.
const NO_TARGET: &str = "$this.none";

fn contains_this(value: &Value) -> bool {
    match value {
        Value::String(s) => s == THIS || s.starts_with(THIS_PREFIX),
        Value::Sequence(items) => items.iter().any(contains_this),
        Value::Mapping(map) => map
            .iter()
            .any(|(k, v)| contains_this(k) || contains_this(v)),
        _ => false,
    }
}

/// Check at load time that a filter using `$this` parses once the anchors
/// are substituted — with placeholders, since no document is at hand yet.
fn check_this_filter(value: &Value, keyword: &str) -> Result<(), String> {
    let placeholder = |section: Option<&str>| match section {
        None => vec![THIS.to_string()],
        Some(section) => vec![format!("{THIS_PREFIX}{section}")],
    };
    let substituted = substitute_this(value, &placeholder, false);
    build_filter_value(&substituted)
        .map(|_| ())
        .map_err(|error| format!("{keyword}: {error}"))
}

/// Replace `$this` and `$this.<Section>` in a filter value. `resolve(None)`
/// yields the document's own key, `resolve(Some(section))` the distinct link
/// targets inside that section. In a list position (an element of a
/// sequence, or the value of `$in`/`$nin`/`$all`) the targets are spliced
/// in as a list; in a scalar position `$this` becomes the key and
/// `$this.<Section>` becomes `{ $in: [targets] }`. An empty target list
/// becomes a sentinel no document has, so `$in` matches nothing and `$nin`
/// matches everything.
fn substitute_this(
    value: &Value,
    resolve: &dyn Fn(Option<&str>) -> Vec<String>,
    list_context: bool,
) -> Value {
    let strings = |items: Vec<String>| -> Value {
        let items = if items.is_empty() {
            vec![NO_TARGET.to_string()]
        } else {
            items
        };
        Value::Sequence(items.into_iter().map(Value::String).collect())
    };
    match value {
        Value::String(s) if s == THIS => {
            let mut keys = resolve(None);
            if list_context {
                strings(keys)
            } else {
                Value::String(keys.pop().unwrap_or_else(|| NO_TARGET.to_string()))
            }
        }
        Value::String(s) if s.starts_with(THIS_FRONTMATTER_PREFIX) => {
            // `$this.frontmatter.<path>`: the document's own field. A single
            // value stays a scalar so it can sit under `$eq`/`$ne` or as a
            // bare equality; several become a list.
            let mut values = resolve(Some(&s[THIS_PREFIX.len()..]));
            if list_context {
                strings(values)
            } else if values.len() == 1 {
                Value::String(values.pop().unwrap())
            } else {
                let mut map = serde_yaml::Mapping::new();
                map.insert(Value::String("$in".to_string()), strings(values));
                Value::Mapping(map)
            }
        }
        Value::String(s) if s.starts_with(THIS_PREFIX) => {
            let list = strings(resolve(Some(&s[THIS_PREFIX.len()..])));
            if list_context {
                list
            } else {
                let mut map = serde_yaml::Mapping::new();
                map.insert(Value::String("$in".to_string()), list);
                Value::Mapping(map)
            }
        }
        Value::Sequence(items) => {
            let mut out = Vec::new();
            for item in items {
                match item {
                    Value::String(s) if s == THIS || s.starts_with(THIS_PREFIX) => {
                        match substitute_this(item, resolve, true) {
                            Value::Sequence(spliced) => out.extend(spliced),
                            other => out.push(other),
                        }
                    }
                    other => out.push(substitute_this(other, resolve, false)),
                }
            }
            Value::Sequence(out)
        }
        Value::Mapping(map) => Value::Mapping(
            map.iter()
                .map(|(k, v)| {
                    let list = k
                        .as_str()
                        .map(|k| LIST_OPERATORS.contains(&k))
                        .unwrap_or(false);
                    (k.clone(), substitute_this(v, resolve, list))
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

/// The `$this.frontmatter.<path>` anchors inside a filter value, as paths.
fn collect_frontmatter_anchors(value: &Value) -> Vec<String> {
    let mut out = Vec::new();
    match value {
        Value::String(s) if s.starts_with(THIS_FRONTMATTER_PREFIX) => {
            out.push(s[THIS_FRONTMATTER_PREFIX.len()..].to_string())
        }
        Value::Sequence(items) => items
            .iter()
            .for_each(|v| out.extend(collect_frontmatter_anchors(v))),
        Value::Mapping(map) => map
            .iter()
            .for_each(|(_, v)| out.extend(collect_frontmatter_anchors(v))),
        _ => {}
    }
    out
}

/// The values at a dotted path in a document's frontmatter, as strings: a
/// scalar gives one, a sequence of scalars gives each, anything else none.
fn frontmatter_values(graph: &Graph, key: &Key, path: &str) -> Vec<String> {
    let Some(mapping) = graph.frontmatter(key) else {
        return Vec::new();
    };
    let mut current = Value::Mapping(mapping.clone());
    for segment in path.split('.') {
        current = match current {
            Value::Mapping(map) => match map.get(Value::String(segment.to_string())) {
                Some(v) => v.clone(),
                None => return Vec::new(),
            },
            // A list of mappings: the field of each element.
            Value::Sequence(items) => Value::Sequence(
                items
                    .iter()
                    .filter_map(|item| match item {
                        Value::Mapping(map) => map.get(Value::String(segment.to_string())).cloned(),
                        _ => None,
                    })
                    .collect(),
            ),
            _ => return Vec::new(),
        };
    }
    let scalar = |v: &Value| -> Option<String> {
        match v {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            Value::Bool(b) => Some(b.to_string()),
            _ => None,
        }
    };
    match &current {
        Value::Sequence(items) => items.iter().filter_map(scalar).collect(),
        other => scalar(other).into_iter().collect(),
    }
}

/// Resolve a `$this` filter for one document and evaluate it over the
/// candidate targets only.
fn this_filter_set(
    raw: &Value,
    key: &Key,
    index: &BlockIndex,
    targets: &[Key],
    graph: &Graph,
) -> Result<HashSet<Key>, String> {
    let resolve = |section: Option<&str>| -> Vec<String> {
        match section {
            None => vec![key.to_string()],
            Some(path) if path.starts_with("frontmatter.") => {
                frontmatter_values(graph, key, &path["frontmatter.".len()..])
            }
            Some(section) => index
                .targets_within(&BlockPredicate::empty().within_section(section))
                .into_iter()
                .map(|k| k.to_string())
                .collect(),
        }
    };
    let substituted = substitute_this(raw, &resolve, false);
    let filter = build_filter_value(&substituted).map_err(|error| error.to_string())?;
    if targets.is_empty() {
        return Ok(HashSet::new());
    }
    let candidates: HashSet<Key> = targets.iter().cloned().collect();
    Ok(evaluate_within(&filter, graph, &candidates))
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
    cache: &mut RunCache,
    index: &BlockIndex,
) -> Vec<Violation> {
    if rules.is_empty() {
        return Vec::new();
    }
    let mut violations = Vec::new();
    for (i, rule) in rules.iter().enumerate() {
        if let Some((when, _)) = &rule.when {
            if !filter_set(cache, schema, i, "when", when, graph).contains(key) {
                continue;
            }
        }
        let scope = rule.within.clone().unwrap_or_default();
        let targets = index.targets_within(&scope);
        let where_ = match (&rule.within_label, &rule.when) {
            (Some(label), Some((_, when))) => format!(" within '{label}' (when {when})"),
            (Some(label), None) => format!(" within '{label}'"),
            (None, Some((_, when))) => format!(" (when {when})"),
            (None, None) => String::new(),
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
        let target_allowed: Option<HashSet<Key>> = match (&rule.target, &rule.target_this) {
            (Some(target), _) => {
                Some(filter_set(cache, schema, i, "target", target, graph).clone())
            }
            (None, Some(raw)) => match this_filter_set(raw, key, index, &targets, graph) {
                Ok(set) => Some(set),
                Err(error) => {
                    report(format!("target filter cannot be resolved: {error}"));
                    None
                }
            },
            (None, None) => None,
        };
        if let Some(allowed) = target_allowed {
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
        let some_allowed: Option<HashSet<Key>> = match (&rule.some, &rule.some_this) {
            (Some(some), _) => Some(filter_set(cache, schema, i, "some", some, graph).clone()),
            (None, Some(raw)) => match this_filter_set(raw, key, index, &targets, graph) {
                Ok(set) => Some(set),
                Err(error) => {
                    report(format!("some filter cannot be resolved: {error}"));
                    None
                }
            },
            (None, None) => None,
        };
        if let Some(allowed) = some_allowed {
            if !targets.iter().any(|t| allowed.contains(t)) {
                report(format!("no link{where_} satisfies the 'some' filter"));
            }
        }
        if let Some((raw, text)) = &rule.covers {
            // Every value of the anchored list must be a link target in scope
            // that satisfies the filter.
            let anchors: Vec<String> = collect_frontmatter_anchors(raw);
            let mut wanted: Vec<String> = Vec::new();
            for path in &anchors {
                wanted.extend(frontmatter_values(graph, key, path));
            }
            wanted.sort();
            wanted.dedup();
            match this_filter_set(raw, key, index, &targets, graph) {
                Ok(satisfied) => {
                    for value in wanted {
                        let k = Key::name(&value);
                        if !targets.contains(&k) {
                            report(format!(
                                "'{value}' is named in the frontmatter but not linked{where_} ({text})"
                            ));
                        } else if !satisfied.contains(&k) {
                            report(format!(
                                "link to '{value}'{where_} does not satisfy the covers filter ({text})"
                            ));
                        }
                    }
                }
                Err(error) => report(format!("covers filter cannot be resolved: {error}")),
            }
        }
        if let Some(reach) = &rule.reach {
            if key != reach {
                let targets = cache
                    .via_targets
                    .entry((schema.to_string(), i))
                    .or_default();
                if !reaches(graph, key, reach, &scope, targets) {
                    report(format!("no chain of links{where_} reaches '{reach}'"));
                }
            }
        }
    }
    violations
}

/// Check one document against a schema's `requires` rules: whenever the
/// document satisfies a rule's `when` filter, the named section must appear
/// the required number of times. Document-local, so it runs on partial
/// graphs too.
fn check_requires(
    graph: &Graph,
    key: &Key,
    schema: &str,
    rules: &[RequireRule],
    cache: &mut RunCache,
    index: &BlockIndex,
) -> Vec<Violation> {
    let mut violations = Vec::new();
    for (i, rule) in rules.iter().enumerate() {
        if !filter_set(cache, schema, i, "requires-when", &rule.when, graph).contains(key) {
            continue;
        }
        let count = index
            .select(&BlockPredicate::empty().header(rule.section.as_str()))
            .len() as u64;
        let mut report = |message: String| {
            violations.push(Violation {
                breadcrumb: vec![Crumb::Header(rule.section.clone())],
                message,
                hint: rule.description.clone(),
                schema_pointer: format!("/requires/{i}"),
                keyword: "requires".to_string(),
            })
        };
        if count < rule.min {
            if count == 0 && rule.min == 1 {
                report(format!(
                    "required section \"{}\" is missing when {}",
                    rule.section, rule.when_text
                ));
            } else {
                report(format!(
                    "section \"{}\" appears {count} time{}, fewer than the minimum of {} when {}",
                    rule.section,
                    if count == 1 { "" } else { "s" },
                    rule.min,
                    rule.when_text
                ));
            }
        }
        if let Some(max) = rule.max {
            if count > max {
                report(format!(
                    "section \"{}\" appears {count} times, greater than the maximum of {max} when {}",
                    rule.section, rule.when_text
                ));
            }
        }
    }
    violations
}

/// Replace `$today`, `$today-Nd` and `$today+Nd` with ISO dates relative to
/// `today`, so date fields (ISO strings compare lexicographically) can be
/// tested against the calendar.
pub fn substitute_today(filter: &str, today: chrono::NaiveDate) -> String {
    let mut out = String::new();
    let mut rest = filter;
    while let Some(at) = rest.find("$today") {
        out.push_str(&rest[..at]);
        let after = &rest[at + "$today".len()..];
        let mut consumed = 0;
        let mut date = today;
        let bytes = after.as_bytes();
        if !bytes.is_empty() && (bytes[0] == b'+' || bytes[0] == b'-') {
            let digits: String = after[1..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if !digits.is_empty() && after[1 + digits.len()..].starts_with('d') {
                let days: i64 = digits.parse().unwrap_or(0);
                let delta = chrono::Duration::days(days);
                date = if bytes[0] == b'+' {
                    today + delta
                } else {
                    today - delta
                };
                consumed = 1 + digits.len() + 1;
            }
        }
        out.push_str(&date.format("%Y-%m-%d").to_string());
        rest = &after[consumed..];
    }
    out.push_str(rest);
    out
}

enum Expect {
    Exactly(u64),
    Compare(Vec<(String, u64)>),
}

impl Expect {
    fn parse(value: &toml::Value) -> Result<Expect, String> {
        match value {
            toml::Value::Integer(n) if *n >= 0 => Ok(Expect::Exactly(*n as u64)),
            toml::Value::Integer(_) => Err("expect: expected a non-negative integer".into()),
            toml::Value::String(text) => {
                let parsed: Value =
                    serde_yaml::from_str(text).map_err(|error| format!("expect: {error}"))?;
                let map = parsed.as_mapping().ok_or(
                    "expect: expected an integer or a count predicate such as { $lte: 3 }",
                )?;
                let mut comparisons = Vec::new();
                for (k, v) in map {
                    let op = k.as_str().ok_or("expect: keys must be strings")?;
                    if !["$eq", "$ne", "$lt", "$lte", "$gt", "$gte"].contains(&op) {
                        return Err(format!("expect: unknown comparison '{op}'"));
                    }
                    let n = v
                        .as_u64()
                        .ok_or_else(|| format!("expect: {op} expects a non-negative integer"))?;
                    comparisons.push((op.to_string(), n));
                }
                if comparisons.is_empty() {
                    return Err("expect: empty count predicate".into());
                }
                Ok(Expect::Compare(comparisons))
            }
            _ => Err("expect: expected an integer or a count predicate string".into()),
        }
    }

    fn satisfied_by(&self, count: u64) -> bool {
        match self {
            Expect::Exactly(n) => count == *n,
            Expect::Compare(comparisons) => comparisons.iter().all(|(op, n)| match op.as_str() {
                "$eq" => count == *n,
                "$ne" => count != *n,
                "$lt" => count < *n,
                "$lte" => count <= *n,
                "$gt" => count > *n,
                "$gte" => count >= *n,
                _ => false,
            }),
        }
    }

    fn describe(&self) -> String {
        match self {
            Expect::Exactly(n) => n.to_string(),
            Expect::Compare(comparisons) => comparisons
                .iter()
                .map(|(op, n)| format!("{op} {n}"))
                .collect::<Vec<_>>()
                .join(", "),
        }
    }
}

/// Check the graph-wide `[invariants]` of the configuration: each names a
/// filter and the count its matches must satisfy. A failing invariant is
/// reported under the synthetic key `invariants/<name>`, listing the
/// offending documents; a malformed one is a configuration error.
pub fn check_invariants(
    config: &Configuration,
    graph: &Graph,
) -> Result<Vec<KeyReport>, Vec<String>> {
    check_invariants_on(config, graph, chrono::Local::now().date_naive())
}

pub fn check_invariants_on(
    config: &Configuration,
    graph: &Graph,
    today: chrono::NaiveDate,
) -> Result<Vec<KeyReport>, Vec<String>> {
    let mut names: Vec<&String> = config.invariants.keys().collect();
    names.sort();
    let mut reports = Vec::new();
    let mut errors = Vec::new();
    for name in names {
        let invariant: &Invariant = &config.invariants[name];
        let expect = match Expect::parse(&invariant.expect) {
            Ok(expect) => expect,
            Err(error) => {
                errors.push(format!("invariant '{name}': {error}"));
                continue;
            }
        };
        let filter = match parse_filter_expression(&substitute_today(&invariant.filter, today)) {
            Ok(filter) => filter,
            Err(error) => {
                errors.push(format!("invariant '{name}': filter: {error}"));
                continue;
            }
        };
        let matches = evaluate(&filter, graph);
        let count = matches.len() as u64;
        if expect.satisfied_by(count) {
            continue;
        }
        let listed: Vec<String> = matches.iter().take(10).map(|k| k.to_string()).collect();
        let more = if matches.len() > listed.len() {
            format!(", and {} more", matches.len() - listed.len())
        } else {
            String::new()
        };
        let message = if matches.is_empty() {
            format!("0 documents match, expected {}", expect.describe())
        } else {
            format!(
                "{count} {}, expected {}: {}{more}",
                if count == 1 {
                    "document matches"
                } else {
                    "documents match"
                },
                expect.describe(),
                listed.join(", ")
            )
        };
        reports.push(KeyReport {
            key: Key::name(&format!("invariants/{name}")),
            schema: "config".to_string(),
            violations: vec![Violation {
                breadcrumb: Vec::new(),
                message,
                hint: invariant.description.clone(),
                schema_pointer: format!("/invariants/{name}"),
                keyword: "invariants".to_string(),
            }],
        });
    }
    if errors.is_empty() {
        Ok(reports)
    } else {
        Err(errors)
    }
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
    let extensions = split_extensions(&source).map_err(|errors| {
        errors
            .into_iter()
            .map(|e| format!("schema '{label}' {e}"))
            .collect::<Vec<_>>()
    })?;

    let compiled = compile_schema(&extensions.source).map_err(|schema_errors| {
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
    let mut cache = RunCache::new();
    for key in keys {
        let document = build_document(graph, key, count_tokens);
        let mut violations = compiled.validate(&document);
        let index = BlockIndex::build(graph, key);
        violations.extend(check_requires(
            graph,
            key,
            &label,
            &extensions.requires,
            &mut cache,
            &index,
        ));
        violations.extend(check_asserts(graph, key, &extensions.asserts, &index));
        violations.extend(check_links(
            graph,
            key,
            &label,
            &extensions.links,
            true,
            &mut cache,
            &index,
        ));
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

/// Reports from the external checkers, split into those that fail the run
/// and those configured to warn only.
pub struct CheckerReports {
    pub failing: Vec<KeyReport>,
    pub warnings: Vec<KeyReport>,
}

/// Run the configured external checkers over `keys`. With `all` false only
/// the `always` checkers run. Each checker's output is parsed into reports
/// under `schema: "checker:<name>"`; a checker that exits non-zero or prints
/// something other than the expected JSON produces one report under the
/// synthetic key `checkers/<name>`.
pub fn run_checkers(
    config: &Configuration,
    root: &std::path::Path,
    keys: &[Key],
    all: bool,
) -> CheckerReports {
    use std::io::Write;
    use std::process::{Command, Stdio};

    #[derive(serde::Deserialize)]
    struct RawViolation {
        message: String,
        #[serde(default)]
        hint: Option<String>,
        #[serde(default)]
        pointer: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct RawReport {
        key: String,
        violations: Vec<RawViolation>,
    }

    let mut out = CheckerReports {
        failing: Vec::new(),
        warnings: Vec::new(),
    };
    let mut names: Vec<&String> = config.checkers.keys().collect();
    names.sort();
    for name in names {
        let checker = &config.checkers[name];
        if !all && !checker.always {
            continue;
        }
        let input = serde_json::json!({
            "root": root.to_string_lossy(),
            "keys": keys.iter().map(|k| k.to_string()).collect::<Vec<_>>(),
        });
        let failure = |message: String| KeyReport {
            key: Key::name(&format!("checkers/{name}")),
            schema: format!("checker:{name}"),
            violations: vec![Violation {
                breadcrumb: Vec::new(),
                message,
                hint: checker.description.clone(),
                schema_pointer: format!("/checkers/{name}"),
                keyword: "checker".to_string(),
            }],
        };
        let mut child = match Command::new("sh")
            .arg("-c")
            .arg(&checker.command)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                out.failing
                    .push(failure(format!("checker could not start: {error}")));
                continue;
            }
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(input.to_string().as_bytes());
        }
        let output = match child.wait_with_output() {
            Ok(output) => output,
            Err(error) => {
                out.failing
                    .push(failure(format!("checker failed: {error}")));
                continue;
            }
        };
        let target = if checker.warn {
            &mut out.warnings
        } else {
            &mut out.failing
        };
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            target.push(failure(format!(
                "checker exited with {}: {}",
                output.status,
                stderr.trim()
            )));
            continue;
        }
        let parsed: Result<Vec<RawReport>, _> = serde_json::from_slice(&output.stdout);
        match parsed {
            Ok(reports) => {
                for report in reports {
                    if report.violations.is_empty() {
                        continue;
                    }
                    target.push(KeyReport {
                        key: Key::name(&report.key),
                        schema: format!("checker:{name}"),
                        violations: report
                            .violations
                            .into_iter()
                            .map(|v| Violation {
                                breadcrumb: Vec::new(),
                                message: v.message,
                                hint: v.hint,
                                schema_pointer: v
                                    .pointer
                                    .unwrap_or_else(|| format!("/checkers/{name}")),
                                keyword: "checker".to_string(),
                            })
                            .collect(),
                    });
                }
            }
            Err(error) => target.push(failure(format!(
                "checker printed something other than a JSON array of reports: {error}"
            ))),
        }
    }
    out
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
    let mut cache = RunCache::new();
    for key in keys {
        let names = bindings.schemas_for(&key.to_string());
        if names.is_empty() {
            continue;
        }
        documents += 1;
        let document = build_document(graph, key, count_tokens);
        let index = BlockIndex::build(graph, key);
        for name in names {
            schemas_used.insert(name);
            let set = &compiled[name];
            let mut violations = set.schema.validate(&document);
            violations.extend(check_requires(graph, key, name, &set.requires, &mut cache, &index));
            violations.extend(check_asserts(graph, key, &set.asserts, &index));
            violations.extend(check_links(
                graph, key, name, &set.links, full, &mut cache, &index,
            ));
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
        let extensions = match split_extensions(&source) {
            Ok(split) => split,
            Err(link_errors) => {
                for error in link_errors {
                    errors.push(format!("schema '{name}' {error}"));
                }
                continue;
            }
        };
        match compile_schema(&extensions.source) {
            Ok(schema) => {
                compiled.insert(
                    name.clone(),
                    CompiledSchemaSet {
                        schema,
                        links: extensions.links,
                        requires: extensions.requires,
                        asserts: extensions.asserts,
                    },
                );
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
