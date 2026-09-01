use pulldown_cmark::{Event, Parser, Tag};

use crate::markdown::reader::PARSER_OPTIONS;
use crate::model::{frontmatter_to_string, Frontmatter};

pub fn split_raw_frontmatter(content: &str) -> (Option<&str>, &str) {
    match leading_metadata_block_end(content) {
        Some(end) => (Some(&content[..end]), &content[end..]),
        None => (None, content),
    }
}

/// Parses `content`'s leading frontmatter block (if any) into a YAML
/// mapping, reusing [`split_raw_frontmatter`]'s pulldown_cmark-based
/// boundary detection rather than naive `---` delimiter stripping — so the
/// same edge cases `split_raw_frontmatter` already handles (CRLF,
/// `...`-closed blocks, a lone `---` that is actually a thematic break, no
/// trailing newline) are handled identically here. The slice
/// `split_raw_frontmatter` returns includes the delimiter lines themselves
/// (e.g. `"---\ntype: note\n---\n"`), which is not by itself valid
/// standalone YAML — feeding it straight to a YAML parser fails, because
/// the second `---` line reads as the start of another document — so this
/// re-parses just that isolated slice with the same metadata-block-aware
/// parser used when reading markdown normally (`liwe::markdown::reader`),
/// rather than string-slicing the delimiters off by hand.
///
/// Returns `None` when `content` has no leading frontmatter block, and
/// `Some` (possibly an empty mapping, if the block is empty or is not a
/// YAML mapping) when it does — the same "absent vs. present-but-empty"
/// shape `split_raw_frontmatter` itself distinguishes.
pub fn parse_leading_frontmatter(content: &str) -> Option<Frontmatter> {
    let (front, _) = split_raw_frontmatter(content);
    let front = front?;
    let normalized = if front.ends_with('\n') {
        front.to_string()
    } else {
        format!("{front}\n")
    };
    let mut events = Parser::new_ext(&normalized, PARSER_OPTIONS).into_offset_iter();
    let starts_metadata_block = matches!(
        events.next(),
        Some((Event::Start(Tag::MetadataBlock(_)), range)) if range.start == 0
    );
    if !starts_metadata_block {
        return Some(Frontmatter::new());
    }
    let text = match events.next() {
        Some((Event::Text(text), _)) => text,
        _ => return Some(Frontmatter::new()),
    };
    match serde_yaml::from_str::<serde_yaml::Value>(&text) {
        Ok(serde_yaml::Value::Mapping(mapping)) => Some(mapping),
        _ => Some(Frontmatter::new()),
    }
}

pub fn prepend_frontmatter(
    frontmatter: Option<Frontmatter>,
    rendered: &str,
) -> Result<String, String> {
    let mapping = match frontmatter {
        Some(mapping) => mapping,
        None => return Ok(rendered.to_string()),
    };

    if mapping.is_empty() {
        return Ok(rendered.to_string());
    }

    if leading_metadata_block_end(rendered).is_some() {
        return Err(
            "the document already begins with a frontmatter block, it would be written twice; \
             drop the frontmatter fields, or pass the complete document as content"
                .to_string(),
        );
    }

    Ok(format!(
        "---\n{}\n---\n\n{}",
        frontmatter_to_string(&mapping),
        rendered
    ))
}

fn leading_metadata_block_end(content: &str) -> Option<usize> {
    if let Some(end) = metadata_block_end(content) {
        return Some(end);
    }

    if content.is_empty() || content.ends_with('\n') {
        return None;
    }

    let terminated = format!("{}\n", content);
    metadata_block_end(&terminated).map(|end| end.min(content.len()))
}

fn metadata_block_end(content: &str) -> Option<usize> {
    match Parser::new_ext(content, PARSER_OPTIONS)
        .into_offset_iter()
        .next()
    {
        Some((Event::Start(Tag::MetadataBlock(_)), range)) if range.start == 0 => {
            Some(range.end + line_ending_len(&content[range.end..]))
        }
        _ => None,
    }
}

fn line_ending_len(rest: &str) -> usize {
    if rest.starts_with("\r\n") {
        2
    } else if rest.starts_with('\n') {
        1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_yaml::Value;

    fn mapping(yaml: &str) -> Frontmatter {
        match serde_yaml::from_str::<Value>(yaml).unwrap() {
            Value::Mapping(m) => m,
            _ => panic!("expected a mapping"),
        }
    }

    #[test]
    fn splits_dash_closed_block() {
        assert_eq!(
            split_raw_frontmatter("---\ntype: note\n---\n\nBody\n"),
            (Some("---\ntype: note\n---\n"), "\nBody\n")
        );
    }

    #[test]
    fn splits_dot_closed_block() {
        assert_eq!(
            split_raw_frontmatter("---\ntype: note\n...\n\nBody\n"),
            (Some("---\ntype: note\n...\n"), "\nBody\n")
        );
    }

    #[test]
    fn splits_crlf_block() {
        assert_eq!(
            split_raw_frontmatter("---\r\ntype: note\r\n---\r\n\r\nBody\r\n"),
            (Some("---\r\ntype: note\r\n---\r\n"), "\r\nBody\r\n")
        );
    }

    #[test]
    fn splits_block_without_trailing_newline() {
        assert_eq!(
            split_raw_frontmatter("---\ntype: note\n---"),
            (Some("---\ntype: note\n---"), "")
        );
    }

    #[test]
    fn keeps_lone_thematic_break() {
        assert_eq!(
            split_raw_frontmatter("---\n\nBody\n"),
            (None, "---\n\nBody\n")
        );
    }

    #[test]
    fn keeps_two_thematic_breaks() {
        assert_eq!(
            split_raw_frontmatter("---\n\n---\n\nBody\n"),
            (None, "---\n\n---\n\nBody\n")
        );
    }

    #[test]
    fn keeps_unterminated_block() {
        assert_eq!(
            split_raw_frontmatter("---\ntype: note\n\nBody\n"),
            (None, "---\ntype: note\n\nBody\n")
        );
    }

    #[test]
    fn keeps_block_below_the_first_line() {
        assert_eq!(
            split_raw_frontmatter("# Title\n\n---\ntype: note\n---\n"),
            (None, "# Title\n\n---\ntype: note\n---\n")
        );
    }

    #[test]
    fn keeps_empty_input() {
        assert_eq!(split_raw_frontmatter(""), (None, ""));
    }

    #[test]
    fn parses_dash_closed_block() {
        assert_eq!(
            parse_leading_frontmatter("---\ntype: note\n---\n\nBody\n"),
            Some(mapping("type: note\n"))
        );
    }

    #[test]
    fn parses_dot_closed_block() {
        assert_eq!(
            parse_leading_frontmatter("---\ntype: note\n...\n\nBody\n"),
            Some(mapping("type: note\n"))
        );
    }

    #[test]
    fn parses_crlf_block() {
        assert_eq!(
            parse_leading_frontmatter("---\r\ntype: note\r\n---\r\n\r\nBody\r\n"),
            Some(mapping("type: note\n"))
        );
    }

    #[test]
    fn parses_block_without_trailing_newline() {
        assert_eq!(
            parse_leading_frontmatter("---\ntype: note\n---"),
            Some(mapping("type: note\n"))
        );
    }

    #[test]
    fn parses_absent_block_as_none() {
        assert_eq!(parse_leading_frontmatter("# Title\n\nBody\n"), None);
        assert_eq!(parse_leading_frontmatter("---\n\nBody\n"), None);
    }

    #[test]
    fn parses_lone_thematic_break_as_no_frontmatter() {
        // Mirrors `keeps_two_thematic_breaks`: `split_raw_frontmatter`
        // does not treat this as a metadata block either, so there is no
        // frontmatter to parse.
        assert_eq!(parse_leading_frontmatter("---\n\n---\n\nBody\n"), None);
    }

    #[test]
    fn prepends_nothing_for_absent_mapping() {
        assert_eq!(
            prepend_frontmatter(None, "# Title\n"),
            Ok("# Title\n".to_string())
        );
    }

    #[test]
    fn prepends_nothing_for_empty_mapping() {
        assert_eq!(
            prepend_frontmatter(Some(Frontmatter::new()), "# Title\n"),
            Ok("# Title\n".to_string())
        );
    }

    #[test]
    fn prepends_prefixed_keys() {
        assert_eq!(
            prepend_frontmatter(Some(mapping("_internal: 1\n$x: 2\n")), "# Title\n"),
            Ok("---\n_internal: 1\n$x: 2\n---\n\n# Title\n".to_string())
        );
    }

    #[test]
    fn prepends_fenced_mapping() {
        assert_eq!(
            prepend_frontmatter(Some(mapping("type: note\ntags:\n- demo\n")), "# Title\n"),
            Ok("---\ntype: note\ntags:\n- demo\n---\n\n# Title\n".to_string())
        );
    }

    #[test]
    fn prepends_mapping_with_every_key() {
        assert_eq!(
            prepend_frontmatter(Some(mapping("_internal: 1\ntype: note\n")), "# Title\n"),
            Ok("---\n_internal: 1\ntype: note\n---\n\n# Title\n".to_string())
        );
    }

    #[test]
    fn rejects_document_with_leading_block() {
        assert_eq!(
            prepend_frontmatter(
                Some(mapping("type: note\n")),
                "---\nother: 1\n---\n\n# Title\n"
            ),
            Err(
                "the document already begins with a frontmatter block, it would be written twice; \
                 drop the frontmatter fields, or pass the complete document as content"
                    .to_string()
            )
        );
    }
}
