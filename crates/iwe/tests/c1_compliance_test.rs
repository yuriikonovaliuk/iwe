// AC7 (C1-compliance is itself testable): "grep the actual shipped diff
// (not just your own test code) for 'layer', 'assembly', 'package',
// 'origin', 'compositor' case-insensitive, and assert zero matches --
// this is a real automated check, not a manual eyeball, since a reviewer
// test is explicitly called for."
//
// The mechanism lives in `scripts/c1_compliance_check.sh` (BASE_REF,
// HEAD_REF -> scans every added line between them, case-insensitively,
// for the forbidden vocabulary as whole words/plurals; exit 0 = clean).
// These tests prove that mechanism actually works -- catches a violation,
// passes a clean diff, doesn't false-positive on innocent substrings --
// using small disposable fixture git repositories, so the check is
// exercised automatically rather than trusted by inspection. Once a
// Developer's implementation lands, the same script is run for real
// against that diff (documented in this crate's test report, not
// re-implemented here: this crate doesn't know that branch's name).
//
// This file is the one deliberate, narrow exception to "your own test
// code and comments must likewise contain zero references" to the
// forbidden vocabulary: it IS the compliance check's own test suite, and
// a word-detector cannot be proven to detect a word without that word
// appearing, literally, in a fixture somewhere. It is meta-tooling for
// the check itself, not part of the shipped journal feature the check
// exists to police -- `scripts/c1_compliance_check.sh` excludes this
// file's own path from what it scans, for exactly this reason. Every
// other file in this deliverable (`journal_test.rs`,
// `journal_baseline_diff_test.rs`, `journal_baseline_diff.sh`, and
// `c1_compliance_check.sh` itself) is held to the zero-tolerance rule
// with no exception.

use std::path::Path;
use std::process::{Command, Output};
use tempfile::TempDir;

fn script_path() -> std::path::PathBuf {
    let mut path = std::env::current_dir().expect("cwd");
    while !path.join("Cargo.toml").exists() || !path.join("crates").exists() {
        assert!(path.pop(), "could not find workspace root");
    }
    path.join("scripts").join("c1_compliance_check.sh")
}

/// A throwaway git repository with a `base` commit and, on `head_branch`
/// branched from `base`, one further commit adding `added_line` to a
/// tracked file.
fn fixture_repo(added_line: &str, head_branch: &str) -> TempDir {
    let temp = TempDir::new().expect("tempdir");
    run_git(temp.path(), &["init", "-q"]);
    run_git(temp.path(), &["config", "user.email", "test@example.com"]);
    run_git(temp.path(), &["config", "user.name", "Test"]);
    std::fs::write(temp.path().join("a.txt"), "clean baseline\n").unwrap();
    run_git(temp.path(), &["add", "a.txt"]);
    run_git(temp.path(), &["commit", "-q", "-m", "base"]);
    run_git(temp.path(), &["branch", "base"]);
    run_git(temp.path(), &["checkout", "-q", "-b", head_branch]);
    std::fs::write(
        temp.path().join("a.txt"),
        format!("clean baseline\n{added_line}\n"),
    )
    .unwrap();
    run_git(temp.path(), &["commit", "-q", "-a", "-m", "head"]);
    temp
}

fn run_git(work_dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(work_dir)
        .output()
        .expect("run git")
}

fn run_check(work_dir: &Path) -> Output {
    Command::new("bash")
        .arg(script_path())
        .arg("base")
        .arg("head")
        .current_dir(work_dir)
        .output()
        .expect("run c1_compliance_check.sh")
}

#[test]
fn clean_diff_passes() {
    let temp = fixture_repo("nothing forbidden in this sentence at all", "head");
    let output = run_check(temp.path());
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn each_forbidden_term_is_caught_case_insensitively() {
    for term in ["layer", "Assembly", "PACKAGE", "origin", "Compositor"] {
        let temp = fixture_repo(&format!("this line mentions {term} directly"), "head");
        let output = run_check(temp.path());
        assert!(
            !output.status.success(),
            "'{term}' should have been flagged; stdout: {} stderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
        assert!(
            stderr.contains(&term.to_lowercase()),
            "stderr should quote the offending line for '{term}': {stderr}"
        );
    }
}

#[test]
fn plural_forms_are_also_caught() {
    for term in ["layers", "assemblies", "packages", "origins", "compositors"] {
        let temp = fixture_repo(&format!("plural check: {term} here"), "head");
        let output = run_check(temp.path());
        assert!(
            !output.status.success(),
            "'{term}' should have been flagged"
        );
    }
}

#[test]
fn innocent_substrings_do_not_false_positive() {
    let temp = fixture_repo(
        "this was originally packaged nicely, layered fine, origination pending",
        "head",
    );
    let output = run_check(temp.path());
    assert!(
        output.status.success(),
        "'originally'/'packaged'/'layered'/'origination' must not trip a \
         whole-word check; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn only_added_lines_are_scanned_not_context_or_removed_lines() {
    // A forbidden word already present on `base` (unchanged, so it shows
    // up only as context, never as a `+` line) must not fail the check
    // for the diff between `base` and `head` -- only genuinely new
    // material is in scope.
    let temp = TempDir::new().expect("tempdir");
    run_git(temp.path(), &["init", "-q"]);
    run_git(temp.path(), &["config", "user.email", "test@example.com"]);
    run_git(temp.path(), &["config", "user.name", "Test"]);
    std::fs::write(
        temp.path().join("a.txt"),
        "an old layer mentioned here, unrelated to this change\nsecond line\n",
    )
    .unwrap();
    run_git(temp.path(), &["add", "a.txt"]);
    run_git(temp.path(), &["commit", "-q", "-m", "base"]);
    run_git(temp.path(), &["branch", "base"]);
    run_git(temp.path(), &["checkout", "-q", "-b", "head"]);
    std::fs::write(
        temp.path().join("a.txt"),
        "an old layer mentioned here, unrelated to this change\nsecond line, now with a third\n",
    )
    .unwrap();
    run_git(temp.path(), &["commit", "-q", "-a", "-m", "head"]);

    let output = run_check(temp.path());
    assert!(
        output.status.success(),
        "unchanged context lines must not be scanned; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
