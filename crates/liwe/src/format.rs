use crate::graph::Reader;
use crate::markdown::{MarkdownReader, MarkdownWriter};
use crate::model::config::{FormatOptions, MarkdownOptions};
use crate::model::document::Document;
use crate::model::writer::Blocks;

#[cfg(feature = "djot")]
use crate::model::config::DjotOptions;

/// The format boundary: a reader/writer pair for one document format, constructed with its own
/// options. Markdown is always built in; djot lives behind the `djot` feature. A consumer can
/// implement this trait to inject another format.
pub trait DocumentFormat {
    fn read(&self, content: &str) -> Document;
    fn write(&self, blocks: &Blocks) -> String;
    fn write_skip_frontmatter(&self, blocks: &Blocks) -> String;
}

pub struct MarkdownFormat {
    options: MarkdownOptions,
}

impl MarkdownFormat {
    pub fn new(options: MarkdownOptions) -> Self {
        Self { options }
    }
}

impl DocumentFormat for MarkdownFormat {
    fn read(&self, content: &str) -> Document {
        MarkdownReader::new().document(content, &self.options)
    }

    fn write(&self, blocks: &Blocks) -> String {
        MarkdownWriter::new().write(blocks, &self.options)
    }

    fn write_skip_frontmatter(&self, blocks: &Blocks) -> String {
        MarkdownWriter::new().write_skip_frontmatter(blocks, &self.options)
    }
}

#[cfg(feature = "djot")]
pub struct DjotFormat {
    options: DjotOptions,
}

#[cfg(feature = "djot")]
impl DjotFormat {
    pub fn new(options: DjotOptions) -> Self {
        Self { options }
    }
}

#[cfg(feature = "djot")]
impl DocumentFormat for DjotFormat {
    fn read(&self, content: &str) -> Document {
        crate::djot::DjotReader::new().document(content, &self.options)
    }

    fn write(&self, blocks: &Blocks) -> String {
        crate::djot::DjotWriter::new().write(blocks, &self.options)
    }

    fn write_skip_frontmatter(&self, blocks: &Blocks) -> String {
        crate::djot::DjotWriter::new().write_skip_frontmatter(blocks, &self.options)
    }
}

/// Build the built-in [`DocumentFormat`] for the given options. Djot resolves only when the
/// `djot` feature is enabled; otherwise it falls back to markdown.
pub fn format_for(format: &FormatOptions) -> Box<dyn DocumentFormat> {
    match format {
        FormatOptions::Markdown(options) => Box::new(MarkdownFormat::new(options.clone())),
        #[cfg(feature = "djot")]
        FormatOptions::Djot(options) => Box::new(DjotFormat::new(options.clone())),
        #[cfg(not(feature = "djot"))]
        FormatOptions::Djot(_) => Box::new(MarkdownFormat::new(MarkdownOptions::default())),
    }
}

pub fn read_document(content: &str, format: &FormatOptions) -> Document {
    match format {
        FormatOptions::Markdown(options) => MarkdownReader::new().document(content, options),
        #[cfg(feature = "djot")]
        FormatOptions::Djot(options) => crate::djot::DjotReader::new().document(content, options),
        #[cfg(not(feature = "djot"))]
        FormatOptions::Djot(_) => {
            MarkdownReader::new().document(content, &MarkdownOptions::default())
        }
    }
}

pub fn write_document(blocks: &Blocks, format: &FormatOptions) -> String {
    match format {
        FormatOptions::Markdown(options) => MarkdownWriter::new().write(blocks, options),
        #[cfg(feature = "djot")]
        FormatOptions::Djot(options) => crate::djot::DjotWriter::new().write(blocks, options),
        #[cfg(not(feature = "djot"))]
        FormatOptions::Djot(_) => MarkdownWriter::new().write(blocks, &MarkdownOptions::default()),
    }
}

pub fn write_document_skip_frontmatter(blocks: &Blocks, format: &FormatOptions) -> String {
    match format {
        FormatOptions::Markdown(options) => {
            MarkdownWriter::new().write_skip_frontmatter(blocks, options)
        }
        #[cfg(feature = "djot")]
        FormatOptions::Djot(options) => {
            crate::djot::DjotWriter::new().write_skip_frontmatter(blocks, options)
        }
        #[cfg(not(feature = "djot"))]
        FormatOptions::Djot(_) => {
            MarkdownWriter::new().write_skip_frontmatter(blocks, &MarkdownOptions::default())
        }
    }
}

/// Byte length of the leading frontmatter block of `content` (opening fence
/// through the closing fence line), exactly as the format's reader
/// recognizes it; `None` when the document has no frontmatter.
pub fn frontmatter_block_len(content: &str, format: &FormatOptions) -> Option<usize> {
    match format {
        #[cfg(feature = "djot")]
        FormatOptions::Djot(_) => crate::djot::reader::frontmatter_block_len(content),
        _ => markdown_frontmatter_block_len(content),
    }
}

/// The markdown reader's rule: a leading YAML metadata block, or the empty
/// `---`/`---` pair it special-cases.
fn markdown_frontmatter_block_len(content: &str) -> Option<usize> {
    for empty in ["---\n---\n", "---\r\n---\r\n", "---\n---", "---\r\n---"] {
        if content.starts_with(empty) && (empty.ends_with('\n') || content.len() == empty.len()) {
            return Some(empty.len());
        }
    }
    crate::model::split_raw_frontmatter(content)
        .0
        .map(|block| block.len())
}

/// The stored leading frontmatter of `content`, verbatim: the frontmatter
/// block plus the blank lines that separate it from the body, so that
/// `prefix + body` restores the document. Empty when there is none.
pub fn frontmatter_prefix<'a>(content: &'a str, format: &FormatOptions) -> &'a str {
    let Some(mut end) = frontmatter_block_len(content, format) else {
        return "";
    };
    while let Some(line_len) = content[end..].find('\n').map(|i| i + 1) {
        if !content[end..end + line_len].trim().is_empty() {
            break;
        }
        end += line_len;
    }
    &content[..end]
}

/// Whether `content` opens with a frontmatter block.
pub fn has_frontmatter(content: &str, format: &FormatOptions) -> bool {
    read_document(content, format).frontmatter.is_some()
}

#[cfg(test)]
mod frontmatter_prefix_tests {
    use super::*;

    fn markdown() -> FormatOptions {
        FormatOptions::Markdown(MarkdownOptions::default())
    }

    #[test]
    fn prefix_is_the_block_and_the_blank_lines_after_it_verbatim() {
        let content = "---\ntags: [a, b]   # kept as written\nstatus: open\n---\n\n\n# Title\n\nBody\n";
        assert_eq!(
            frontmatter_prefix(content, &markdown()),
            "---\ntags: [a, b]   # kept as written\nstatus: open\n---\n\n\n"
        );
    }

    #[test]
    fn prefix_is_empty_without_frontmatter() {
        assert_eq!(frontmatter_prefix("# Title\n\n---\n\nafter a rule\n", &markdown()), "");
        assert_eq!(frontmatter_prefix("", &markdown()), "");
        assert!(!has_frontmatter("# Title\n", &markdown()));
    }

    #[test]
    fn empty_and_body_less_blocks_are_recognized() {
        assert_eq!(frontmatter_prefix("---\n---\n# T\n", &markdown()), "---\n---\n");
        assert_eq!(frontmatter_prefix("---\na: 1\n---\n", &markdown()), "---\na: 1\n---\n");
        assert_eq!(frontmatter_prefix("---\na: 1\n---", &markdown()), "---\na: 1\n---");
    }

    #[test]
    fn crlf_blocks_keep_their_line_endings() {
        let content = "---\r\na: 1\r\n---\r\n\r\n# T\r\n";
        assert_eq!(frontmatter_prefix(content, &markdown()), "---\r\na: 1\r\n---\r\n\r\n");
    }

    #[cfg(feature = "djot")]
    #[test]
    fn djot_prefix_is_the_block_verbatim() {
        let djot = FormatOptions::Djot(DjotOptions::default());
        let content = "---\nstatus: open # c\n---\n\n# Title\n";
        assert_eq!(frontmatter_prefix(content, &djot), "---\nstatus: open # c\n---\n\n");
        assert_eq!(frontmatter_prefix("# Title\n", &djot), "");
    }

    #[test]
    fn has_frontmatter_follows_the_reader() {
        assert!(has_frontmatter("---\na: 1\n---\n# T\n", &markdown()));
        assert!(has_frontmatter("---\n---\n# T\n", &markdown()));
    }
}
