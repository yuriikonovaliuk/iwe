//! Placement test for
//! `efforts/knowledge-compositor/m6-b-cutover-preconditions/t5-fencing-check-before-write/contract`,
//! covering the two `crates/iwe/src` sites the contract names
//! (`new.rs`'s `write_document`/`write_document_with`, and `main.rs`'s
//! `write_single_document`/`write_single_document_with`). The sibling
//! site in `crates/diwe/src/fs.rs` has its own placement test in that
//! crate's `tests/fencing_placement_test.rs`.
//!
//! # Why a source-inspection test, not a race
//!
//! `crates/iwe/tests/fencing_check_before_write_test.rs` (this same
//! directory) and `crates/iwe/tests/cli_lock_wiring_test.rs` already
//! cover the *behavioral* side of fencing with real concurrent reclaims
//! -- but, as both files' own doc comments note, a reclaimer that reacts
//! the instant it observes the CLI's lock generation on disk can land
//! anywhere across the acquire-to-completion span of the command. It
//! cannot be steered deterministically into the one interval this
//! contract exists to close: between `check_fencing()` returning `Ok`
//! and the specific filesystem write that follows it (contract
//! acceptance criterion 7, "the check→write window").
//!
//! Investigating the actual code (both pre- and, by design, post-fix)
//! shows why no such steering is possible without a new synchronization
//! hook this contract's Shared surface explicitly forbids ("no new
//! API"): the one delay hook that already exists for this purpose,
//! `IWE_TEST_LOCK_FENCING_DELAY_MS` (`iwe::new::widen_fencing_window_for_test`),
//! fires at the very start of `acquire_cli_commit_lock()` -- structurally
//! *before* `check_fencing()` wherever it is called from, so relocating
//! `check_fencing()` only widens the window that hook already covers
//! (acquire→check); it cannot be repurposed to widen a window that starts
//! only once `check_fencing()` has already returned. And the fix this
//! contract asks for -- "immediately before the write" -- is by
//! construction meant to leave nothing schedulable between the check and
//! the write: no commit, no permission check, no disk read, just the
//! `check_fencing()` call followed directly by the write call. That is
//! exactly AC7's own escape clause: "the check and write become
//! atomically adjacent with literally nothing schedulable between them."
//!
//! What *is* independently, deterministically testable is the placement
//! itself: whether `check_fencing()` actually sits immediately before the
//! write (this file), together with the *reused* correctness proof that
//! `check_fencing()` reports staleness accurately wherever it is called
//! from -- already covered by `liwe::write_lock`'s own
//! `check_fencing_reports_stale_after_reclaim` /
//! `check_fencing_ok_while_hold_is_current` unit tests, which this task's
//! Shared surface pins as "reuse existing function, no new API": the
//! move changes *where* the already-correct check runs, not *whether* it
//! is correct. This file is the deterministic complement that specific
//! placement claim needs.
//!
//! This test asserts against the source text of `crates/iwe/src/new.rs`
//! and `crates/iwe/src/main.rs` directly. It is expected, and intended,
//! to fail against the pre-fix code: today `check_fencing()` is called in
//! the *outer* `write_document`/`write_single_document` wrapper,
//! immediately after acquiring the lock and well before the inner
//! `_with` function that does the actual filesystem write runs at all.

use std::path::Path;

fn read_source(relative_to_crate: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative_to_crate);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()))
}

/// Extracts the body (contents between the outermost matching `{`/`}`) of
/// the first `fn <name>` found in `source`, by brace-counting -- not by
/// parsing Rust, just enough structure to isolate one function's text
/// from its neighbors. Panics with a clear message if `name` isn't found,
/// since that itself would mean this task's contracted call sites moved
/// somewhere this test doesn't know to look, which is worth failing loud
/// on rather than silently skipping.
fn extract_fn_body(source: &str, name: &str) -> String {
    let marker = format!("fn {name}");
    let start = source
        .find(&marker)
        .unwrap_or_else(|| panic!("could not find `{marker}` -- has this function been renamed or removed?"));
    let open = source[start..]
        .find('{')
        .map(|i| start + i)
        .expect("function signature must be followed by a `{`");
    let bytes = source.as_bytes();
    let mut depth = 0i32;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return source[open + 1..i].to_string();
                }
            }
            _ => {}
        }
        i += 1;
    }
    panic!("unbalanced braces while extracting `{name}`'s body");
}

/// All the forbidden bits of work that must NOT appear between
/// `check_fencing()` and the write it guards: another transaction-object
/// method call (`tx.`), the write-permission check, or a fresh read of
/// the file's prior content. Any of these landing between the check and
/// the write would reopen exactly the gap this task exists to close.
const FORBIDDEN_BETWEEN_CHECK_AND_WRITE: &[&str] =
    &["tx.", "check_write_permission_for_content(", "read_to_string("];

/// Asserts that, within `body`, the nearest `check_fencing()` call
/// preceding `body[..write_at]`'s write immediately precedes it (nothing
/// forbidden in between), and that a `tx.commit()` occurs before that
/// `check_fencing()` -- i.e. the check has moved past the commit step,
/// not sitting where the old bug left it (right after lock acquisition,
/// long before any commit).
fn assert_check_fencing_immediately_precedes(body: &str, write_marker: &str, context: &str) {
    let write_at = body
        .find(write_marker)
        .unwrap_or_else(|| panic!("{context}: could not find the write call `{write_marker}`"));

    let check_at = body[..write_at].rfind("check_fencing()").unwrap_or_else(|| {
        panic!(
            "{context}: no `check_fencing()` call precedes the write `{write_marker}` at all -- \
             it must run immediately before this write, not merely somewhere earlier in the \
             function (or, pre-fix, not in this function at all)"
        )
    });

    let commit_at = body[..check_at].rfind("tx.commit()");
    assert!(
        commit_at.is_some(),
        "{context}: `check_fencing()` must run after this write's own `tx.commit()`, not before \
         it -- finding no preceding `tx.commit()` means the check is still sitting where the old \
         bug left it, immediately after lock acquisition rather than immediately before the write"
    );

    let between = &body[check_at + "check_fencing()".len()..write_at];
    for forbidden in FORBIDDEN_BETWEEN_CHECK_AND_WRITE {
        assert!(
            !between.contains(forbidden),
            "{context}: found `{forbidden}` between `check_fencing()` and the write \
             `{write_marker}` -- the check must be immediately before the write, with nothing \
             schedulable (no further transaction step, no permission re-check, no disk read) in \
             between. Found in: {between:?}"
        );
    }
}

/// Contract AC1: `crates/iwe/src/new.rs` -- `check_fencing()` immediately
/// before `write_document_with`'s actual filesystem write
/// (`std::fs::write(&prepared.path, &prepared.content)`), not in the
/// outer `write_document` wrapper immediately after
/// `acquire_cli_commit_lock()`.
#[test]
fn new_rs_check_fencing_runs_immediately_before_the_write() {
    let source = read_source("src/new.rs");
    let body = extract_fn_body(&source, "write_document_with");
    assert_check_fencing_immediately_precedes(
        &body,
        "std::fs::write(&prepared.path, &prepared.content)",
        "crates/iwe/src/new.rs::write_document_with",
    );
}

/// Companion to the above: the *outer* `write_document` wrapper must no
/// longer call `check_fencing()` immediately after acquiring the lock --
/// the exact shape the pre-fix defect has. This does not forbid
/// `write_document` from mentioning `check_fencing` at all (e.g. in a
/// doc comment), only from calling it directly on the freshly acquired
/// guard before handing off to `write_document_with`.
#[test]
fn new_rs_write_document_no_longer_checks_fencing_right_after_acquire() {
    let source = read_source("src/new.rs");
    let body = extract_fn_body(&source, "write_document");
    if let Some(acquire_at) = body.find("acquire_cli_commit_lock()") {
        if let Some(check_at) = body[acquire_at..].find("check_fencing()") {
            let between = &body[acquire_at + "acquire_cli_commit_lock()".len()
                ..acquire_at + check_at];
            // The old bug: nothing but the `?` / error-mapping boilerplate
            // between acquiring the lock and checking fencing, i.e. no
            // write of any kind in between either.
            assert!(
                between.contains("write_document_with(") || between.contains("std::fs::write("),
                "crates/iwe/src/new.rs::write_document: `check_fencing()` still runs right after \
                 `acquire_cli_commit_lock()`, before the actual write -- this is the pre-fix \
                 placement the contract exists to move"
            );
        }
    }
}

/// Contract AC2 (second location, ~4053-4128): `crates/iwe/src/main.rs`
/// -- `check_fencing()` immediately before `write_single_document_with`'s
/// actual filesystem write (`std::fs::write(path, content)`), not in the
/// outer `write_single_document` wrapper immediately after
/// `acquire_cli_commit_lock()`.
#[test]
fn main_rs_write_single_document_check_fencing_runs_immediately_before_the_write() {
    let source = read_source("src/main.rs");
    let body = extract_fn_body(&source, "write_single_document_with");
    assert_check_fencing_immediately_precedes(
        &body,
        "std::fs::write(path, content)",
        "crates/iwe/src/main.rs::write_single_document_with",
    );
}
