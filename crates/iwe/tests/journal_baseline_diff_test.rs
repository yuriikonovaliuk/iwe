// AC3: "journal.path unset (default) -> ... assert zero output
// differences anywhere in that suite versus a baseline run without your
// changes -- this is a stronger bar than 'tests still pass,' it's
// 'behavior is byte-identical.' Set up the baseline comparison
// explicitly."
//
// The comparison mechanism itself lives in
// `scripts/journal_baseline_diff.sh` (BASE_REF, HEAD_REF -> builds two
// disposable worktrees, runs the full workspace test suite in each with
// `journal.path` unconfigured, normalizes away known non-deterministic
// noise -- per-run timings, temp-directory paths, build-hash-bearing
// binary names -- and diffs the result byte-for-byte). Running it for
// real needs two full workspace builds and is too expensive to pay on
// every `cargo test`; the `#[ignore]`d test below does pay that cost and
// is meant for CI / a deliberate manual run once a Developer's
// implementation exists to compare against a pre-journal baseline.
//
// The always-on tests here exercise the normalization rules themselves
// fast, without a build, so the comparison mechanism's correctness isn't
// only trusted by inspection.

use std::process::{Command, Output};
use tempfile::TempDir;

fn script_path() -> std::path::PathBuf {
    let mut path = std::env::current_dir().expect("cwd");
    while !path.join("Cargo.toml").exists() || !path.join("crates").exists() {
        assert!(path.pop(), "could not find workspace root");
    }
    path.join("scripts").join("journal_baseline_diff.sh")
}

fn normalize(text: &str) -> String {
    let temp = TempDir::new().expect("tempdir");
    let file = temp.path().join("sample.out");
    std::fs::write(&file, text).unwrap();
    let output: Output = Command::new("bash")
        .arg(script_path())
        .arg("--normalize")
        .arg(&file)
        .output()
        .expect("run journal_baseline_diff.sh --normalize");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("valid UTF-8")
}

#[test]
fn per_run_timing_is_normalized_away() {
    let a = normalize("test result: ok. 3 passed; 0 failed; finished in 0.02s\n");
    let b = normalize("test result: ok. 3 passed; 0 failed; finished in 1.87s\n");
    assert_eq!(a, b);
    assert!(a.contains("finished in Ns"));
}

#[test]
fn build_hash_bearing_binary_names_are_normalized_away() {
    let a = normalize("Running unittests src/lib.rs (target/debug/deps/iwe-a1b2c3d4e5f60718)\n");
    let b = normalize("Running unittests src/lib.rs (target/debug/deps/iwe-00112233445566ff)\n");
    assert_eq!(a, b);
}

#[test]
fn temp_directory_paths_are_normalized_away() {
    let a = normalize("wrote to /tmp/.tmpAbC123xyz/note.md\n");
    let b = normalize("wrote to /tmp/.tmpZzZ999qqq/note.md\n");
    assert_eq!(a, b);
}

#[test]
fn a_genuine_behavior_difference_is_not_normalized_away() {
    let a = normalize("test tests::foo ... ok\n");
    let b = normalize("test tests::foo ... FAILED\n");
    assert_ne!(
        a, b,
        "normalization must not paper over an actual pass/fail difference"
    );
}

#[test]
fn identical_input_normalizes_identically() {
    let text = "test tests::foo ... ok\ntest result: ok. 1 passed; 0 failed; finished in 0.11s\n";
    assert_eq!(normalize(text), normalize(text));
}

/// The real end-to-end comparison: two disposable worktrees, two full
/// `cargo test --workspace` runs, one diff. Expensive (two full builds)
/// and requires network/local access to resolve BASE_REF/HEAD_REF as git
/// refs, so this is `#[ignore]`d by default. Run explicitly:
///
///   cargo test --test integration journal_baseline_diff_smoke -- --ignored
///
/// which, absent any command-line refs, compares HEAD against itself --
/// proving the *mechanism* reports zero differences when there is
/// genuinely nothing to diff. Comparing a real pre-journal base ref
/// against a Developer's implementation branch is a separate, deliberate
/// invocation of the same script (`scripts/journal_baseline_diff.sh
/// <base> <head>`), documented in this task's test report rather than
/// hardcoded here, since this crate has no way to know that branch's name.
#[test]
#[ignore]
fn journal_baseline_diff_smoke() {
    let output = Command::new("bash")
        .arg(script_path())
        .arg("HEAD")
        .arg("HEAD")
        .current_dir(repo_root())
        .output()
        .expect("run journal_baseline_diff.sh");
    assert!(
        output.status.success(),
        "HEAD vs HEAD must diff as identical; stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn repo_root() -> std::path::PathBuf {
    let mut path = std::env::current_dir().expect("cwd");
    while !path.join("Cargo.toml").exists() || !path.join("crates").exists() {
        assert!(path.pop(), "could not find workspace root");
    }
    path
}
