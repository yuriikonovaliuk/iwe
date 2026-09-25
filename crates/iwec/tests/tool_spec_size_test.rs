//! The compact tool listing is what every agent request carries; this pins
//! its size so a new parameter cannot quietly bloat it.

use crate::fixture::Fixture;

#[tokio::test]
async fn compact_tool_listing_stays_small() {
    let f = Fixture::with_documents(vec![]).await;
    let tools = f.list_tools().await;
    let bytes = serde_json::to_string(&tools.tools).unwrap().len();
    eprintln!("compact tool listing: {bytes} bytes");
    assert!(bytes < 12_000, "compact tool listing grew to {bytes} bytes");
}
