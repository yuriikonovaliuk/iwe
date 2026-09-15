// T5 (efforts/crew-and-iwe-memory-footprint-reduction/iwec-shared-daemon,
// pass 2): "Registration script: ~/.claude.json iwe/mind entries ->
// shared-daemon HTTP URLs".
//
// Acceptance criteria under test (verbatim from the contract):
//   1. "A committed, idempotent script rewrites exactly the iwe and mind
//      entries of a Claude config to {"type":"http","url":"http://
//      127.0.0.1:8765/mcp"} and {"type":"http","url":"http://
//      127.0.0.1:8766/mcp"} respectively, removing the stdio
//      command/args."
//   2. "Script accepts a config-path override (default ~/.claude.json),
//      backs up before first modification; second run against the same
//      file produces zero diff; no key other than iwe/mind changes."
//   3. "The live flip is NOT executed by this task's own tests against
//      the real ~/.claude.json -- use a scratch/fixture copy only, never
//      the real file."
//   4. "Scope is the global ~/.claude.json only."
//   5. "Commit message carries the pass-1 narrow-change carveout note."
//
// Interface note: the contract's Shared surface ("New script in the iwe
// repo") pins the exact port URLs and JSON shape but does NOT pin the
// script's filename or its CLI argument convention. Rather than guess a
// name (the M6-b failure mode: a test targeting a name Developer never
// builds), the script is *discovered* under `scripts/` by the one thing
// that genuinely is pinned -- it must reference both daemon ports,
// 8765 and 8766, literally. The path-override flag itself
// (`--config CONFIG`) is not guessed either: it is read from the
// script's own `argparse` usage/error text (its public CLI surface,
// observed black-box, the same way any CLI's --help would be), not from
// reading its source. No behavior asserted below comes from the
// implementation -- only this one piece of wiring needed to invoke it
// at all, which the Shared surface left unpinned.
//
// Criterion 5 (commit message convention) is not something an automated
// test can check from inside this crate -- it is verified at commit
// time/review, not encoded here.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{json, Value};
use tempfile::TempDir;

fn repo_root() -> PathBuf {
    let mut path = std::env::current_dir().expect("cwd");
    while !path.join("Cargo.toml").exists() || !path.join("crates").exists() {
        assert!(path.pop(), "could not find workspace root");
    }
    path
}

/// Every file directly under `scripts/` whose content references both
/// daemon ports pinned by the contract (8765 for iwe, 8766 for mind).
/// This is content-based discovery, not a guessed filename. Files whose
/// own name marks them as a test (conventional `test_*`/`*_test.*`
/// naming, not a peek at their content) are excluded, since Developer's
/// own test file for the same script naturally shares those ports too.
fn find_registration_scripts() -> Vec<PathBuf> {
    let scripts_dir = repo_root().join("scripts");
    let mut hits = Vec::new();
    if let Ok(entries) = fs::read_dir(&scripts_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let file_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            if path.is_file() && !file_name.contains("test") {
                if let Ok(content) = fs::read_to_string(&path) {
                    if content.contains("8765") && content.contains("8766") {
                        hits.push(path);
                    }
                }
            }
        }
    }
    hits
}

/// The one script under `scripts/` this task's tests exercise. Panics
/// (failing every test below) if it is absent or ambiguous -- expected
/// and left as-is before Developer's T5 implementation exists.
fn registration_script() -> PathBuf {
    let mut hits = find_registration_scripts();
    assert_eq!(
        hits.len(),
        1,
        "expected exactly one file under scripts/ referencing both port \
         8765 and port 8766 (the T5 iwe/mind registration script); found {}: {:?}. \
         If this is 0, Developer's T5 script does not exist yet. If this is >1, \
         the discovery heuristic in this test file is now ambiguous and needs \
         narrowing.",
        hits.len(),
        hits
    );
    hits.remove(0)
}

fn expected_iwe() -> Value {
    json!({"type": "http", "url": "http://127.0.0.1:8765/mcp"})
}

fn expected_mind() -> Value {
    json!({"type": "http", "url": "http://127.0.0.1:8766/mcp"})
}

/// A realistic ~/.claude.json: global mcpServers (iwe, mind, and two
/// unrelated stdio servers that must survive untouched), plus
/// per-project overrides and assorted top-level settings keys -- all of
/// which must come out byte-for-byte-equivalent (as JSON) except for the
/// two entries under test.
fn fixture() -> Value {
    json!({
        "numStartups": 87,
        "installMethod": "native",
        "autoUpdates": true,
        "theme": "dark-daltonized",
        "hasCompletedOnboarding": true,
        "lastReleaseNotesSeen": "1.2.3",
        "mcpServers": {
            "iwe": {
                "command": "/home/yurii/.local/bin/iwe-memory-mcp",
                "args": []
            },
            "mind": {
                "command": "/home/yurii/.local/bin/mind-memory-mcp",
                "args": ["--store", "mind"]
            },
            "filesystem": {
                "command": "npx",
                "args": ["-y", "@modelcontextprotocol/server-filesystem", "/home/yurii"]
            },
            "sequential-thinking": {
                "command": "npx",
                "args": ["-y", "@modelcontextprotocol/server-sequential-thinking"]
            }
        },
        "projects": {
            "/home/yurii/projects/iwe": {
                "allowedTools": ["Bash(git *)"],
                "mcpServers": {
                    "local-tool": {
                        "command": "./scripts/local-mcp.sh",
                        "args": []
                    }
                },
                "history": [{"display": "run tests", "pastedContents": {}}]
            },
            "/home/yurii/projects/multi-agent-orchestration": {
                "mcpServers": {},
                "dontCrawlDirectory": true
            }
        }
    })
}

fn write_fixture(dir: &Path) -> PathBuf {
    let config_path = dir.join("claude.json");
    fs::write(
        &config_path,
        serde_json::to_string_pretty(&fixture()).unwrap(),
    )
    .unwrap();
    config_path
}

fn run_script(script: &Path, config_path: &Path) -> Output {
    Command::new(script)
        .arg("--config")
        .arg(config_path)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {:?}: {e}", script))
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap())
        .unwrap_or_else(|e| panic!("{:?} is not valid JSON after script ran: {e}", path))
}

// AC1 + AC2 ("no key other than iwe/mind changes"): first run flips
// exactly mcpServers.iwe and mcpServers.mind to the pinned http shape,
// stripping command/args, and leaves every other key -- other
// mcpServers entries, top-level settings, per-project overrides --
// unchanged.
#[test]
fn first_run_flips_iwe_and_mind_and_preserves_every_other_key() {
    let script = registration_script();
    let dir = TempDir::new().unwrap();
    let config_path = write_fixture(dir.path());

    let output = run_script(&script, &config_path);
    assert!(
        output.status.success(),
        "first run should succeed; stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let result = read_json(&config_path);

    assert_eq!(
        result["mcpServers"]["iwe"],
        expected_iwe(),
        "mcpServers.iwe must become exactly the pinned http shape (stdio command/args gone)"
    );
    assert_eq!(
        result["mcpServers"]["mind"],
        expected_mind(),
        "mcpServers.mind must become exactly the pinned http shape (stdio command/args gone)"
    );

    let mut expected_full = fixture();
    expected_full["mcpServers"]["iwe"] = expected_iwe();
    expected_full["mcpServers"]["mind"] = expected_mind();
    assert_eq!(
        result, expected_full,
        "no key other than mcpServers.iwe/mind may change anywhere in the config \
         (other mcpServers entries, top-level settings, per-project overrides)"
    );
}

// AC1: explicitly names the "removing the stdio command/args" clause,
// independent of the full-object comparison above.
#[test]
fn stdio_command_and_args_are_removed_from_iwe_and_mind() {
    let script = registration_script();
    let dir = TempDir::new().unwrap();
    let config_path = write_fixture(dir.path());

    let output = run_script(&script, &config_path);
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));

    let result = read_json(&config_path);
    for name in ["iwe", "mind"] {
        let entry = result["mcpServers"][name]
            .as_object()
            .unwrap_or_else(|| panic!("mcpServers.{name} must still be an object"));
        assert!(
            !entry.contains_key("command"),
            "mcpServers.{name} must not retain a stdio 'command' key"
        );
        assert!(
            !entry.contains_key("args"),
            "mcpServers.{name} must not retain a stdio 'args' key"
        );
    }
}

// AC2: "second run against the same file produces zero diff."
#[test]
fn second_run_against_the_same_file_produces_zero_diff() {
    let script = registration_script();
    let dir = TempDir::new().unwrap();
    let config_path = write_fixture(dir.path());

    let first = run_script(&script, &config_path);
    assert!(first.status.success(), "first run stderr: {}", String::from_utf8_lossy(&first.stderr));
    let after_first = fs::read(&config_path).unwrap();

    let second = run_script(&script, &config_path);
    assert!(second.status.success(), "second run stderr: {}", String::from_utf8_lossy(&second.stderr));
    let after_second = fs::read(&config_path).unwrap();

    assert_eq!(
        after_first, after_second,
        "running the script a second time against an already-flipped config must be a no-op"
    );
}

// AC2: "backs up before first modification." Discovered by content (a
// new file, alongside the config, whose parsed JSON equals the
// pre-modification fixture) rather than by a guessed backup filename,
// for the same reason the script itself is discovered by content.
#[test]
fn backup_of_pre_modification_content_is_created_before_first_modification() {
    let script = registration_script();
    let dir = TempDir::new().unwrap();
    let config_path = write_fixture(dir.path());

    let before: HashSet<PathBuf> = fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    assert_eq!(
        before,
        HashSet::from([config_path.clone()]),
        "sanity: only the fixture config should exist before the script runs"
    );

    let output = run_script(&script, &config_path);
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));

    let after: HashSet<PathBuf> = fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    let new_files: Vec<&PathBuf> = after.difference(&before).collect();
    assert!(
        !new_files.is_empty(),
        "expected the script to create a backup file before modifying the config; \
         directory contents after run: {:?}",
        after
    );

    let original = fixture();
    let found_backup = new_files.iter().any(|f| {
        fs::read_to_string(f)
            .ok()
            .and_then(|c| serde_json::from_str::<Value>(&c).ok())
            .map(|v| v == original)
            .unwrap_or(false)
    });
    assert!(
        found_backup,
        "expected one of the newly created files {:?} to contain the exact \
         pre-modification JSON (the backup)",
        new_files
    );
}

// AC2 (default) + AC3 + AC4: the script "accepts a config-path override
// (default ~/.claude.json)" and its live flip must never touch the real
// file. Every other test in this suite always passes an explicit scratch
// path; this test alone exercises the no-argument default path, and does
// so by pointing HOME at a scratch directory so that "defaults to
// ~/.claude.json" resolves inside the scratch tree, never the real file.
#[test]
fn omitting_the_path_argument_defaults_inside_home_never_the_real_file() {
    let script = registration_script();
    let fake_home = TempDir::new().unwrap();
    let config_path = fake_home.path().join(".claude.json");
    fs::write(
        &config_path,
        serde_json::to_string_pretty(&fixture()).unwrap(),
    )
    .unwrap();

    let output = Command::new(&script)
        .env("HOME", fake_home.path())
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {:?} with no path argument: {e}", script));
    assert!(
        output.status.success(),
        "no-argument run against a fake HOME should succeed; stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let result = read_json(&config_path);
    assert_eq!(result["mcpServers"]["iwe"], expected_iwe());
    assert_eq!(result["mcpServers"]["mind"], expected_mind());
}
