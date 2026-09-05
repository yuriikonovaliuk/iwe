//! Acceptance-criteria tests for
//! `efforts/multi-agent-orchestration/implementation/mind-write-separation/t2-env-overlay`.
//!
//! Written from the task's persisted contract only (Shared surface:
//! `IWE_TRANSACTIONS_DENY` / `IWE_TRANSACTIONS_ALLOW`, comma-separated
//! pattern lists, override-entirely semantics, applied inside
//! `load_config()` in `crates/diwe/src/config.rs`) -- without reading or
//! waiting on Developer's implementation of the same task, per the
//! Test-builder role's independence requirement.
//!
//! The exact fail-fast error variant is Developer's to confirm and had not
//! landed in the contract at the time these tests were written, so the
//! fail-fast case below asserts only `Result::is_err()` against
//! `load_config()`'s existing `Result<Configuration, String>` return type
//! (no new parallel error type per the contract), rather than a guessed
//! variant name. Tighten to the specific variant/message once it is
//! confirmed.
//!
//! Kept as a separate integration-test file (rather than added inline to
//! `crates/diwe/src/config.rs` or appended to `write_permitted_test.rs`)
//! to avoid clobbering the same file a Developer agent is concurrently
//! editing for this task.
//!
//! `load_config()` takes no parameters: it re-derives its config path from
//! `std::env::current_dir()` and reads `IWE_TRANSACTIONS_DENY` /
//! `IWE_TRANSACTIONS_ALLOW` from the process environment directly, so each
//! test below has to swap both the process cwd and those two env vars for
//! the duration of its own call. Rust test binaries run tests on separate
//! threads by default, and both cwd and env vars are process-global, so
//! every test in this file is serialized on `env_lock()` -- this crate has
//! no existing serial-env-tests convention (checked: no `serial_test`
//! dependency, no shared mutex helper) so a local `std::sync::Mutex` is
//! used, matching the "no other thread touches the env while the lock is
//! held" discipline already used for `IWE_TEST_LOCK_FENCING_DELAY_MS` in
//! `crates/iwec/tests/agent_transaction_test.rs`.

use std::{
    env, fs,
    sync::{Mutex, OnceLock},
};

use diwe::config::{load_config, Configuration};

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Runs one `load_config()` call against a fresh temp project directory
/// whose `.iwe/config.toml` is `config_toml`, with
/// `IWE_TRANSACTIONS_DENY` / `IWE_TRANSACTIONS_ALLOW` set to `deny_env` /
/// `allow_env` (`None` = unset for the duration of the call). Restores the
/// original cwd and clears both env vars again before returning, all
/// while holding `env_lock()`.
fn load_with(
    config_toml: &str,
    deny_env: Option<&str>,
    allow_env: Option<&str>,
) -> Result<Configuration, String> {
    let _guard = env_lock().lock().unwrap_or_else(|poisoned| poisoned.into_inner());

    let dir = tempfile::tempdir().expect("create temp project dir");
    let iwe_dir = dir.path().join(".iwe");
    fs::create_dir_all(&iwe_dir).expect("create .iwe dir");
    fs::write(iwe_dir.join("config.toml"), config_toml).expect("write .iwe/config.toml");

    let original_dir = env::current_dir().expect("read current dir");
    env::set_current_dir(dir.path()).expect("chdir into temp project dir");

    // SAFETY: serialized by `env_lock` above -- no other thread reads or
    // writes these two vars, or the process cwd, while the guard is held.
    unsafe {
        match deny_env {
            Some(v) => env::set_var("IWE_TRANSACTIONS_DENY", v),
            None => env::remove_var("IWE_TRANSACTIONS_DENY"),
        }
        match allow_env {
            Some(v) => env::set_var("IWE_TRANSACTIONS_ALLOW", v),
            None => env::remove_var("IWE_TRANSACTIONS_ALLOW"),
        }
    }

    let result = load_config();

    // SAFETY: see above.
    unsafe {
        env::remove_var("IWE_TRANSACTIONS_DENY");
        env::remove_var("IWE_TRANSACTIONS_ALLOW");
    }
    env::set_current_dir(&original_dir).expect("restore original cwd");

    result
}

const FILE_DENY_A_ONLY: &str = "version = 3\n\n[transactions]\ndeny = [\"a/**\"]\n";

// ---------------------------------------------------------------------
// Test (b): env IWE_TRANSACTIONS_ALLOW set alone overrides the file's
// deny/allow pair entirely -- the file's non-empty `deny` is discarded,
// not merged with the env-provided `allow`.
// ---------------------------------------------------------------------

#[test]
fn env_allow_only_discards_file_deny_entirely() {
    let config = load_with(FILE_DENY_A_ONLY, None, Some("mind/**"))
        .expect("only allow set, both empty->non-empty resolved list -- must not fail fast");

    assert_eq!(config.transactions.allow, vec!["mind/**".to_string()]);
    assert_eq!(
        config.transactions.deny,
        Vec::<String>::new(),
        "file's deny = [\"a/**\"] must be discarded, not merged, once ALLOW is set from env"
    );
}

// ---------------------------------------------------------------------
// Test (c): both env vars set non-empty in the same process env ->
// load_config() fails fast, regardless of file contents.
//
// Fixture note: the file's own deny/allow start EMPTY here (no
// `[transactions]` section at all), so the only way either list can end
// up non-empty is via the env overlay. A prior version of this fixture
// started the file with both deny and allow already non-empty before
// applying env overrides, which could not distinguish "fail-fast fires
// because the *env-driven final resolved state* has both lists
// non-empty" (the actual contract rule) from a weaker/wrong
// implementation that fails fast merely because the file itself already
// had both lists populated, independent of any env involvement. Starting
// from an empty-file fixture and populating both lists purely from env
// makes this test actually pin the env-triggered fail-fast path.
// ---------------------------------------------------------------------

#[test]
fn both_env_vars_set_non_empty_fails_fast_regardless_of_file_contents() {
    let file = "version = 3\n";

    let result = load_with(file, Some("x/**"), Some("y/**"));

    assert!(
        result.is_err(),
        "both env vars non-empty (file's own deny/allow start empty) must fail fast \
         (exact variant TBD, see module docs): {result:?}"
    );
}

// ---------------------------------------------------------------------
// Negative case for the fail-fast rule: only ONE env var set non-empty,
// while the file's OTHER list is also non-empty. A naive merge-then-check
// implementation would see two non-empty lists here and wrongly fail
// fast; override-entirely must not, because the file's list on the
// unset-env side is discarded (resolves empty), not merged in.
// ---------------------------------------------------------------------

#[test]
fn env_deny_only_with_non_empty_file_allow_does_not_fail_fast() {
    // File has allow=["mind/**"] (non-empty) and no deny. Only
    // IWE_TRANSACTIONS_DENY is set from env. A merge-then-check bug would
    // combine env's deny with the file's allow and see both non-empty;
    // override-entirely must resolve allow=[] (env's allow is absent) and
    // so must not fail fast.
    let file = "version = 3\n\n[transactions]\nallow = [\"mind/**\"]\n";

    let config = load_with(file, Some("secret/**"), None)
        .expect("only deny set from env -- must not fail fast even though file's allow is non-empty");

    assert_eq!(config.transactions.deny, vec!["secret/**".to_string()]);
    assert_eq!(
        config.transactions.allow,
        Vec::<String>::new(),
        "file's allow = [\"mind/**\"] must be discarded (not merged) once DENY is set from env"
    );
}

// ---------------------------------------------------------------------
// Baseline/regression: neither env var set -> the file's deny/allow apply
// unchanged.
// ---------------------------------------------------------------------

#[test]
fn neither_env_var_set_leaves_file_deny_allow_unchanged() {
    // Only one of deny/allow is non-empty in the file: the fail-fast rule
    // fires on the *final resolved* deny/allow being both non-empty
    // regardless of source (see `both_env_vars_set_non_empty_...` above,
    // which is the same rule tripped by an env override instead), so a
    // baseline/regression fixture for "no env override" has to keep the
    // file itself on the single-non-empty-list side of that rule too.
    let config = load_with(FILE_DENY_A_ONLY, None, None).expect("no env override -- must not fail fast");

    assert_eq!(config.transactions.deny, vec!["a/**".to_string()]);
    assert_eq!(config.transactions.allow, Vec::<String>::new());
}

// ---------------------------------------------------------------------
// Comma-separated parsing: split on ',', trim whitespace per entry
// (contract's default assumption absent a stricter spec).
// ---------------------------------------------------------------------

#[test]
fn env_var_comma_separated_list_is_split_and_trimmed() {
    let config = load_with(FILE_DENY_A_ONLY, None, Some(" mind/** , other/** "))
        .expect("only allow set -- must not fail fast");

    assert_eq!(
        config.transactions.allow,
        vec!["mind/**".to_string(), "other/**".to_string()]
    );
    assert_eq!(config.transactions.deny, Vec::<String>::new());
}
