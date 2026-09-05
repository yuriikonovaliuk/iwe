//! Placement test for
//! `efforts/knowledge-compositor/m6-b-cutover-preconditions/t5-fencing-check-before-write/contract`,
//! covering `crates/diwe/src/fs.rs`'s `apply_changes_with` -- the third
//! of the three sites the contract names. See
//! `crates/iwe/tests/fencing_placement_test.rs` for the full rationale
//! (why this is a source-inspection test rather than a race, and what it
//! deliberately does not claim to prove) and the sibling test suite,
//! `crates/iwe/tests/fencing_check_before_write_test.rs`, for the
//! behavioral (real-reclaim) side of this same contract, covering the
//! other two sites.
//!
//! `check_fencing()` does not exist anywhere in this crate today
//! (`diwe` has no dependency-driven reason to import
//! `liwe::write_lock` before this task's fix lands: `apply_changes_with`
//! currently has no notion of the store-wide commit lock at all -- the
//! `iwe` CLI's `apply_changes` wrapper in `main.rs` acquires the lock and
//! checks fencing once, entirely outside this function, before ever
//! calling into it). This test is expected, and intended, to fail
//! against the pre-fix code for exactly that reason: there is nothing
//! here yet for it to find.

use std::path::Path;

fn read_source() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/fs.rs");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()))
}

/// See `crates/iwe/tests/fencing_placement_test.rs::extract_fn_body` --
/// identical brace-counting technique, duplicated here so this crate's
/// test doesn't reach across crates for a private helper.
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

/// Byte offsets of every non-overlapping occurrence of `needle` in
/// `haystack`, in order.
fn all_positions(haystack: &str, needle: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = haystack[from..].find(needle) {
        out.push(from + i);
        from += i + needle.len();
    }
    out
}

/// Forbidden between `check_fencing()` and the write it guards -- see
/// `crates/iwe/tests/fencing_placement_test.rs` for the rationale shared
/// with this crate's copy of the check.
const FORBIDDEN_BETWEEN_CHECK_AND_WRITE: &[&str] = &["tx.commit(", "tx.write(", "tx.begin(", "check(key"];

/// Asserts that the write at `body`'s `write_positions[occurrence]` is
/// immediately preceded by a `check_fencing()` call that itself comes
/// after that same write's own `tx.commit()` -- i.e. the check has moved
/// into this per-key loop, immediately before its filesystem operation,
/// rather than being absent (pre-fix) or running once for the whole
/// batch (which would leave later keys' writes unguarded by anything
/// closer than the first key's check).
fn assert_check_fencing_guards_this_write(body: &str, write_marker: &str, occurrence: usize, context: &str) {
    let positions = all_positions(body, write_marker);
    let write_at = *positions.get(occurrence).unwrap_or_else(|| {
        panic!(
            "{context}: expected at least {} occurrence(s) of `{write_marker}`, found {}",
            occurrence + 1,
            positions.len()
        )
    });

    let check_at = body[..write_at].rfind("check_fencing()").unwrap_or_else(|| {
        panic!(
            "{context}: no `check_fencing()` call precedes this write of `{write_marker}` -- it \
             must run immediately before every write this function performs (pre-fix, this \
             function has no `check_fencing()` call at all)"
        )
    });

    let commit_at = body[..check_at].rfind("tx.commit()");
    assert!(
        commit_at.is_some(),
        "{context}: the nearest `check_fencing()` preceding this write has no `tx.commit()` \
         before it -- either it belongs to an earlier loop iteration (too far away to be \
         \"immediately before\" this write) or the ordering is wrong"
    );

    let between = &body[check_at + "check_fencing()".len()..write_at];
    for forbidden in FORBIDDEN_BETWEEN_CHECK_AND_WRITE {
        assert!(
            !between.contains(forbidden),
            "{context}: found `{forbidden}` between `check_fencing()` and the write \
             `{write_marker}` (occurrence {occurrence}) -- nothing should be schedulable between \
             the check and the write it guards. Found in: {between:?}"
        );
    }
}

/// Contract AC3: `crates/diwe/src/fs.rs`'s `apply_changes_with` --
/// `check_fencing()` immediately before each of the three filesystem
/// operations it performs (remove, create, update), each preceded by its
/// own `tx.commit()` for that same key.
#[test]
fn apply_changes_with_check_fencing_runs_immediately_before_each_write() {
    let source = read_source();
    let body = extract_fn_body(&source, "apply_changes_with");

    assert_check_fencing_guards_this_write(
        &body,
        "fs::remove_file(&file_path)",
        0,
        "crates/diwe/src/fs.rs::apply_changes_with (remove)",
    );
    assert_check_fencing_guards_this_write(
        &body,
        "fs::write(&file_path, markdown)",
        0,
        "crates/diwe/src/fs.rs::apply_changes_with (create)",
    );
    assert_check_fencing_guards_this_write(
        &body,
        "fs::write(&file_path, markdown)",
        1,
        "crates/diwe/src/fs.rs::apply_changes_with (update)",
    );
}
