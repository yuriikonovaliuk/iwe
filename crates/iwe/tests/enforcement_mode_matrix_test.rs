// T13 — Enforcement-mode observation record and identical-under-both-
// invocations proof.
//
// `m2/design-enforcement-modes` names three possible enforcement modes for a
// write-permission construct:
//
//   - mode one   — rejected by default, on any invocation, no flag.
//   - mode two   — rejected only under `--strict`/the always-strict `iwec`
//                  path.
//   - mode three — never rejected at write time, detected only by a later
//                  `iwe schema validate` pass.
//
// and three reachable write paths:
//
//   - (a) IWE's ordinary write path — LSP, MCP, CLI without `--strict`.
//   - (b) IWE's strict path — CLI `--strict`, and `iwec` (MCP), which is
//         unconditionally strict for schema-validation gating
//         (`ensure_schema_clean` runs on every mutating tool call — see
//         `crates/iwec/src/lib.rs`).
//   - (c) a raw filesystem edit outside IWE entirely.
//
// This file is the record of the OBSERVED mode for each of the three M2
// write-permission constructs (freeze / EXT-FREEZE, per-property mutability
// / EXT-PER-PROPERTY-MUTABILITY, and their dominance composition /
// EXT-BOTH-JOINTLY) against every invocation that can reach path (a) or (b).
// Every cell below was produced by actually running the referenced test
// (`cargo test`), not by reading `diwe::permissions::check_write_permission`
// and inferring what it must do — a passing/failing `cargo test` run is the
// only thing that counts as "observed" here. That a construct is expressible
// or that the crate compiles is never cited as evidence of enforcement.
//
// # The matrix
//
// Because `check_write_permission` (`crates/diwe/src/permissions.rs`) is
// wired in unconditionally at every WP-02..WP-13 site — never gated behind
// `--strict` — "CLI ordinary", "CLI --strict", and "iwec" (MCP, always
// schema-strict) are, in this implementation, the *same code path* for
// write-permission purposes, not three independently-arrived-at agreements.
// The "code path" column records how that sameness is established for each
// row: either by direct source trace (every call site listed resolves to
// `check_write_permission_for_content[_in]` -> `check_write_permission`), or
// — for freeze — by an additional, stronger, empirical proof that ordinary
// and `--strict` CLI invocation produce byte-identical rejection output (see
// `freeze_rejection_is_byte_identical_between_ordinary_and_strict_cli_invocation`
// below), which is the standard `m2/design-enforcement-modes` sets ("The
// construct tests must show identical rejection under both ordinary and
// strict invocation, not merely rejection under each").
//
// | construct                        | CLI ordinary (a)   | CLI --strict (b)   | MCP / iwec (b, always-strict) | code path identity |
// |-----------------------------------|---------------------|---------------------|----------------------------------|----------------------|
// | EXT-FREEZE                        | mode one — REJECTED | mode one — REJECTED | mode one — REJECTED             | proven byte-identical, see below |
// | EXT-PER-PROPERTY-MUTABILITY (LAW-09) | mode one — REJECTED | mode one — REJECTED | mode one — REJECTED          | source trace: same call site |
// | EXT-BOTH-JOINTLY (freeze dominates mutability, R15/LAW-13) | mode one — REJECTED, rejected *as freeze* | mode one — REJECTED, rejected *as freeze* | mode one — REJECTED, rejected *as freeze* | source trace: same call site |
//
// Evidence per cell (test name -> file), all re-run for this record and
// passing on `07c6ba4` + this task's additions:
//
// EXT-FREEZE:
//   - CLI ordinary: `write_permission_test::body_write_to_a_frozen_document_is_rejected`,
//     `write_permission_test::frontmatter_only_write_to_a_frozen_document_is_also_rejected`,
//     `write_permission_test::create_of_a_frozen_document_is_rejected`
//     (`crates/iwe/tests/write_permission_test.rs`).
//   - CLI --strict: `write_permission_test::body_write_to_a_frozen_document_is_rejected_under_strict`,
//     `write_permission_test::frontmatter_write_to_a_frozen_document_is_rejected_under_strict`
//     (same file).
//   - MCP: `write_permission_test::update_of_a_frozen_document_is_rejected`,
//     `write_permission_test::create_of_a_frozen_document_is_rejected`
//     (`crates/iwec/tests/write_permission_test.rs`).
//   - Identical-rejection proof: this file, below.
//
// EXT-PER-PROPERTY-MUTABILITY:
//   - CLI ordinary: `mutability_test::update_ordinary_rejects_a_write_to_an_immutable_body`
//     (`crates/iwe/tests/mutability_test.rs`).
//   - CLI --strict: `mutability_test::update_strict_rejects_the_same_write_identically`
//     (same file — asserts the rejection's stderr contains the same
//     document/rule/selector text under both invocations).
//   - MCP: `mutability_test::update_rejects_a_write_to_an_immutable_body_and_leaves_the_file_unchanged`
//     (`crates/iwec/tests/mutability_test.rs`).
//
// EXT-BOTH-JOINTLY (dominance):
//   - CLI ordinary: `freeze_dominates_mutability_test::write_to_a_property_the_schema_marks_mutable_is_still_rejected_when_the_document_is_frozen`
//     (`crates/iwe/tests/freeze_dominates_mutability_test.rs`).
//   - CLI --strict: `freeze_dominates_mutability_test::write_to_a_property_the_schema_marks_mutable_is_still_rejected_under_strict_too`
//     (same file).
//   - MCP: `freeze_dominates_mutability_test::mcp_write_to_a_property_the_schema_marks_mutable_is_still_rejected_when_the_document_is_frozen`
//     (`crates/iwec/tests/freeze_dominates_mutability_test.rs` — added by
//     this task; no MCP-path proof of the dominance composition existed
//     before T13).
//
// # Path (c): raw filesystem edit outside IWE
//
// Per `m2/design-enforcement-modes`: "Path (c) — a raw filesystem edit
// outside IWE — is unreachable by any IWE-side mechanism whatever and is
// covered only by the compositor's checks at sync and by materialization's
// divergence detection." This is stated, not tested: there is no IWE
// process running when a plain text editor edits a file on disk, so there is
// nothing for `check_write_permission` (or any other IWE-side check) to
// intercept. No test in this repository exercises path (c), and none
// should — an automated test of "IWE does not run" would be vacuous.
//
// # Sub-mode-one finding
//
// None. All three constructs were verified, empirically, at mode one on
// every reachable ordinary and strict invocation (CLI without `--strict`,
// CLI with `--strict`, and MCP) — no cell in the matrix above observed
// anything weaker (mode two or mode three). Nothing here is flagged for
// T14.
//
// Layer-free fixtures only: no `origin:`/`mint:`/package vocabulary
// anywhere in this file or the fixtures it drives.

use diwe::config::{Configuration, LibraryOptions, MarkdownOptions};
use std::fs::{create_dir_all, write};
use std::path::Path;
use std::process::{Command, Output};
use tempfile::TempDir;

const FROZEN_DOC: &str = "---\nfreeze: true\nstatus: draft\n---\n\n# Frozen Document\n\nOriginal body.\n";

/// The strongest form of `m2/design-enforcement-modes`'s "one mechanism, not
/// two that agree" requirement: not just "both invocations reject", and not
/// just "both invocations' stderr *contains* the same substrings" (as the
/// other construct tests already show), but that ordinary and `--strict`
/// CLI invocation of the exact same rejected write produce byte-for-byte
/// identical stderr and the identical exit code. Two independently-arrived-
/// at rejections could coincidentally overlap on a contained substring;
/// they cannot coincidentally produce byte-identical output unless they are,
/// in fact, the same code emitting the same `Display` value from the same
/// call — see `crates/diwe/src/permissions.rs`'s
/// `WritePermissionError::Display` impl and `write_single_document_with`'s
/// `Err(rejected) => rejected.to_string()` in `crates/iwe/src/main.rs`,
/// which is the one site both invocations funnel through.
///
/// The fixture is deliberately schema-less (no `.iwe/schemas` binding), so
/// `--strict`'s *additional* schema-validation gate (`gate_pending`) is a
/// pure no-op here — it must not be the thing coincidentally producing
/// matching text. What's being proven is that write-permission evaluation
/// itself is identical, not that two unrelated checks happen to agree.
#[test]
fn freeze_rejection_is_byte_identical_between_ordinary_and_strict_cli_invocation() {
    let ordinary = setup();
    let strict = setup();

    let ordinary_output = run_update(
        ordinary.path(),
        &["-k", "doc", "-c", "# Frozen Document\n\nNew body.\n"],
    );
    let strict_output = run_update(
        strict.path(),
        &[
            "-k",
            "doc",
            "-c",
            "# Frozen Document\n\nNew body.\n",
            "--strict",
        ],
    );

    assert!(!ordinary_output.status.success());
    assert!(!strict_output.status.success());
    assert_eq!(
        ordinary_output.status.code(),
        strict_output.status.code(),
        "ordinary and --strict must exit with the same code for the same rejected write"
    );

    let ordinary_stderr = String::from_utf8(ordinary_output.stderr).expect("valid UTF-8 stderr");
    let strict_stderr = String::from_utf8(strict_output.stderr).expect("valid UTF-8 stderr");
    assert_eq!(
        ordinary_stderr, strict_stderr,
        "ordinary and --strict rejection text must be byte-identical — any \
         divergence would mean --strict is a variant implementation of \
         write-permission evaluation, not a superset over the same one \
         (m2/design-enforcement-modes: \"must never re-implement or vary \
         it\")"
    );
    // Pin down what that identical text actually is, so a future change that
    // (for instance) starts appending "for '<key>'" to one path but not the
    // other is caught even if the two paths happened to still agree on
    // content by coincidence.
    assert_eq!(
        ordinary_stderr,
        "Error: write to 'doc' rejected: document is frozen (unset 'freeze' to allow writes)\n"
    );
}

fn setup() -> TempDir {
    let temp = TempDir::new().expect("tempdir");
    create_dir_all(temp.path().join(".iwe")).expect("mkdir .iwe");
    let config = Configuration {
        library: LibraryOptions {
            path: "".to_string(),
            ..Default::default()
        },
        markdown: MarkdownOptions {
            refs_extension: "".to_string(),
            ..Default::default()
        },
        ..Default::default()
    };
    write(
        temp.path().join(".iwe").join("config.toml"),
        toml::to_string(&config).expect("config"),
    )
    .expect("write config");
    write(temp.path().join("doc.md"), FROZEN_DOC).expect("write doc");
    temp
}

fn run_update(work_dir: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(crate::common::get_iwe_binary_path());
    command.arg("update").current_dir(work_dir);
    for arg in args {
        command.arg(arg);
    }
    command.output().expect("run iwe update")
}
