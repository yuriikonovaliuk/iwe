//! Documentation / public-API shape check for iwe-lock, covering the
//! contract's acceptance criterion: "Zero dependency on kc's crate/package;
//! zero layer/assembly/compositor vocabulary anywhere in the crate's public
//! API, types, or doc comments."
//!
//! This is a *mechanical* check (word-boundary text scanning over the
//! crate's own manifest and `src/` tree), not a semantic one. It is a
//! best-effort automation of a criterion the contract itself flags as
//! possibly not fully automatable:
//!   - It can false-negative: a paraphrase that avoids these literal tokens
//!     (e.g. renaming "compositor" to some new synonym) would slip through.
//!   - It can false-positive: an unrelated, legitimate use of a common
//!     English word sharing a stem (e.g. "composite" for "composit") would
//!     trip the "assembl"/"composit" stem checks and need human judgment.
//! Any failure here should be treated as "go look," not as an infallible
//! verdict -- flag deviations for manual review rather than trusting this
//! test alone either way.
//!
//! "kc" here refers to the sibling `knowledge-compositor` package (the repo
//! at ~/projects/knowledge-compositor, informally "kc" in the task contract)
//! -- iwe-lock must depend on neither that package nor its vocabulary.

use std::fs;
use std::path::{Path, PathBuf};

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().map(|e| e == "rs").unwrap_or(false) {
            out.push(path);
        }
    }
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// True if `needle` occurs in `haystack` as a whole word (not as a substring
/// of a larger identifier/word on either side), e.g. `contains_word("a
/// player acquires", "layer")` is false but `contains_word("the layer
/// below", "layer")` is true.
fn contains_word(haystack: &str, needle: &str) -> bool {
    let hay: Vec<char> = haystack.chars().collect();
    let ndl: Vec<char> = needle.chars().collect();
    if ndl.is_empty() || hay.len() < ndl.len() {
        return false;
    }
    for start in 0..=(hay.len() - ndl.len()) {
        if hay[start..start + ndl.len()] == ndl[..] {
            let before_ok = start == 0 || !is_word_char(hay[start - 1]);
            let after_idx = start + ndl.len();
            let after_ok = after_idx == hay.len() || !is_word_char(hay[after_idx]);
            if before_ok && after_ok {
                return true;
            }
        }
    }
    false
}

/// True if `stem` occurs in `haystack` at the *start* of a word (so it also
/// matches longer words built on that stem, e.g. "assembl" matches
/// "assembly" and "assembling").
fn contains_word_stem(haystack: &str, stem: &str) -> bool {
    let hay: Vec<char> = haystack.chars().collect();
    let ndl: Vec<char> = stem.chars().collect();
    if ndl.is_empty() || hay.len() < ndl.len() {
        return false;
    }
    for start in 0..=(hay.len() - ndl.len()) {
        if hay[start..start + ndl.len()] == ndl[..] {
            let before_ok = start == 0 || !is_word_char(hay[start - 1]);
            if before_ok {
                return true;
            }
        }
    }
    false
}

struct Forbidden {
    label: &'static str,
    check: fn(&str) -> bool,
}

fn forbidden_checks() -> Vec<Forbidden> {
    vec![
        Forbidden {
            label: "'kc' (whole word)",
            check: |s| contains_word(s, "kc"),
        },
        Forbidden {
            label: "'layer' (whole word)",
            check: |s| contains_word(s, "layer"),
        },
        Forbidden {
            label: "'assembl*' (assembly/assembler/assembling)",
            check: |s| contains_word_stem(s, "assembl"),
        },
        Forbidden {
            label: "'composit*' (compositor/composition/composite)",
            check: |s| contains_word_stem(s, "composit"),
        },
        Forbidden {
            label: "'knowledge-compositor' / 'knowledge_compositor'",
            check: |s| s.contains("knowledge-compositor") || s.contains("knowledge_compositor"),
        },
    ]
}

#[test]
fn public_api_and_docs_are_free_of_kc_vocabulary() {
    let src_dir = manifest_dir().join("src");
    let mut files = Vec::new();
    collect_rs_files(&src_dir, &mut files);
    assert!(
        !files.is_empty(),
        "expected at least one .rs file under {} (iwe-lock crate not present yet, or restructured away from src/)",
        src_dir.display()
    );

    let checks = forbidden_checks();
    let mut violations = Vec::new();
    for file in &files {
        let text = fs::read_to_string(file).expect("read source file");
        let lower = text.to_lowercase();
        for check in &checks {
            if (check.check)(&lower) {
                violations.push(format!("{}: matches forbidden token {}", file.display(), check.label));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "found kc/layer/assembly/compositor vocabulary in iwe-lock's own source \
         (public API, types, or doc comments are expected to be free of it):\n{}",
        violations.join("\n")
    );
}

#[test]
fn manifest_has_no_dependency_on_kc_crate() {
    let manifest_path = manifest_dir().join("Cargo.toml");
    let text = fs::read_to_string(&manifest_path).expect("read Cargo.toml");
    let lower = text.to_lowercase();
    assert!(
        !lower.contains("knowledge-compositor") && !lower.contains("knowledge_compositor"),
        "iwe-lock's Cargo.toml appears to reference the knowledge-compositor (\"kc\") package; \
         iwe-lock must have zero dependency on it"
    );
}
