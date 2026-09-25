// `[integrity]` at the MCP surface, in real scratch stores: every write
// tool and `iwe_tx_commit` go through the one commit gate, a refusal
// changes nothing on disk and carries `iwe_error: "link_integrity"`, and
// `iwe_create`'s `link_from` lands a page with the link that makes it
// reachable in one commit (or stages both on a transaction).

use std::fs::{create_dir_all, read_to_string, write};

use diwe::config::{
    ActionDefinition, Attach, Configuration, IntegrityMode, IntegrityOptions, MarkdownOptions,
};
use rmcp::model::ErrorData;
use rmcp::ServiceError;
use serde_json::json;
use tempfile::TempDir;

use crate::fixture::Fixture;

const INDEX: &str = "# Index\n\n- [Hub](hub)\n";
const HUB: &str = "# Hub\n\n- [Leaf](leaf)\n";
const LEAF: &str = "# Leaf\n\n## Part\n\nText.\n";
const OLD: &str = "# Old\n\nSee [Gone](gone).\n";

const MODES: [IntegrityMode; 2] = [IntegrityMode::NoNew, IntegrityMode::Strict];

fn store(with_debt: bool) -> TempDir {
    let dir = TempDir::new().unwrap();
    let base = dir.path();
    create_dir_all(base.join(".iwe")).unwrap();
    write(base.join("index.md"), INDEX).unwrap();
    write(base.join("hub.md"), HUB).unwrap();
    write(base.join("leaf.md"), LEAF).unwrap();
    if with_debt {
        write(base.join("old.md"), OLD).unwrap();
    }
    dir
}

fn config(mode: IntegrityMode) -> Configuration {
    let mut config = Configuration {
        markdown: MarkdownOptions {
            refs_extension: String::new(),
            ..Default::default()
        },
        integrity: IntegrityOptions {
            links: mode,
            orphans: mode,
            root: "index".to_string(),
        },
        ..Default::default()
    };
    config.actions.insert(
        "daily".to_string(),
        ActionDefinition::Attach(Attach {
            title: "Daily".to_string(),
            key_template: "daily".to_string(),
            document_template: "# Daily\n\n{{content}}\n".to_string(),
        }),
    );
    config
}

async fn fixture(dir: &TempDir, mode: IntegrityMode) -> Fixture {
    let base = dir.path().canonicalize().unwrap();
    Fixture::with_path(base.to_str().unwrap(), config(mode)).await
}

fn read(dir: &TempDir, key: &str) -> String {
    read_to_string(dir.path().join(format!("{key}.md"))).unwrap()
}

fn refused(result: Result<rmcp::model::CallToolResult, ServiceError>) -> ErrorData {
    match result.expect_err("expected a refusal") {
        ServiceError::McpError(error) => error,
        other => panic!("expected McpError, got: {other:?}"),
    }
}

fn assert_integrity_refusal(error: &ErrorData, needles: &[&str]) {
    assert_eq!(
        error.data,
        Some(json!({ "iwe_error": "link_integrity" })),
        "{}",
        error.message
    );
    for needle in needles {
        assert!(error.message.contains(needle), "missing {needle:?} in: {}", error.message);
    }
}

#[tokio::test]
async fn create_without_a_link_is_refused_and_with_link_from_is_accepted() {
    for mode in MODES {
        let dir = store(false);
        let f = fixture(&dir, mode).await;

        let error = refused(
            f.try_call_tool("iwe_create", json!({"key": "page", "content": "# Page\n"}))
                .await,
        );
        assert_integrity_refusal(
            &error,
            &[
                "orphan: page",
                "pass link_from=<a reachable parent key> to iwe_create, or create the page and its link in one transaction (iwe_tx_begin … iwe_tx_commit)",
            ],
        );
        assert!(!dir.path().join("page.md").exists());

        let result = f
            .call_tool(
                "iwe_create",
                json!({"key": "topics/page", "content": "# Page\n", "link_from": "hub"}),
            )
            .await;
        assert_eq!(Fixture::result_json(&result)["created"], true);
        assert_eq!(read(&dir, "topics/page"), "# Page\n");
        assert_eq!(read(&dir, "hub"), "# Hub\n\n- [Leaf](leaf)\n- [Page](topics/page)\n");
    }
}

#[tokio::test]
async fn link_from_resolves_relative_to_the_parent_directory() {
    let dir = store(false);
    create_dir_all(dir.path().join("topics")).unwrap();
    write(dir.path().join("topics/index.md"), "# Topics\n\nIntro.\n").unwrap();
    write(dir.path().join("index.md"), "# Index\n\n- [Hub](hub)\n- [Topics](topics/index)\n").unwrap();
    let f = fixture(&dir, IntegrityMode::Strict).await;
    f.call_tool(
        "iwe_create",
        json!({"key": "topics/sub/page", "content": "# Sub Page\n", "link_from": "topics/index"}),
    )
    .await;
    assert_eq!(
        read(&dir, "topics/index"),
        "# Topics\n\nIntro.\n\n- [Sub Page](sub/page)\n"
    );
}

#[tokio::test]
async fn a_missing_link_from_is_an_error_and_writes_nothing() {
    let dir = store(false);
    let f = fixture(&dir, IntegrityMode::NoNew).await;
    let error = refused(
        f.try_call_tool(
            "iwe_create",
            json!({"key": "page", "content": "# Page\n", "link_from": "nowhere"}),
        )
        .await,
    );
    assert!(error.message.contains("'nowhere' does not exist"), "{}", error.message);
    assert!(!dir.path().join("page.md").exists());
}

#[tokio::test]
async fn link_from_works_inside_a_transaction() {
    let dir = store(false);
    let f = fixture(&dir, IntegrityMode::NoNew).await;
    let begun = f.call_tool("iwe_tx_begin", json!({"handle": "t1"})).await;
    assert_eq!(Fixture::result_json(&begun)["status"], "open");
    f.call_tool(
        "iwe_create",
        json!({"key": "page", "content": "# Page\n", "link_from": "hub", "handle": "t1"}),
    )
    .await;
    assert!(!dir.path().join("page.md").exists(), "staged, not written");
    assert_eq!(read(&dir, "hub"), HUB);
    f.call_tool("iwe_tx_commit", json!({"handle": "t1"})).await;
    assert_eq!(read(&dir, "page"), "# Page\n");
    assert!(read(&dir, "hub").contains("- [Page](page)"));
}

#[tokio::test]
async fn a_page_and_its_link_in_one_transaction_are_accepted_alone_refused() {
    for mode in MODES {
        let dir = store(false);
        let f = fixture(&dir, mode).await;

        // The page alone, committed: refused whole, with the code.
        f.call_tool("iwe_tx_begin", json!({"handle": "alone"})).await;
        f.call_tool(
            "iwe_create",
            json!({"key": "page", "content": "# Page\n", "handle": "alone"}),
        )
        .await;
        let error = refused(f.try_call_tool("iwe_tx_commit", json!({"handle": "alone"})).await);
        assert_integrity_refusal(&error, &["orphan: page"]);
        assert!(!dir.path().join("page.md").exists());

        // The page and the link to it: accepted together.
        f.call_tool("iwe_tx_begin", json!({"handle": "both"})).await;
        f.call_tool(
            "iwe_create",
            json!({"key": "page", "content": "# Page\n", "handle": "both"}),
        )
        .await;
        f.call_tool(
            "iwe_update",
            json!({"key": "hub", "content": "# Hub\n\n- [Leaf](leaf)\n- [Page](page)\n", "handle": "both"}),
        )
        .await;
        f.call_tool("iwe_tx_commit", json!({"handle": "both"})).await;
        assert_eq!(read(&dir, "page"), "# Page\n");
    }
}

#[tokio::test]
async fn a_link_to_a_missing_key_is_refused() {
    for mode in MODES {
        let dir = store(false);
        let f = fixture(&dir, mode).await;
        let error = refused(
            f.try_call_tool(
                "iwe_update",
                json!({"key": "leaf", "content": "# Leaf\n\n[M](missing)\n"}),
            )
            .await,
        );
        assert_integrity_refusal(&error, &["broken link: leaf → missing"]);
        assert_eq!(read(&dir, "leaf"), LEAF);
    }
}

#[tokio::test]
async fn delete_and_query_delete_that_orphan_a_page_are_refused() {
    for mode in MODES {
        let dir = store(false);
        let f = fixture(&dir, mode).await;
        let error = refused(f.try_call_tool("iwe_delete", json!({"key": "hub"})).await);
        assert_integrity_refusal(&error, &["orphan: leaf"]);
        assert_eq!(read(&dir, "hub"), HUB);
        assert_eq!(read(&dir, "index"), INDEX);

        let error = refused(
            f.try_call_tool(
                "iwe_query",
                json!({"operation": "delete", "document": "filter: { $key: hub }\nexpect: 1\n"}),
            )
            .await,
        );
        assert_integrity_refusal(&error, &["orphan: leaf"]);
        assert_eq!(read(&dir, "hub"), HUB);

        // Deleting the leaf: its links are removed in the same commit.
        f.call_tool("iwe_delete", json!({"key": "leaf"})).await;
        assert!(!dir.path().join("leaf.md").exists());
        assert!(!read(&dir, "hub").contains("leaf"));
    }
}

#[tokio::test]
async fn rename_extract_and_inline_keep_integrity() {
    for mode in MODES {
        let dir = store(false);
        let f = fixture(&dir, mode).await;
        f.call_tool("iwe_rename", json!({"old_key": "hub", "new_key": "topics/hub"}))
            .await;
        assert!(read(&dir, "index").contains("(topics/hub)"));

        // Extract leaves an inclusion link to the new page: reachable.
        let extracted = f
            .call_tool("iwe_extract", json!({"key": "leaf", "section": "Part"}))
            .await;
        let created = Fixture::result_json(&extracted)["creates"][0]["key"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(dir.path().join(format!("{created}.md")).exists());

        // Inline it back; the extracted page is removed with its only link.
        f.call_tool("iwe_inline", json!({"key": "leaf", "reference": created}))
            .await;
        assert!(read(&dir, "leaf").contains("Text."));
    }
}

#[tokio::test]
async fn attach_creating_an_unreachable_target_is_refused() {
    let dir = store(false);
    let f = fixture(&dir, IntegrityMode::NoNew).await;
    let error = refused(
        f.try_call_tool("iwe_attach", json!({"to": ["daily"], "key": "leaf"}))
            .await,
    );
    assert_integrity_refusal(&error, &["orphan: daily"]);
    assert!(!dir.path().join("daily.md").exists());
}

#[tokio::test]
async fn no_new_tolerates_old_debt_and_strict_refuses_it() {
    let dir = store(true);
    let f = fixture(&dir, IntegrityMode::NoNew).await;
    f.call_tool("iwe_update", json!({"key": "leaf", "content": "# Leaf\n\nMore.\n"}))
        .await;
    f.call_tool("iwe_update", json!({"key": "old", "content": "# Old\n"}))
        .await;
    assert_eq!(read(&dir, "old"), "# Old\n");

    let dir = store(true);
    let f = fixture(&dir, IntegrityMode::Strict).await;
    let error = refused(
        f.try_call_tool("iwe_update", json!({"key": "leaf", "content": "# Leaf\n\nMore.\n"}))
            .await,
    );
    assert_integrity_refusal(&error, &["orphan: old", "broken link: old → gone"]);
    assert_eq!(read(&dir, "leaf"), LEAF);
}

#[tokio::test]
async fn off_leaves_behaviour_unchanged() {
    let dir = store(true);
    let f = fixture(&dir, IntegrityMode::Off).await;
    f.call_tool("iwe_create", json!({"key": "page", "content": "# Page\n\n[M](missing)\n"}))
        .await;
    assert!(dir.path().join("page.md").exists());
    f.call_tool("iwe_delete", json!({"key": "hub"})).await;
    let stats = f.call_tool("iwe_stats", json!({})).await;
    assert!(Fixture::result_json(&stats).get("integrity").is_none());
}

#[tokio::test]
async fn stats_report_the_debt_when_enabled() {
    let dir = store(true);
    let f = fixture(&dir, IntegrityMode::NoNew).await;
    let stats = f.call_tool("iwe_stats", json!({})).await;
    let json = Fixture::result_json(&stats);
    assert_eq!(json["integrity"]["brokenLinkCount"], 1);
    assert_eq!(json["integrity"]["unreachableDocuments"], 1);
    assert_eq!(json["brokenLinkCount"], 1);
}
