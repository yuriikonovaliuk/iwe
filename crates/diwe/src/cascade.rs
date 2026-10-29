//! Cascade a rename or a delete into frontmatter key references.
//!
//! `rename` and `delete` rewrite inline links and inclusions. Documents also
//! name other documents in their frontmatter — a claim's `proposition.subject`,
//! a qualifier's `term`, anything a bound schema's `links` rules read through
//! `$key: $this.frontmatter.<path>`. Those fields are references too, and a
//! rename that leaves them behind makes the referrer invalid, which the gate
//! then refuses as a whole. This module derives the set of such paths from the
//! schemas in force ([`KeyReferences`]) and, for a rename, rewrites every
//! value equal to the old key in the same change set; for a delete, it names
//! the referrers so the caller can refuse. Nothing is configured: a schema
//! that declares a key path gets the cascade.

use std::collections::BTreeMap;

use liwe::graph::{Graph, GraphContext};
use liwe::model::node::NodeIter;
use liwe::model::{parse_leading_frontmatter, prepend_frontmatter, split_raw_frontmatter, Key};
use liwe::operations::Changes;
use serde_yaml::{Mapping, Value};

use crate::schema::KeyReferences;

/// One frontmatter field of one document that holds a document key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontmatterReference {
    pub key: Key,
    pub path: String,
}

impl std::fmt::Display for FrontmatterReference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.key, self.path)
    }
}

/// Every frontmatter field in the graph that names `target` through a path a
/// bound schema declares as a key, in key order.
pub fn frontmatter_referrers(
    graph: &Graph,
    refs: &KeyReferences,
    target: &Key,
) -> Vec<FrontmatterReference> {
    let target = target.to_string();
    let mut out = Vec::new();
    let mut keys = graph.keys();
    keys.sort();
    for key in keys {
        let paths = refs.paths_for(&key.to_string());
        if paths.is_empty() {
            continue;
        }
        let Some(mapping) = graph.frontmatter(&key) else {
            continue;
        };
        for path in paths {
            if values_at(&Value::Mapping(mapping.clone()), &segments(&path))
                .iter()
                .any(|v| v == &target)
            {
                out.push(FrontmatterReference { key: key.clone(), path });
            }
        }
    }
    out
}

/// Fold the rename of `old` to `new` into `changes`: every frontmatter field
/// naming `old` through a declared key path is rewritten, on top of whatever
/// `changes` already does to that document (a rename of a document that is
/// both linked and named rewrites both in one update). Returns what was
/// rewritten.
pub fn cascade_rename(
    graph: &Graph,
    refs: &KeyReferences,
    old: &Key,
    new: &Key,
    changes: &mut Changes,
) -> Vec<FrontmatterReference> {
    let referrers = frontmatter_referrers(graph, refs, old);
    let mut by_key: BTreeMap<Key, Vec<String>> = BTreeMap::new();
    for r in &referrers {
        by_key.entry(r.key.clone()).or_default().push(r.path.clone());
    }
    let format = graph.format_options().clone();
    for (key, paths) in by_key {
        // The renamed document itself travels under its new key.
        let (slot_key, is_create) = if &key == old {
            (new.clone(), true)
        } else {
            (key.clone(), false)
        };
        let existing = if is_create {
            changes.creates.iter_mut().find(|(k, _)| k == &slot_key)
        } else {
            changes.updates.iter_mut().find(|(k, _)| k == &slot_key)
        };
        match existing {
            Some((_, markdown)) => {
                if let Some(rewritten) = rewrite_markdown(markdown, &paths, old, new) {
                    *markdown = rewritten;
                }
            }
            None => {
                let tree = graph.collect(&key);
                let markdown = tree.iter().to_text(&key.parent(), &format);
                if let Some(rewritten) = rewrite_markdown(&markdown, &paths, old, new) {
                    changes.add_update(key.clone(), rewritten);
                }
            }
        }
    }
    referrers
}

/// Rewrite the frontmatter of a rendered document: every scalar at one of
/// `paths` equal to `old` becomes `new`. None when nothing changed.
pub fn rewrite_markdown(markdown: &str, paths: &[String], old: &Key, new: &Key) -> Option<String> {
    let mapping = parse_leading_frontmatter(markdown)?;
    let mut value = Value::Mapping(mapping);
    let mut changed = false;
    for path in paths {
        changed |= rewrite_at(&mut value, &segments(path), &old.to_string(), &new.to_string());
    }
    if !changed {
        return None;
    }
    let Value::Mapping(mapping) = value else {
        return None;
    };
    let (_, body) = split_raw_frontmatter(markdown);
    prepend_frontmatter(Some(mapping), body.trim_start_matches('\n')).ok()
}

fn segments(path: &str) -> Vec<&str> {
    path.split('.').filter(|s| !s.is_empty()).collect()
}

/// The scalar values at a dotted path, descending into sequences the way the
/// schema engine's `$this.frontmatter.<path>` does: a list of mappings yields
/// the field of each element.
fn values_at(value: &Value, path: &[&str]) -> Vec<String> {
    match (value, path) {
        (Value::String(s), []) => vec![s.clone()],
        (Value::Sequence(items), []) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        (Value::Mapping(map), [head, rest @ ..]) => map
            .get(Value::String((*head).to_string()))
            .map(|v| values_at(v, rest))
            .unwrap_or_default(),
        (Value::Sequence(items), _) => items.iter().flat_map(|v| values_at(v, path)).collect(),
        _ => Vec::new(),
    }
}

/// Replace `old` with `new` at a dotted path, in scalars and in lists of
/// scalars, descending into lists of mappings. Reports whether anything changed.
fn rewrite_at(value: &mut Value, path: &[&str], old: &str, new: &str) -> bool {
    match (value, path) {
        (Value::String(s), []) if s == old => {
            *s = new.to_string();
            true
        }
        (Value::Sequence(items), []) => items.iter_mut().fold(false, |acc, v| {
            let hit = matches!(v, Value::String(s) if s == old);
            if hit {
                *v = Value::String(new.to_string());
            }
            acc || hit
        }),
        (Value::Mapping(map), [head, rest @ ..]) => map
            .get_mut(Value::String((*head).to_string()))
            .map(|v| rewrite_at(v, rest, old, new))
            .unwrap_or(false),
        (Value::Sequence(items), _) => items
            .iter_mut()
            .fold(false, |acc, v| rewrite_at(v, path, old, new) || acc),
        _ => false,
    }
}

/// A mapping's values at a path, exposed for the few callers that already
/// hold a mapping.
pub fn mapping_values_at(mapping: &Mapping, path: &str) -> Vec<String> {
    values_at(&Value::Mapping(mapping.clone()), &segments(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Configuration, Patterns, SchemaBinding};
    use liwe::markdown::MarkdownReader;
    use liwe::operations::rename;
    use std::fs::{create_dir_all, write};
    use tempfile::TempDir;

    const CLAIM_SCHEMA: &str = "\
frontmatter:
  type: object
links:
  - some: { type: concept, $key: $this.frontmatter.proposition.subject }
  - some: { type: relation, $key: $this.frontmatter.proposition.predicate }
  - covers: { $key: $this.frontmatter.proposition.qualifiers.term, type: concept }
  - when: { stale_after: { $gt: $this.frontmatter.opened_at } }
    within: Rests on
    min: 0
";

    fn store() -> (TempDir, Configuration) {
        let temp = TempDir::new().unwrap();
        let schemas = temp.path().join(".iwe").join("schemas");
        create_dir_all(&schemas).unwrap();
        write(schemas.join("claim.yaml"), CLAIM_SCHEMA).unwrap();
        let config = Configuration {
            schemas: [(
                "claim".to_string(),
                SchemaBinding { r#match: Patterns::One("claims/**".to_string()) },
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        (temp, config)
    }

    fn graph_with(entries: &[(&str, &str)]) -> Graph {
        let mut graph = Graph::new();
        for (key, content) in entries {
            graph.from_markdown(Key::name(key), content, MarkdownReader::new());
        }
        graph
    }

    const CLAIM: &str = "\
---
type: claim
opened_at: 2026-01-01
proposition:
  subject: concepts/cat
  predicate: relations/chases
  object: concepts/mouse
  qualifiers:
  - relation: relations/in
    term: concepts/garden
  - term: concepts/cat
---

# Cats chase mice

A [cat](../concepts/cat.md) [chases](../relations/chases.md) a [mouse](../concepts/mouse.md) in a [garden](../concepts/garden.md).
";

    #[test]
    fn key_paths_come_from_key_filters_only() {
        let (temp, config) = store();
        let refs = KeyReferences::load(&temp.path().join(".iwe/schemas"), &config).unwrap();
        assert_eq!(
            refs.paths_for("claims/cats"),
            vec![
                "proposition.predicate".to_string(),
                "proposition.qualifiers.term".to_string(),
                "proposition.subject".to_string(),
            ],
            "opened_at is compared, not a key"
        );
        assert!(refs.paths_for("concepts/cat").is_empty());
    }

    #[test]
    fn referrers_are_found_through_scalars_and_lists_of_mappings() {
        let (temp, config) = store();
        let refs = KeyReferences::load(&temp.path().join(".iwe/schemas"), &config).unwrap();
        let graph = graph_with(&[("claims/cats", CLAIM), ("concepts/cat", "# Cat\n")]);
        let found = frontmatter_referrers(&graph, &refs, &Key::name("concepts/cat"));
        assert_eq!(
            found,
            vec![
                FrontmatterReference { key: Key::name("claims/cats"), path: "proposition.qualifiers.term".into() },
                FrontmatterReference { key: Key::name("claims/cats"), path: "proposition.subject".into() },
            ]
        );
        assert!(frontmatter_referrers(&graph, &refs, &Key::name("concepts/mouse")).is_empty(), "object is not declared as a key by this schema");
    }

    #[test]
    fn rename_rewrites_links_and_frontmatter_in_one_update() {
        let (temp, config) = store();
        let refs = KeyReferences::load(&temp.path().join(".iwe/schemas"), &config).unwrap();
        let graph = graph_with(&[("claims/cats", CLAIM), ("concepts/cat", "# Cat\n")]);
        let old = Key::name("concepts/cat");
        let new = Key::name("concepts/feline");
        let mut changes = rename(&graph, &old, &new).unwrap();
        let touched = cascade_rename(&graph, &refs, &old, &new, &mut changes);
        assert_eq!(touched.len(), 2);
        assert_eq!(changes.updates.len(), 1, "the claim is updated once, links and frontmatter together");
        let (key, markdown) = &changes.updates[0];
        assert_eq!(key, &Key::name("claims/cats"));
        assert!(markdown.contains("subject: concepts/feline"), "{markdown}");
        assert!(markdown.contains("- term: concepts/feline"), "{markdown}");
        assert!(markdown.contains("term: concepts/garden"), "other qualifiers stay: {markdown}");
        assert!(markdown.contains("(../concepts/feline"), "the inline link is rewritten too: {markdown}");
        assert!(markdown.starts_with("---\n"), "{markdown}");
        assert_eq!(markdown.matches("---\n").count(), 2, "one frontmatter block: {markdown}");
    }

    #[test]
    fn a_document_only_named_in_frontmatter_gets_its_own_update() {
        let (temp, config) = store();
        let refs = KeyReferences::load(&temp.path().join(".iwe/schemas"), &config).unwrap();
        let unlinked = "---\ntype: claim\nproposition:\n  subject: concepts/cat\n  predicate: relations/chases\n---\n\n# No links here\n";
        let graph = graph_with(&[("claims/quiet", unlinked), ("concepts/cat", "# Cat\n")]);
        let old = Key::name("concepts/cat");
        let new = Key::name("concepts/feline");
        let mut changes = rename(&graph, &old, &new).unwrap();
        assert!(changes.updates.is_empty(), "rename alone sees no link");
        cascade_rename(&graph, &refs, &old, &new, &mut changes);
        assert_eq!(changes.updates.len(), 1);
        assert!(changes.updates[0].1.contains("subject: concepts/feline"));
        assert!(changes.updates[0].1.contains("# No links here"));
    }

    #[test]
    fn nothing_to_rewrite_leaves_changes_alone() {
        let (temp, config) = store();
        let refs = KeyReferences::load(&temp.path().join(".iwe/schemas"), &config).unwrap();
        let graph = graph_with(&[("claims/cats", CLAIM), ("concepts/dog", "# Dog\n")]);
        let old = Key::name("concepts/dog");
        let new = Key::name("concepts/hound");
        let mut changes = rename(&graph, &old, &new).unwrap();
        let before = changes.updates.len();
        assert!(cascade_rename(&graph, &refs, &old, &new, &mut changes).is_empty());
        assert_eq!(changes.updates.len(), before);
    }
}
