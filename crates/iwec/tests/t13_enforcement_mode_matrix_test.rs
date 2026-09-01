// T13 — Enforcement-mode matrix (independent verification), MCP half.
//
// Independent verification per `m2/design-enforcement-modes` (C13): this
// file drives a real, in-process MCP server (via `Fixture`, a real
// `rmcp` client/server pair over a duplex transport — real JSON-RPC tool
// calls, not a call into `diwe::permissions` directly) for the same three
// constructs the CLI half
// (`crates/iwe/tests/t13_enforcement_mode_matrix_test.rs`) exercises:
// freeze (EXT-FREEZE, T10), per-property mutability
// (EXT-PER-PROPERTY-MUTABILITY, T11), and their dominance composition
// (R15/LAW-13). See that file's header for the full mode-one/two/three
// definitions and the matrix table; this file supplies the MCP column.
//
// MCP has no `--strict`/ordinary distinction of its own — write-permission
// evaluation is wired unconditionally into `IweServer::write_file`
// (`enforce_write_permission`, called from inside `write_file_with`'s
// transaction bracket) the same way it is in the CLI's ordinary path;
// there is no flag that varies it. So the MCP column of the matrix is one
// cell wide per construct, not two.
//
// | construct                    | MCP    |
// |-------------------------------|--------|
// | freeze                        | mode one |
// | per-property mutability       | mode one |
// | dominance (freeze > mutable)  | mode one |
//
// Sub-mode-one finding: none. All three MCP cells reject on an ordinary
// (unconfigured, no separate flag) tool call — verified empirically below,
// not inferred from `write_file`'s source.
//
// One incidental, independently-observed finding, reported because
// independence is the point of this task: `crates/iwec/tests/
// mutability_test.rs`'s own comment (on
// `update_rejects_a_write_to_an_immutable_body_and_leaves_the_file_
// unchanged`) states that "WP-12's `write_file` silently drops the write
// on rejection (no MCP error surfaced today...)". Empirically, that is no
// longer accurate for `iwe_update` at HEAD (07c6ba4): both freeze and
// per-property-mutability rejections there return `Err` from
// `try_call_tool`, with the same rejection message
// `check_write_permission`'s `Display` impl produces (see
// `matrix_mutability_mcp_rejects` below). This is not a gap this task
// needs to close (the write is refused either way, satisfying mode one);
// it is flagged only because it contradicts a comment already in the
// tree, and this task's whole premise is not inferring behavior from
// comments or code but observing it.
//
// Path (c) — a raw filesystem edit made outside IWE (no `iwe` CLI
// process, no MCP tool call) — is unreachable by anything in this file or
// its CLI counterpart, for the same reason given there: there is no
// IWE-side call site to reach through it. Not tested here, deliberately.
//
// Layer-free fixtures only.

use crate::fixture::Fixture;
use diwe::config::{Configuration, Patterns, SchemaBinding};
use std::collections::HashMap;
use std::fs::{create_dir_all, read_to_string, write};
use tempfile::TempDir;

fn schema_config(name: &str, pattern: &str) -> Configuration {
    let mut schemas = HashMap::new();
    schemas.insert(
        name.to_string(),
        SchemaBinding {
            r#match: Patterns::One(pattern.to_string()),
        },
    );
    Configuration {
        schemas,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------
// Cell: freeze x MCP
// ---------------------------------------------------------------------

const FROZEN_DOC: &str = "---\nfreeze: true\nstatus: draft\n---\n\n# Frozen\n\nOriginal body.\n";

#[tokio::test]
async fn matrix_freeze_mcp_rejects() {
    let dir = TempDir::new().unwrap();
    let base = dir.path().canonicalize().unwrap();
    write(base.join("doc.md"), FROZEN_DOC).unwrap();
    let f = Fixture::with_path(base.to_str().unwrap(), Configuration::default()).await;

    let result = f
        .try_call_tool(
            "iwe_update",
            serde_json::json!({
                "key": "doc",
                "content": "---\nfreeze: true\nstatus: draft\n---\n\n# Frozen\n\nNew body.\n"
            }),
        )
        .await;

    assert!(
        result.is_err(),
        "freeze under MCP invocation must reject the write (mode one)"
    );
    let message = result.unwrap_err().to_string();
    assert!(
        message.contains("doc") && message.contains("frozen"),
        "got: {message}"
    );
    assert_eq!(
        read_to_string(base.join("doc.md")).unwrap(),
        FROZEN_DOC,
        "frozen document must be unchanged on disk"
    );
}

// ---------------------------------------------------------------------
// Cell: per-property mutability x MCP
// ---------------------------------------------------------------------

const MUTABILITY_DOC: &str = "# Reference\n\noriginal body\n";

#[tokio::test]
async fn matrix_mutability_mcp_rejects() {
    let dir = TempDir::new().unwrap();
    let base = dir.path().canonicalize().unwrap();
    create_dir_all(base.join(".iwe/schemas")).unwrap();
    write(
        base.join(".iwe/schemas/reference.yaml"),
        "mutable:\n  $content: false\n",
    )
    .unwrap();
    create_dir_all(base.join("notes")).unwrap();
    write(base.join("notes/one.md"), MUTABILITY_DOC).unwrap();
    let f = Fixture::with_path(
        base.to_str().unwrap(),
        schema_config("reference", "notes/**"),
    )
    .await;

    let result = f
        .try_call_tool(
            "iwe_update",
            serde_json::json!({ "key": "notes/one", "content": "# Reference\n\nchanged body\n" }),
        )
        .await;

    assert!(
        result.is_err(),
        "per-property mutability under MCP invocation must reject the write \
         and surface it as a tool error (mode one) — not merely leave the file \
         unchanged while reporting success"
    );
    let message = result.unwrap_err().to_string();
    assert!(message.contains("notes/one"), "{message}");
    assert!(message.contains("mutable: false"), "{message}");
    assert!(message.contains("$content"), "{message}");
    assert_eq!(
        read_to_string(base.join("notes/one.md")).unwrap(),
        MUTABILITY_DOC
    );
}

// ---------------------------------------------------------------------
// Cell: dominance (freeze > mutable) x MCP
// ---------------------------------------------------------------------

const FROZEN_BUT_NOMINALLY_MUTABLE_DOC: &str =
    "---\nfreeze: true\n---\n\n# Reference\n\noriginal body\n";

#[tokio::test]
async fn matrix_dominance_mcp_rejects() {
    let dir = TempDir::new().unwrap();
    let base = dir.path().canonicalize().unwrap();
    create_dir_all(base.join(".iwe/schemas")).unwrap();
    write(
        base.join(".iwe/schemas/reference.yaml"),
        "mutable:\n  $content: true\n",
    )
    .unwrap();
    create_dir_all(base.join("notes")).unwrap();
    write(base.join("notes/one.md"), FROZEN_BUT_NOMINALLY_MUTABLE_DOC).unwrap();
    let f = Fixture::with_path(
        base.to_str().unwrap(),
        schema_config("reference", "notes/**"),
    )
    .await;

    let result = f
        .try_call_tool(
            "iwe_update",
            serde_json::json!({
                "key": "notes/one",
                "content": "---\nfreeze: true\n---\n\n# Reference\n\nchanged body\n"
            }),
        )
        .await;

    assert!(
        result.is_err(),
        "dominance under MCP invocation must still reject the write even though \
         the schema marks the written property mutable"
    );
    let message = result.unwrap_err().to_string();
    assert!(
        message.contains("notes/one") && message.contains("frozen"),
        "must be attributed to freeze: got {message}"
    );
    assert!(
        !message.contains("mutable: false"),
        "must not be misreported as a mutability rejection: got {message}"
    );
    assert_eq!(
        read_to_string(base.join("notes/one.md")).unwrap(),
        FROZEN_BUT_NOMINALLY_MUTABLE_DOC
    );
}
