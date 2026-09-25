//! Model-facing tool specs. Every agent request carries every tool's spec,
//! so the listing is compact by default: a one-line description per tool,
//! parameter descriptions cut to their first sentence, deprecated parameters
//! left out, and JSON-Schema noise dropped (`$schema`, `format`, numeric
//! bounds, `null` unions -- optional is already "not in `required`").
//! `IWEC_FULL_TOOL_SCHEMAS=1` serves the full specs instead.

use std::borrow::Cow;
use std::sync::Arc;

use rmcp::model::Tool;
use serde_json::Value;

/// One line per tool: what it does and the parameters that matter.
fn terse_description(name: &str) -> Option<&'static str> {
    Some(match name {
        "iwe_find" => "Find documents: fuzzy/lexical text, refs_to/refs_from, in/in_any/not_in subtree filters, project fields; returns keys and titles.",
        "iwe_retrieve" => "Read documents by keys (or search/fuzzy seeds), optionally expanding includes/includedBy/references/referencedBy; cap with limit, max_documents, max_tokens.",
        "iwe_tree" => "Show the inclusion tree under the given keys (or the whole graph).",
        "iwe_stats" => "Graph statistics: document counts, references, broken links, orphans.",
        "iwe_squash" => "Flatten a document and everything it includes into one markdown text.",
        "iwe_create" => "Create a document at `key` from complete markdown `content` (frontmatter first); if_exists=skip makes retries idempotent.",
        "iwe_update" => "Replace a document's full markdown content.",
        "iwe_delete" => "Delete a document and remove every link and reference to it.",
        "iwe_query" => "Run a query operation document: find/count read; update/delete change the documents a filter matches.",
        "iwe_rename" => "Rename a document key; every link to it is updated.",
        "iwe_extract" => "Move a section into a new document, leaving an inclusion link in its place.",
        "iwe_inline" => "Replace an inclusion link with the included document's content (list mode shows the links).",
        "iwe_normalize" => "Rewrite every document into canonical formatting.",
        "iwe_attach" => "Attach a document as a block reference through a configured attach action.",
        "iwe_argue" => "Compute each claim's and objection's standing (in, out, undecided) from the objections against it.",
        "iwe_check" => "Validate documents by key against their schemas (per-document rules, no invariants).",
        "iwe_tx_begin" => "Open a transaction: later writes stage until iwe_tx_commit; returns the handle.",
        "iwe_tx_commit" => "Validate all staged writes as one unit and write them, or refuse them all.",
        "iwe_tx_abort" => "Discard every staged write of the open transaction.",
        _ => return None,
    })
}

/// A short hint for a parameter whose name alone does not say how to use
/// it; every other parameter is served without a description.
fn param_hint(name: &str) -> Option<&'static str> {
    Some(match name {
        "project" => "fields to return, e.g. \"$key,$title,priority\"",
        "add_fields" => "extra fields to return, same grammar as project",
        "expand" => "{includes|includedBy|references|referencedBy: depth}, 0 = unbounded",
        "in" => "sub-documents of ALL these keys (key or {key, depth})",
        "in_any" => "sub-documents of ANY of these keys",
        "not_in" => "not sub-documents of these keys",
        "max_depth" => "default depth for in/in_any/not_in",
        "search" | "lexical" => "BM25 full-text query",
        "fuzzy" => "fuzzy match on title and key",
        "handle" => "transaction handle (optional)",
        "if_exists" => "fail (default) | skip",
        "content" => "full markdown, frontmatter first",
        "document" => "the operation as YAML",
        "operation" => "find | count | update | delete",
        "filter" => "query filter selecting documents",
        "block" => "block number, 1-based (list mode shows them)",
        "section" => "section title (partial match)",
        "reference" => "key or title to inline (partial match)",
        "to" => "attach action name(s)",
        "explain" => "diagnose the cycles behind undecided claims",
        "backlinks" => "include incoming links (default true)",
        "children" => "fill the includes edges",
        "depth" => "levels to show",
        "max_documents" => "cap on documents after expansion",
        "max_tokens" | "max_document_tokens" => "cap on content tokens",
        "frontmatter" => "content leads with the stored frontmatter, verbatim",
        "keep_frontmatter" => "keep stored frontmatter; content = body only",
        _ => return None,
    })
}

/// The first sentence, at most `max` characters.
fn first_sentence(text: &str, max: usize) -> String {
    let text = text.trim();
    let end = text
        .char_indices()
        .find(|&(i, c)| c == '.' && text[i + 1..].starts_with(|n: char| n.is_whitespace()))
        .map(|(i, _)| i)
        .unwrap_or(text.len());
    let sentence = text[..end].trim_end_matches('.');
    if sentence.chars().count() <= max {
        sentence.to_string()
    } else {
        let cut: String = sentence.chars().take(max).collect();
        format!("{}…", cut.trim_end())
    }
}

fn is_null_schema(v: &Value) -> bool {
    v.get("type").and_then(Value::as_str) == Some("null") && v.as_object().is_some_and(|o| o.len() == 1)
}

fn compact_schema(v: &mut Value) {
    match v {
        Value::Object(map) => {
            for k in ["$schema", "format", "minimum", "maximum", "title"] {
                map.remove(k);
            }
            if map.get("default").is_some_and(Value::is_null) {
                map.remove("default");
            }
            // ["integer","null"] -> "integer"
            if let Some(Value::Array(types)) = map.get("type") {
                let non_null: Vec<Value> = types.iter().filter(|t| t.as_str() != Some("null")).cloned().collect();
                if non_null.len() == 1 {
                    map.insert("type".into(), non_null[0].clone());
                }
            }
            // anyOf [X, {"type":"null"}] -> X
            if let Some(Value::Array(any)) = map.get("anyOf") {
                let rest: Vec<Value> = any.iter().filter(|s| !is_null_schema(s)).cloned().collect();
                if rest.len() == 1 && rest.len() < any.len() {
                    map.remove("anyOf");
                    if let Value::Object(inner) = rest[0].clone() {
                        for (k, val) in inner {
                            map.entry(k).or_insert(val);
                        }
                    }
                }
            }
            if let Some(Value::Object(props)) = map.get_mut("properties") {
                props.retain(|_, p| {
                    !p.get("description").and_then(Value::as_str).is_some_and(|d| d.trim_start().starts_with("DEPRECATED"))
                });
                for (name, p) in props.iter_mut() {
                    if let Value::Object(p) = p {
                        match param_hint(name) {
                            Some(hint) => {
                                p.insert("description".into(), Value::String(hint.into()));
                            }
                            None => {
                                p.remove("description");
                            }
                        }
                    }
                }
            }
            for (_, child) in map.iter_mut() {
                compact_schema(child);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(compact_schema),
        _ => {}
    }
}

/// The model-facing form of `tool` (see the module docs).
pub fn compact(mut tool: Tool) -> Tool {
    if let Some(d) = terse_description(&tool.name) {
        tool.description = Some(Cow::Borrowed(d));
    } else if let Some(d) = tool.description.take() {
        tool.description = Some(Cow::Owned(first_sentence(&d, 160)));
    }
    let mut schema = Value::Object((*tool.input_schema).clone());
    compact_schema(&mut schema);
    if let Value::Object(map) = schema {
        tool.input_schema = Arc::new(map);
    }
    tool.output_schema = None;
    tool
}

/// Whether to serve compact specs (the default) or the full ones.
pub fn enabled() -> bool {
    std::env::var("IWEC_FULL_TOOL_SCHEMAS").map(|v| v != "1").unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nullable_integers_become_plain_and_lose_bounds() {
        let mut v = serde_json::json!({"properties": {
            "limit": {"type": ["integer", "null"], "format": "uint", "minimum": 0, "description": "Cap. More text here."},
            "handle": {"type": ["string", "null"], "description": "Transaction handle from iwe_tx_begin. Long."}
        }});
        compact_schema(&mut v);
        assert_eq!(v, serde_json::json!({"properties": {
            "limit": {"type": "integer"},
            "handle": {"type": "string", "description": "transaction handle (optional)"}
        }}));
    }

    #[test]
    fn deprecated_properties_are_dropped_and_nullable_refs_unwrapped() {
        let mut v = serde_json::json!({"$schema": "x", "properties": {
            "depth": {"description": "DEPRECATED: use expand."},
            "expand": {"anyOf": [{"$ref": "#/$defs/E"}, {"type": "null"}]}
        }});
        compact_schema(&mut v);
        assert_eq!(v, serde_json::json!({"properties": {"expand": {
            "$ref": "#/$defs/E",
            "description": "{includes|includedBy|references|referencedBy: depth}, 0 = unbounded"
        }}}));
    }

    #[test]
    fn frontmatter_flags_get_terse_hints() {
        let mut v = serde_json::json!({"properties": {
            "frontmatter": {"type": ["boolean", "null"], "description": "Lead each document's content with its stored frontmatter. Long."},
            "keep_frontmatter": {"type": ["boolean", "null"], "description": "Keep the stored frontmatter. Long."}
        }});
        compact_schema(&mut v);
        assert_eq!(v, serde_json::json!({"properties": {
            "frontmatter": {"type": "boolean", "description": "content leads with the stored frontmatter, verbatim"},
            "keep_frontmatter": {"type": "boolean", "description": "keep stored frontmatter; content = body only"}
        }}));
    }

    #[test]
    fn first_sentence_stops_at_the_period_and_caps_length() {
        assert_eq!(first_sentence("Read docs. Then more.", 90), "Read docs");
        assert_eq!(first_sentence("abcdef", 3), "abc…");
        assert_eq!(first_sentence("see file.md for details", 90), "see file.md for details");
    }
}
