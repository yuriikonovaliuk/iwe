// CLI half of the `iwe`/`iwec` write-scope parity check for
// `efforts/multi-agent-orchestration/implementation/mind-write-separation/t4-cli-parity`.
//
// Drives the real `iwe` binary as a subprocess against a scratch store
// whose `.iwe/config.toml` carries the production-default `[transactions]`
// shape -- an empty `[transactions]` section with no `validate` key, no
// in-file `deny`/`allow`. The deny/allow list is resolved entirely via
// the subprocess env (`IWE_TRANSACTIONS_ALLOW=mind/**`); the *same*
// config shape the milestone's acceptance runs use, and the shape that
// closes the t3 defect: t4b widened `ValidatingTransaction::for_config`
// (commit 646e449) to construct a scope-None validating backend whenever
// `[transactions] deny`/`allow` is non-empty, even with `validate` left
// at its default `None`. Without that gate widening, the production-
// default `[transactions]` config would resolve to a `NoopTransaction`
// and the write-scope check would never run -- this test exercises the
// path that closed that defect.
//
// The MCP half of this parity check lives in
// `crates/iwec/tests/cli_mcp_write_scope_parity_test.rs`, driving the
// real `iwec` MCP binary over its HTTP transport with the identical
// resolved deny/allow (`IWE_TRANSACTIONS_ALLOW=mind/**` in both
// processes' env) and the same target key pairs (`mind/a` permitted,
// `other/b` denied). Both halves assert the same observable parity:
// the denied key is refused distinguishably (nonzero exit + stderr
// text on the CLI, error-message text on MCP), and the permitted key
// lands and is readable back through the canonical read path on that
// surface (`iwe retrieve` here, `iwe_retrieve` on the MCP half).
//
// What is observable across the subprocess boundary: only the
// `ValidationFailure::WriteScopeDenied`'s own `Display` rendering
// (literally: `write to '<key>' rejected: refused by the configured
// write scope`). The variant name never reaches stderr or the MCP
// message; this test matches on substrings ("rejected" / "write
// scope") plus the key, never on the variant identifier.

use std::fs::{create_dir_all, write};
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

const ALLOWED_KEY: &str = "mind/a";
const DENIED_KEY: &str = "other/b";
const ALLOWED_CONTENT: &str = "# A\n";
const DENIED_CONTENT: &str = "# B\n";
const ALLOW_ENV: &str = "mind/**";

/// Production-default `[transactions]` shape: the section is present
/// but empty, with no `validate` key and no in-file `deny`/`allow` --
/// the very shape t4b's gate widening was written to support. The
/// deny/allow list comes entirely from the subprocess env (see
/// `iwe_create`/`iwe_retrieve`).
fn store() -> TempDir {
    let dir = TempDir::new().unwrap();
    create_dir_all(dir.path().join(".iwe")).unwrap();
    write(dir.path().join(".iwe/config.toml"), "[transactions]\n").unwrap();
    dir
}

fn iwe(work_dir: &Path, args: &[&str]) -> Output {
    Command::new(crate::common::get_iwe_binary_path())
        .args(args)
        // `IWE_TRANSACTIONS_DENY` is explicitly cleared so no ambient
        // value from the test-runner's own environment leaks in --
        // `apply_transactions_env_overlay` treats either env var being
        // set as "override entirely," and the fail-fast on the resolved
        // deny-and-allow-both-non-empty case would otherwise fire on a
        // run where the runner has both set.
        .env("IWE_TRANSACTIONS_ALLOW", ALLOW_ENV)
        .env_remove("IWE_TRANSACTIONS_DENY")
        .current_dir(work_dir)
        .output()
        .expect("run iwe")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn iwe_create_to_an_allow_listed_key_succeeds_and_a_canonical_re_read_shows_it_landed() {
    let dir = store();

    let write_output = iwe(dir.path(), &["create", ALLOWED_KEY, "--content", ALLOWED_CONTENT]);
    assert!(
        write_output.status.success(),
        "write to an allow-listed key must succeed, got: {}",
        stderr(&write_output)
    );

    // Canonical re-read on this surface: the same binary that wrote the
    // document reading it back through `iwe retrieve`, not a direct file
    // read. This is the parity shape the MCP half mirrors with
    // `iwe_retrieve` -- the read path proves the document landed as a
    // document in the store, not merely as bytes on disk.
    let read_output = iwe(dir.path(), &["retrieve", "-k", ALLOWED_KEY]);
    assert!(
        read_output.status.success(),
        "re-read via `iwe retrieve` must succeed, got: {}",
        stderr(&read_output)
    );
    let stdout = stdout(&read_output);
    assert!(
        stdout.contains("# A"),
        "the re-read must surface the document's content, got: {stdout}"
    );
}

#[test]
fn iwe_create_to_a_key_outside_the_allow_list_fails_distinguishably_and_leaves_disk_untouched() {
    let dir = store();

    let output = iwe(dir.path(), &["create", DENIED_KEY, "--content", DENIED_CONTENT]);

    assert!(
        !output.status.success(),
        "a write to a denied key must fail (nonzero exit), not silently succeed"
    );
    let message = stderr(&output);
    assert!(
        message.contains(DENIED_KEY),
        "the CLI's failure signal must name the rejected key, got: {message}"
    );
    // "rejected" / "write scope" are the distinguishing substrings of
    // `ValidationFailure::WriteScopeDenied`'s `Display` rendering. The
    // variant name itself (`WriteScopeDenied`) never reaches stderr; we
    // assert on the rendered text, not the variant identifier. The
    // alternatives to match -- a parse error, a usage error -- would
    // carry neither substring.
    assert!(
        message.contains("rejected") || message.contains("write scope"),
        "the CLI's failure signal must surface the write-scope rejection \
         distinguishably from an unrelated parse/usage error, got: {message}"
    );
    assert!(
        !dir.path().join(format!("{DENIED_KEY}.md")).exists(),
        "a denied write must not land on disk"
    );
}