// CLI half of the `iwe`/`iwec` write-scope parity check for
// `efforts/multi-agent-orchestration/implementation/mind-write-separation/t4-cli-parity`.
//
// The task's first sub-step (Task-scoping did not pin this): the CLI
// entrypoint driven here is `iwe create <key> --content <content>`
// (`crates/iwe/src/main.rs::create_command`, landing through
// `iwe::new::write_document` -> `validating_backend` ->
// `ValidatingTransaction::for_config`). This is the CLI's most direct
// single-key scoped write, and its `get_configuration()` call (`load_config()`
// from `crates/diwe/src/config.rs`) is the same one every other CLI write
// command (`update`, `attach`, `delete`, `rename`, `extract`, `inline`)
// funnels through via `validating_backend`, so this entrypoint stands in
// for all of them for the purpose of this parity check.
//
// The sibling half of this test lives in
// `crates/iwec/tests/cli_mcp_write_scope_parity_test.rs`, driving the real
// `iwec` MCP binary over HTTP with the identical resolved deny/allow
// (`IWE_TRANSACTIONS_ALLOW=mind/**` in both processes' env) and the same
// target keys (`mind/a` permitted, `other/b` denied).
//
// CLI-parity verdict (see this task's handback): the `iwe` CLI's write
// path already constructs its `ValidatingTransaction` via the same
// `load_config()` diwe exposes and the same
// `ValidatingTransaction::for_config()` iwec's MCP backend calls -- no
// production code change was needed. This test exercises that
// already-identical path, not a fixed one.

use std::fs::{create_dir_all, read_to_string, write};
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

/// `[transactions] validate = "affected-set"` is enough to turn on
/// `ValidatingTransaction` (and therefore its write-scope check) without
/// requiring any schema setup -- `validate_final_state` under
/// `AffectedSet`/`None` is a no-op with no schemas bound (see
/// `ValidatingTransaction::for_config`'s own doc comment: `None` is the
/// only scope this task's check is skipped for entirely).
fn store() -> TempDir {
    let dir = TempDir::new().unwrap();
    create_dir_all(dir.path().join(".iwe")).unwrap();
    write(
        dir.path().join(".iwe/config.toml"),
        "[transactions]\nvalidate = \"affected-set\"\n",
    )
    .unwrap();
    dir
}

fn iwe_create(work_dir: &Path, key: &str, content: &str) -> Output {
    Command::new(crate::common::get_iwe_binary_path())
        .args(["create", key, "--content", content])
        .env("IWE_TRANSACTIONS_ALLOW", "mind/**")
        .current_dir(work_dir)
        .output()
        .expect("run iwe create")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn iwe_create_to_an_allow_listed_key_succeeds() {
    let dir = store();

    let output = iwe_create(dir.path(), "mind/a", "# A\n");

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        read_to_string(dir.path().join("mind/a.md")).unwrap(),
        "# A\n"
    );
}

#[test]
fn iwe_create_to_a_key_outside_the_allow_list_fails_distinguishably_and_leaves_disk_untouched() {
    let dir = store();

    let output = iwe_create(dir.path(), "other/b", "# B\n");

    assert!(
        !output.status.success(),
        "a write to a denied key must fail (nonzero exit), not silently succeed"
    );
    let message = stderr(&output);
    assert!(
        message.contains("other/b"),
        "the CLI's failure signal must name the rejected key, got: {message}"
    );
    assert!(
        message.contains("rejected"),
        "the CLI's failure signal must surface the write-scope rejection distinguishably \
         (not confused with an unrelated error), got: {message}"
    );
    assert!(!dir.path().join("other/b.md").exists());
}
