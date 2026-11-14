use crate::fixture::Fixture;
use serde_json::json;

// A misspelled argument used to be dropped by serde, so `iwe_retrieve` with
// `key` (for `keys`) ran with `keys: []` and answered `[]` as if nothing
// matched. It must be refused, naming the argument and the accepted ones.
#[tokio::test]
async fn retrieve_refuses_misspelled_key_argument() {
    let f = Fixture::with_documents(vec![("1", "# Hello world\n\nSome content\n")]).await;

    let err = f
        .try_call_tool("iwe_retrieve", json!({"key": "1"}))
        .await
        .expect_err("an unknown argument must be refused");
    let msg = err.to_string();
    assert!(msg.contains("unknown argument(s) key"), "{msg}");
    assert!(msg.contains("keys"), "accepted arguments are listed: {msg}");
}

#[tokio::test]
async fn every_tool_refuses_an_unknown_argument() {
    let f = Fixture::with_documents(vec![("1", "# Hello world\n")]).await;
    for tool in [
        "iwe_find",
        "iwe_retrieve",
        "iwe_tree",
        "iwe_stats",
        "iwe_query",
        "iwe_argue",
    ] {
        let err = f
            .try_call_tool(tool, json!({"no_such_argument": 1}))
            .await
            .expect_err(tool);
        assert!(
            err.to_string().contains("no_such_argument"),
            "{tool}: {err}"
        );
    }
}

#[tokio::test]
async fn declared_arguments_still_work() {
    let f = Fixture::with_documents(vec![("1", "# Hello world\n\nSome content\n")]).await;
    let result = f
        .call_tool(
            "iwe_retrieve",
            json!({"keys": ["1"], "backlinks": false, "max_document_tokens": 50}),
        )
        .await;
    let output = Fixture::result_json(&result);
    assert_eq!(output[0]["key"], "1");
}

#[test]
fn schema_allowing_additional_properties_accepts_anything() {
    let schema: rmcp::model::JsonObject =
        serde_json::from_value(json!({"properties": {"a": {}}, "additionalProperties": true}))
            .unwrap();
    let args: rmcp::model::JsonObject = serde_json::from_value(json!({"b": 1})).unwrap();
    assert!(iwec::unknown_arguments(&schema, &args).is_empty());

    let closed: rmcp::model::JsonObject =
        serde_json::from_value(json!({"properties": {"a": {}}})).unwrap();
    assert_eq!(
        iwec::unknown_arguments(&closed, &args),
        vec!["b".to_string()]
    );
}
