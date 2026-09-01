//! Fill-in requests: what a missing document owes. A fetch of a key with no
//! document — or a whole-store sweep for referenced-but-missing targets —
//! answers with the shape the store expects there instead of nothing: the
//! schemas the key would bind to, the `type` the folder pins, the required
//! frontmatter and sections, what the links rules will demand, and who
//! already references it. Who fills the request is not IWE's business.

use std::collections::HashSet;
use std::fs::read_to_string;

use serde::Serialize;
use serde_yaml::Value;

use liwe::graph::Graph;
use liwe::model::Key;
use liwe::query::block::BlockPredicate;
use liwe::query::block_eval::BlockIndex;

use crate::config::{schemas_dir, Configuration};
use crate::schema::SchemaBindings;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FillInRequest {
    pub key: String,
    /// The schemas the key will be validated against once written.
    pub schemas: Vec<String>,
    /// The `type` the folder expects, when a bound schema pins one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_type: Option<String>,
    /// Frontmatter fields the bound schemas require.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub required_frontmatter: Vec<String>,
    /// Sections the bound schemas require, in schema order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub required_sections: Vec<String>,
    /// What the links rules will demand, in their own words.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub owed_links: Vec<String>,
    /// Documents that already reference the missing key — the terms known.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub referenced_by: Vec<Referrer>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Referrer {
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// The fill-in request for one key with no document.
pub fn fill_in_request(
    config: &Configuration,
    graph: &Graph,
    key: &Key,
) -> Result<FillInRequest, String> {
    let bindings = SchemaBindings::compile(&config.schemas)
        .map_err(|errors| errors.join("; "))?;
    let names: Vec<String> = bindings
        .schemas_for(&key.to_string())
        .into_iter()
        .map(str::to_string)
        .collect();

    let mut request = FillInRequest {
        key: key.to_string(),
        schemas: names.clone(),
        expected_type: None,
        required_frontmatter: Vec::new(),
        required_sections: Vec::new(),
        owed_links: Vec::new(),
        referenced_by: referrers(graph, key),
    };

    let dir = schemas_dir()?;
    for name in &names {
        let path = dir.join(format!("{name}.yaml"));
        let Ok(source) = read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_yaml::from_str::<Value>(&source) else {
            continue;
        };
        merge_schema(&mut request, &value);
    }
    Ok(request)
}

/// Every key some document links to that has no document itself, sorted.
pub fn missing_link_targets(graph: &Graph) -> Vec<Key> {
    let everything = BlockPredicate::empty();
    let mut missing: HashSet<Key> = HashSet::new();
    for key in graph.keys() {
        for target in BlockIndex::build(graph, &key).targets_within(&everything) {
            if graph.maybe_key(&target).is_none() {
                missing.insert(target);
            }
        }
    }
    let mut missing: Vec<Key> = missing.into_iter().collect();
    missing.sort();
    missing
}

fn referrers(graph: &Graph, key: &Key) -> Vec<Referrer> {
    let mut out: Vec<Referrer> = graph
        .get_document_references_to(key)
        .into_iter()
        .map(|r| Referrer {
            key: r.source_key.to_string(),
            title: r.source_title,
        })
        .collect();
    out.sort_by(|a, b| a.key.cmp(&b.key));
    out.dedup_by(|a, b| a.key == b.key);
    out
}

fn merge_schema(request: &mut FillInRequest, schema: &Value) {
    if let Some(frontmatter) = schema.get("frontmatter") {
        if request.expected_type.is_none() {
            request.expected_type = frontmatter
                .get("properties")
                .and_then(|p| p.get("type"))
                .and_then(|t| t.get("const"))
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        if let Some(required) = frontmatter.get("required").and_then(Value::as_sequence) {
            for field in required.iter().filter_map(Value::as_str) {
                push_unique(&mut request.required_frontmatter, field);
            }
        }
    }
    if let Some(sections) = schema.get("sections") {
        collect_required_sections(sections, &mut request.required_sections);
    }
    if let Some(links) = schema.get("links").and_then(Value::as_sequence) {
        for rule in links {
            let owed = rule
                .get("min")
                .and_then(Value::as_u64)
                .is_some_and(|min| min > 0)
                || rule.get("reach").is_some();
            if !owed {
                continue;
            }
            if let Some(text) = rule.get("description").and_then(Value::as_str) {
                push_unique(&mut request.owed_links, text);
            }
        }
    }
}

/// Walk a schema's `sections` tree collecting the headers of sections that
/// must appear — a `header: { const: ... }` entry not opted out with
/// `minContains: 0`.
fn collect_required_sections(value: &Value, out: &mut Vec<String>) {
    let Some(entries) = value.as_sequence() else {
        return;
    };
    for entry in entries {
        let optional = entry
            .get("minContains")
            .and_then(Value::as_u64)
            .is_some_and(|min| min == 0);
        if !optional {
            if let Some(header) = entry
                .get("header")
                .and_then(|h| h.get("const"))
                .and_then(Value::as_str)
            {
                push_unique(out, header);
            }
        }
        if let Some(nested) = entry.get("sections") {
            collect_required_sections(nested, out);
        }
    }
}

fn push_unique(list: &mut Vec<String>, item: &str) {
    if !list.iter().any(|existing| existing == item) {
        list.push(item.to_string());
    }
}
