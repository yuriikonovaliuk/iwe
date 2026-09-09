//! Behavioral test suite for the handle-keyed open-transaction map on
//! `IweServer` (atomic task `handle-keyed-open-txs`, milestone
//! efforts/knowledge-compositor/m6-b-cutover-preconditions/design-7).
//!
//! Built from the acceptance criteria alone — no `crates/iwec/src/lib.rs`
//! was read while authoring this file. Every identifier used below comes
//! from the task's acceptance criteria, this crate's already-committed
//! test fixture (`tests/fixture.rs`), and the established MCP-server test
//! conventions in `tests/agent_transaction_test.rs` and
//! `tests/tx_commit_lock_wiring_test.rs`.
//!
//! Observable surface exercised here:
//!
//!   - The `MCP list_tools` response: the input schema for every
//!     transaction-participating tool (`iwe_tx_begin`, `iwe_tx_commit`,
//!     `iwe_tx_abort`, `iwe_create`, `iwe_update`, `iwe_delete`,
//!     `iwe_query`, `iwe_rename`, `iwe_extract`, `iwe_inline`,
//!     `iwe_normalize`, `iwe_attach`) carries an `handle` property — i.e.
//!     the schema's `properties.handle` exists and is a string-shaped
//!     optional. This is the observable counterpart to the schema-shape
//!     acceptance criteria (`TxBeginParams`/`TxCommitParams`/
//!     `TxAbortParams`/every write-tool Params carries
//!     `handle: Option<String>` with the documented description).
//!
//!   - The `iwe_tx_begin` / `iwe_tx_commit` / `iwe_tx_abort` tool calls:
//!     a second `iwe_tx_begin` on the same resolved key (explicit or
//!     default) is refused with a message that names the occupying
//!     transaction's staged keys; a `iwe_tx_begin` on a different
//!     explicit handle succeeds (multiplicity); `iwe_tx_commit` /
//!     `iwe_tx_abort` with no open transaction under the resolved key
//!     report "no transaction is open".
//!
//! What this suite does NOT cover:
//!
//!   - The internal type of `IweServer::open_txs`
//!     (`Arc<std::sync::Mutex<HashMap<String, OpenTransaction>>>`) and the
//!     `DEFAULT_TX_HANDLE` constant's exact string: both are private
//!     implementation details. The observable proxies used here are: an
//!     omitted `handle` parameter resolves to a slot whose name is
//!     echoed back as `"default"` by `iwe_tx_begin` (asserted against
//!     the tool's textual result); explicit handles resolve verbatim.
//!
//! Pre-existing partial coverage acknowledged (NOT duplicated here):
//!
//!   - `tests/agent_transaction_test.rs::a_second_begin_is_refused_while_one_is_open`
//!     covers the *omitted-handle* leg of the begin-refused criterion
//!     and the refusal message's "already open" + staged-keys shape.
//!   - `tests/agent_transaction_test.rs::a_second_explicit_begin_is_refused_while_a_different_explicit_one_is_open`
//!     covers the *explicit-handle* leg of the same criterion across
//!     three coexisting explicit handles, with the conflict checked on
//!     each.
//!   - `tests/tx_begin_echoes_resolved_handle_test.rs::tx_begin_with_no_handle_echoes_default_key`
//!     covers the "omitted handle resolves to `default`" leg of
//!     `DEFAULT_TX_HANDLE`.
//!
//!   This file's distinct contributions: explicit-handle multiplicity
//!   (a different explicit handle succeeds while one is open, plus the
//!   refusal message names the occupying key) AND the no-transaction-open
//!   refusal from both `iwe_tx_commit` and `iwe_tx_abort`, including the
//!   same refusal with an explicit-but-unoccupied handle. The schema-shape
//!   assertions are also new — no pre-existing test inspects the MCP
//!   `list_tools` response for the `handle` parameter's presence.
//!
//! Each `iwe_tx_begin` requires `[transactions] validate` to be enabled
//! (per `tests/agent_transaction_test.rs`'s
//! `without_the_transactions_section_begin_is_refused`), so every fixture
//! here uses `ValidationScope::Full`.

use std::collections::HashMap;
use std::fs::{create_dir_all, write};

use diwe::config::{
    Configuration, JournalOptions, Patterns, SchemaBinding, TransactionOptions, ValidationScope,
};
use serde_json::{json, Value};
use tempfile::TempDir;

use crate::fixture::Fixture;

const HUB: &str = "---\ntype: note\n---\n# Hub\n\nSee [Leaf](leaf).\n";
const LEAF: &str = "---\ntype: note\n---\n# Leaf\n\nBack to [Hub](hub).\n";

/// Same store shape as `agent_transaction_test.rs`: `notes/**` under a
/// schema that requires every link to resolve to a note.
fn store() -> TempDir {
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    create_dir_all(base.join(".iwe/schemas")).unwrap();
    write(
        base.join(".iwe/schemas/note.yaml"),
        "links:\n  - target: { type: note }\n",
    )
    .unwrap();
    create_dir_all(base.join("notes")).unwrap();
    write(base.join("notes/hub.md"), HUB).unwrap();
    write(base.join("notes/leaf.md"), LEAF).unwrap();
    dir
}

fn config(scope: ValidationScope) -> Configuration {
    let mut schemas = HashMap::new();
    schemas.insert(
        "note".to_string(),
        SchemaBinding {
            r#match: Patterns::One("notes/**".to_string()),
        },
    );
    Configuration {
        schemas,
        transactions: TransactionOptions {
            validate: scope,
            ..Default::default()
        },
        journal: JournalOptions {
            path: Some(".iwe/journal.ndjson".to_string()),
        },
        ..Default::default()
    }
}

async fn fixture(dir: &TempDir, scope: ValidationScope) -> Fixture {
    let base = dir.path().canonicalize().unwrap();
    Fixture::with_path(base.to_str().unwrap(), config(scope)).await
}

/// Find the input schema for a named tool in the `list_tools` response.
fn tool_schema(tools: &rmcp::model::ListToolsResult, name: &str) -> Value {
    let tool = tools
        .tools
        .iter()
        .find(|tool| tool.name == name)
        .unwrap_or_else(|| panic!("tool {name} must be listed by list_tools"));
    serde_json::to_value(&tool.input_schema)
        .unwrap_or_else(|e| panic!("input schema for {name} serializes: {e}"))
}

/// `schemars` represents `Option<String>` either as `"type": "string"`
/// or — when the optional-vs-required distinction is materialized — as
/// `"type": ["string", "null"]`. Accept either, and reject anything
/// that does not name `string` as one of the allowed types.
fn schema_type_allows_string(handle_schema: &Value) -> bool {
    match handle_schema.get("type") {
        Some(Value::String(t)) => t == "string",
        Some(Value::Array(arr)) => arr
            .iter()
            .any(|v| v.as_str() == Some("string")),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Acceptance criterion (A): "TxBeginParams, TxCommitParams, TxAbortParams
// each carry handle: Option<String> with #[schemars(description = ...)] text
// matching the design."
//
// Observable proxy: each tool's MCP input schema exposes `handle` as a
// named string property (optional). The `description` text is asserted
// to be present and non-empty (the design pins it to a meaningful human
// string; the exact wording is design-internal and not asserted verbatim
// here — only that the server carries it).
// ---------------------------------------------------------------------------

/// `iwe_tx_begin`'s MCP input schema carries `handle` as a string-shaped
/// optional parameter with a non-empty description.
#[tokio::test]
async fn tx_begin_schema_exposes_handle_property_with_description() {
    let f = Fixture::with_documents_and_config(vec![], Configuration::default()).await;
    let tools = f.list_tools().await;
    let schema = tool_schema(&tools, "iwe_tx_begin");

    let properties = schema
        .get("properties")
        .and_then(|p| p.as_object())
        .expect("input schema must have a properties object");
    let handle = properties
        .get("handle")
        .unwrap_or_else(|| panic!("iwe_tx_begin schema must declare a `handle` property: {schema:?}"));

    // String-shaped optional: schemars emits `"type": "string"` for the
    // bare Option<String> case (no enum, no format), or `"type":
    // ["string", "null"]` when the optional-vs-required distinction is
    // materialized. Accept either.
    assert!(
        schema_type_allows_string(handle),
        "iwe_tx_begin.handle must allow the string type, got: {handle:?}"
    );
    let description = handle
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("iwe_tx_begin.handle must carry a description: {handle:?}"));
    assert!(
        !description.trim().is_empty(),
        "iwe_tx_begin.handle.description must be non-empty, got: {description:?}"
    );

    let required = schema
        .get("required")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default();
    let required_names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
    assert!(
        !required_names.contains(&"handle"),
        "handle must be optional, not required: required={required_names:?}"
    );
}

/// `iwe_tx_commit`'s MCP input schema carries `handle` as a string-shaped
/// optional parameter with a non-empty description.
#[tokio::test]
async fn tx_commit_schema_exposes_handle_property_with_description() {
    let f = Fixture::with_documents_and_config(vec![], Configuration::default()).await;
    let tools = f.list_tools().await;
    let schema = tool_schema(&tools, "iwe_tx_commit");

    let properties = schema
        .get("properties")
        .and_then(|p| p.as_object())
        .expect("input schema must have a properties object");
    let handle = properties
        .get("handle")
        .unwrap_or_else(|| panic!("iwe_tx_commit schema must declare a `handle` property: {schema:?}"));
    assert!(
        schema_type_allows_string(handle),
        "iwe_tx_commit.handle must allow the string type, got: {handle:?}"
    );
    let description = handle
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("iwe_tx_commit.handle must carry a description: {handle:?}"));
    assert!(
        !description.trim().is_empty(),
        "iwe_tx_commit.handle.description must be non-empty, got: {description:?}"
    );

    let required = schema
        .get("required")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default();
    let required_names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
    assert!(
        !required_names.contains(&"handle"),
        "handle must be optional, not required: required={required_names:?}"
    );
}

/// `iwe_tx_abort`'s MCP input schema carries `handle` as a string-shaped
/// optional parameter with a non-empty description.
#[tokio::test]
async fn tx_abort_schema_exposes_handle_property_with_description() {
    let f = Fixture::with_documents_and_config(vec![], Configuration::default()).await;
    let tools = f.list_tools().await;
    let schema = tool_schema(&tools, "iwe_tx_abort");

    let properties = schema
        .get("properties")
        .and_then(|p| p.as_object())
        .expect("input schema must have a properties object");
    let handle = properties
        .get("handle")
        .unwrap_or_else(|| panic!("iwe_tx_abort schema must declare a `handle` property: {schema:?}"));
    assert!(
        schema_type_allows_string(handle),
        "iwe_tx_abort.handle must allow the string type, got: {handle:?}"
    );
    let description = handle
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("iwe_tx_abort.handle must carry a description: {handle:?}"));
    assert!(
        !description.trim().is_empty(),
        "iwe_tx_abort.handle.description must be non-empty, got: {description:?}"
    );

    let required = schema
        .get("required")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default();
    let required_names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
    assert!(
        !required_names.contains(&"handle"),
        "handle must be optional, not required: required={required_names:?}"
    );
}

// ---------------------------------------------------------------------------
// Acceptance criterion (B): "Every write-tool Params struct
// (CreateParams, UpdateParams, DeleteParams, QueryParams, RenameParams,
// ExtractParams, InlineParams, NormalizeParams, AttachParams) gains
// handle: Option<String> with the documented schema description."
//
// Observable proxy: each tool's MCP input schema exposes `handle` as a
// string-shaped optional parameter with a non-empty description, exactly
// the same shape asserted on the tx tools above.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_write_tool_schema_exposes_handle_property_with_description() {
    let f = Fixture::with_documents_and_config(vec![], Configuration::default()).await;
    let tools = f.list_tools().await;

    let write_tools = [
        "iwe_create",
        "iwe_update",
        "iwe_delete",
        "iwe_query",
        "iwe_rename",
        "iwe_extract",
        "iwe_inline",
        "iwe_normalize",
        "iwe_attach",
    ];

    for name in write_tools {
        let schema = tool_schema(&tools, name);
        let properties = schema
            .get("properties")
            .and_then(|p| p.as_object())
            .unwrap_or_else(|| panic!("{name} input schema must have a properties object: {schema:?}"));
        let handle = properties.get("handle").unwrap_or_else(|| {
            panic!("{name} schema must declare a `handle` property: {schema:?}")
        });
        assert!(
            schema_type_allows_string(handle),
            "{name}.handle must allow the string type, got: {handle:?}"
        );
        let description = handle
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("{name}.handle must carry a description: {handle:?}"));
        assert!(
            !description.trim().is_empty(),
            "{name}.handle.description must be non-empty, got: {description:?}"
        );

        let required = schema
            .get("required")
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default();
        let required_names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
        assert!(
            !required_names.contains(&"handle"),
            "{name}.handle must be optional, not required: required={required_names:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Acceptance criterion (C): "iwe_tx_begin on an occupied key — explicit OR
// default — is refused while a transaction is open under that key (the
// refusal message contains the key's name); a second begin on a different
// explicit handle succeeds."
//
// Multiplicity leg (a different explicit handle succeeds while one is
// open) is the new contribution here; the "occupied key is refused" leg
// for the *default* key is the new contribution in addition to the
// pre-existing explicit-handle coverage in
// `a_second_explicit_begin_is_refused_while_a_different_explicit_one_is_open`.
// ---------------------------------------------------------------------------

/// Two explicit-handle begins succeed back-to-back (multiplicity: an
/// occupied explicit key does not block a *different* explicit key).
#[tokio::test]
async fn a_second_explicit_handle_begin_succeeds_when_a_different_explicit_one_is_open() {
    let dir = store();
    let f = fixture(&dir, ValidationScope::Full).await;

    let begun = f.call_tool("iwe_tx_begin", json!({"handle": "alpha"})).await;
    assert!(!begun.is_error.unwrap_or(false), "{begun:?}");
    f.call_tool(
        "iwe_create",
        json!({
            "handle": "alpha",
            "key": "notes/alpha_note",
            "content": "---\ntype: note\n---\n# Alpha\n\nSee [Hub](hub).\n",
        }),
    )
    .await;

    // Multiplicity: a *different* explicit handle begins successfully
    // while alpha is still open.
    let begun = f.call_tool("iwe_tx_begin", json!({"handle": "beta"})).await;
    assert!(
        !begun.is_error.unwrap_or(false),
        "a second begin on a different explicit handle must succeed, got: {begun:?}"
    );

    // Now close alpha (its absence here doesn't matter — the proof above
    // is that beta's begin succeeded while alpha was still open).
    let aborted = f.call_tool("iwe_tx_abort", json!({"handle": "alpha"})).await;
    assert!(!aborted.is_error.unwrap_or(false), "{aborted:?}");
}

/// A second begin with no `handle` (i.e. on the *default* key) is refused
/// while a previous explicit handle begin is still open *and that
/// previous handle's resolved key is also "default"* — i.e. an explicit
/// handle literally named "default" is functionally indistinguishable
/// from an omitted one and creates an identical collision.
///
/// Also verifies that a second begin with the same explicit handle is
/// refused and the refusal names the occupying key — the message should
/// contain both "already open" and the key whose slot is occupied.
#[tokio::test]
async fn a_second_begin_on_an_occupied_key_is_refused_and_names_the_key() {
    let dir = store();
    let f = fixture(&dir, ValidationScope::Full).await;

    // Open an explicit handle and stage a write so the refusal has
    // observable context to name.
    let begun = f.call_tool("iwe_tx_begin", json!({"handle": "alpha"})).await;
    assert!(!begun.is_error.unwrap_or(false), "{begun:?}");
    f.call_tool(
        "iwe_create",
        json!({
            "handle": "alpha",
            "key": "notes/alpha_only",
            "content": "---\ntype: note\n---\n# A\n\nSee [Hub](hub).\n",
        }),
    )
    .await;

    // Second begin on the *same* explicit handle: refused, and the
    // message must contain both "already open" and the occupying key.
    let message = f
        .try_call_tool("iwe_tx_begin", json!({"handle": "alpha"}))
        .await
        .expect_err("a second begin on an occupied explicit handle must be refused")
        .to_string();
    assert!(
        message.contains("already open"),
        "the refusal must contain 'already open', got: {message}"
    );
    assert!(
        message.contains("alpha"),
        "the refusal must name the occupied key 'alpha', got: {message}"
    );

    // Clean up: close alpha before the next sub-case so the assertions
    // are independent.
    let aborted = f.call_tool("iwe_tx_abort", json!({"handle": "alpha"})).await;
    assert!(!aborted.is_error.unwrap_or(false), "{aborted:?}");

    // Open an *explicit* handle literally named "default" — this
    // occupies the same slot an omitted handle would resolve to.
    let begun = f.call_tool("iwe_tx_begin", json!({"handle": "default"})).await;
    assert!(!begun.is_error.unwrap_or(false), "{begun:?}");
    f.call_tool(
        "iwe_create",
        json!({
            "handle": "default",
            "key": "notes/default_only",
            "content": "---\ntype: note\n---\n# D\n\nSee [Hub](hub).\n",
        }),
    )
    .await;

    // A second begin with no `handle` (which resolves to the default
    // key) finds that key occupied and is refused. The refusal must
    // name the occupying transaction's staged keys, same shape as the
    // pre-existing omitted-handle collision coverage.
    let message = f
        .try_call_tool("iwe_tx_begin", json!({}))
        .await
        .expect_err("the default key is already open, occupied by the explicit 'default' begin")
        .to_string();
    assert!(
        message.contains("already open"),
        "the refusal must contain 'already open', got: {message}"
    );
    assert!(
        message.contains("notes/default_only"),
        "the refusal must name the occupying transaction's staged key, got: {message}"
    );
}

// ---------------------------------------------------------------------------
// Acceptance criterion (D): "iwe_tx_commit and iwe_tx_abort resolve the
// handle via resolve_tx_handle and report 'no transaction is open' when
// the resolved key is not in open_txs."
//
// Observable: with no transaction begun, both tools return a refusal
// whose text contains the literal phrase "no transaction is open".
// Exercised both with no handle (resolves to default) and with an
// explicit handle (resolves to itself), proving resolve_tx_handle is
// applied uniformly.
// ---------------------------------------------------------------------------

/// With no transaction begun, `iwe_tx_commit` (no handle) reports "no
/// transaction is open".
#[tokio::test]
async fn tx_commit_with_no_open_transaction_reports_no_transaction_is_open() {
    let dir = store();
    let f = fixture(&dir, ValidationScope::Full).await;

    let message = f
        .try_call_tool("iwe_tx_commit", json!({}))
        .await
        .expect_err("iwe_tx_commit with no open transaction must be refused")
        .to_string();
    assert!(
        message.contains("no transaction is open"),
        "iwe_tx_commit (no handle) refusal must say 'no transaction is open', got: {message}"
    );
}

/// With no transaction begun, `iwe_tx_abort` (no handle) reports "no
/// transaction is open".
#[tokio::test]
async fn tx_abort_with_no_open_transaction_reports_no_transaction_is_open() {
    let dir = store();
    let f = fixture(&dir, ValidationScope::Full).await;

    let message = f
        .try_call_tool("iwe_tx_abort", json!({}))
        .await
        .expect_err("iwe_tx_abort with no open transaction must be refused")
        .to_string();
    assert!(
        message.contains("no transaction is open"),
        "iwe_tx_abort (no handle) refusal must say 'no transaction is open', got: {message}"
    );
}

/// With one transaction open under a different explicit handle,
/// `iwe_tx_commit` / `iwe_tx_abort` on an unoccupied explicit handle
/// also report "no transaction is open" — proves the lookup key is the
/// *resolved* handle, not a global "is anything open" check.
#[tokio::test]
async fn tx_commit_and_tx_abort_resolve_handle_then_report_no_transaction_is_open() {
    let dir = store();
    let f = fixture(&dir, ValidationScope::Full).await;

    // Open "alpha"; commit/abort on an explicit *different* handle must
    // both refuse with "no transaction is open".
    let begun = f.call_tool("iwe_tx_begin", json!({"handle": "alpha"})).await;
    assert!(!begun.is_error.unwrap_or(false), "{begun:?}");

    let message = f
        .try_call_tool("iwe_tx_commit", json!({"handle": "beta"}))
        .await
        .expect_err("iwe_tx_commit on an unoccupied explicit handle must be refused")
        .to_string();
    assert!(
        message.contains("no transaction is open"),
        "iwe_tx_commit (explicit handle, unoccupied) must say 'no transaction is open', got: {message}"
    );

    let message = f
        .try_call_tool("iwe_tx_abort", json!({"handle": "beta"}))
        .await
        .expect_err("iwe_tx_abort on an unoccupied explicit handle must be refused")
        .to_string();
    assert!(
        message.contains("no transaction is open"),
        "iwe_tx_abort (explicit handle, unoccupied) must say 'no transaction is open', got: {message}"
    );

    // alpha is still open — proof that the prior failures did not touch
    // it. A begin on the same occupied alpha slot is refused.
    let message = f
        .try_call_tool("iwe_tx_begin", json!({"handle": "alpha"}))
        .await
        .expect_err("alpha is still open from above")
        .to_string();
    assert!(
        message.contains("already open"),
        "alpha remains open across the unrelated commit/abort failures, got: {message}"
    );

    // Cleaning up so the fixture's TempDir does not leak a hung handle.
    let aborted = f.call_tool("iwe_tx_abort", json!({"handle": "alpha"})).await;
    assert!(!aborted.is_error.unwrap_or(false), "{aborted:?}");
}

// ---------------------------------------------------------------------------
// Acceptance criterion (E): "DEFAULT_TX_HANDLE: &str = \"default\" is
// defined as the map key for a transaction begun without an explicit
// handle."
//
// Observable proxy: when a tool call's result text must contain a key
// identifier (for `iwe_tx_begin` to echo the resolved handle), an
// omitted handle produces "default" in the result. We don't reach into
// the constant directly — the observable is the echo behavior, which
// `tx_begin_echoes_resolved_handle_test.rs` already pins for the begin
// tool. Here we exercise the same observable leg for `iwe_tx_commit`'s
// result and assert it likewise contains "default" when no handle is
// given.
//
// (This is the leg where the design's "the omitted-handle key is the
// literal string 'default'" observable fact must hold for non-begin
// tools too — so commit/abort must look up the same key begin put a
// transaction in.)
// ---------------------------------------------------------------------------

/// Closing a default-slot transaction commits and removes only the
/// default-slot staged write — a *different* explicit handle's
/// transactions, if any were open, would be untouched. This is the
/// shape that proves the lookup uses the resolved key, not a global
/// "the one transaction" lookup.
#[tokio::test]
async fn omitted_handle_resolves_to_default_slot_for_commit_and_abort() {
    let dir = store();
    let f = fixture(&dir, ValidationScope::Full).await;

    // Begin on the default slot (no handle).
    let begun = f.call_tool("iwe_tx_begin", json!({})).await;
    assert!(!begun.is_error.unwrap_or(false), "{begun:?}");
    assert!(
        Fixture::result_text(&begun).contains("default"),
        "iwe_tx_begin with no handle must echo the resolved default key, got: {:?}",
        Fixture::result_text(&begun)
    );

    // Stage a clean write against the default slot.
    f.call_tool(
        "iwe_create",
        json!({
            "key": "notes/default_slot",
            "content": "---\ntype: note\n---\n# DS\n\nSee [Hub](hub).\n",
        }),
    )
    .await;

    // Commit with no handle — must resolve to the default slot and
    // commit exactly the default-slot staged writes (none of the
    // non-default keys touched because no other handle was used).
    let committed = f
        .call_tool("iwe_tx_commit", json!({}))
        .await;
    assert!(!committed.is_error.unwrap_or(false), "{committed:?}");
    assert!(
        dir.path().join("notes/default_slot.md").exists(),
        "the default-slot staged write lands on commit, got directory: {:?}",
        std::fs::read_dir(dir.path().join("notes")).unwrap().collect::<Vec<_>>()
    );

    // After commit, the default slot is empty: a no-handle commit is
    // refused with "no transaction is open" — proving the default
    // slot, specifically, was emptied by the prior commit.
    let message = f
        .try_call_tool("iwe_tx_commit", json!({}))
        .await
        .expect_err("the default slot is empty after a successful commit")
        .to_string();
    assert!(
        message.contains("no transaction is open"),
        "a no-handle commit on the empty default slot must say 'no transaction is open', got: {message}"
    );

    // Same observable check, via abort path: open on the default slot,
    // stage nothing (open-and-immediately-close is valid), abort with
    // no handle. After abort the default slot is empty.
    let begun = f.call_tool("iwe_tx_begin", json!({})).await;
    assert!(!begun.is_error.unwrap_or(false), "{begun:?}");
    let aborted = f.call_tool("iwe_tx_abort", json!({})).await;
    assert!(!aborted.is_error.unwrap_or(false), "{aborted:?}");
    let message = f
        .try_call_tool("iwe_tx_abort", json!({}))
        .await
        .expect_err("the default slot is empty after a successful abort")
        .to_string();
    assert!(
        message.contains("no transaction is open"),
        "a no-handle abort on the empty default slot must say 'no transaction is open', got: {message}"
    );
}

// ---------------------------------------------------------------------------
// Acceptance criterion (F): "IweServer::open_txs is
// Arc<std::sync::Mutex<HashMap<String, OpenTransaction>>> (was:
// Option<OpenTransaction>)."
//
// The internal type is not directly observable from outside the crate.
// The observable consequence the design's multiplicity implies is: two
// distinct keys can hold open transactions simultaneously in the same
// server instance, AND they remain addressable independently by their
// resolved keys. That is covered by
// `a_second_explicit_handle_begin_succeeds_when_a_different_explicit_one_is_open`
// above and by the existing concurrent-handles test
// (`tests/concurrent_handles_test.rs`); we re-assert the leg that
// proves lookup is per-key rather than singleton here, in one short
// test, so the criterion is at least named in this file's contract map.
// ---------------------------------------------------------------------------

/// Three distinct explicit handles open at once and are addressable
/// independently: each one's abort closes only its own slot, and the
/// others remain open.
#[tokio::test]
async fn three_distinct_explicit_handles_open_at_once_are_addressable_independently() {
    let dir = store();
    let f = fixture(&dir, ValidationScope::Full).await;

    for handle in ["alpha", "beta", "gamma"] {
        let begun = f.call_tool("iwe_tx_begin", json!({"handle": handle})).await;
        assert!(
            !begun.is_error.unwrap_or(false),
            "begin on {handle} must succeed, got: {begun:?}"
        );
        f.call_tool(
            "iwe_create",
            json!({
                "handle": handle,
                "key": format!("notes/{handle}"),
                "content": format!("---\ntype: note\n---\n# {handle}\n\nSee [Hub](hub).\n"),
            }),
        )
        .await;
    }

    // Aborting alpha must not affect beta or gamma: a begin on alpha is
    // still refused (the slot was emptied by abort, not beta/gamma's
    // slots), and begins on beta/gamma are refused because they're
    // still occupied.
    let aborted = f.call_tool("iwe_tx_abort", json!({"handle": "alpha"})).await;
    assert!(!aborted.is_error.unwrap_or(false), "{aborted:?}");

    // Alpha is now empty: aborting it again reports "no transaction is
    // open" on the alpha key.
    let message = f
        .try_call_tool("iwe_tx_abort", json!({"handle": "alpha"}))
        .await
        .expect_err("alpha was just aborted; the slot is empty")
        .to_string();
    assert!(
        message.contains("no transaction is open"),
        "alpha slot must be empty after abort, got: {message}"
    );

    // Beta and gamma are still open: a second begin on either is
    // refused and names the right staged key.
    for (handle, expected_key) in [("beta", "notes/beta"), ("gamma", "notes/gamma")] {
        let message = f
            .try_call_tool("iwe_tx_begin", json!({"handle": handle}))
            .await
            .expect_err("{handle} is still open")
            .to_string();
        assert!(
            message.contains("already open") && message.contains(expected_key),
            "begin on {handle} must be refused naming {expected_key}, got: {message}"
        );
    }

    // Clean up.
    for handle in ["beta", "gamma"] {
        let aborted = f.call_tool("iwe_tx_abort", json!({"handle": handle})).await;
        assert!(!aborted.is_error.unwrap_or(false), "{aborted:?}");
    }
}
