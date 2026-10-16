// The supervisor's wait for a signal, end to end: the agent's MCP client
// against the real server over Streamable HTTP, a poller's `record` on the
// server side. A wait ends the moment a signal is queued; a wait the client
// abandons (cancelled, as at shutdown) takes nothing, so the signal it would
// have woken on is still there for the next get_next_signal.

mod support;

use std::time::{Duration, Instant};

use agent::mcp::{ConnectOptions, Mcp, McpClient, open_http};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

fn wait_args(ms: u64) -> Map<String, Value> {
    json!({ "timeoutMs": ms }).as_object().cloned().unwrap_or_default()
}

#[test]
fn a_wait_ends_on_the_signal_and_an_abandoned_wait_takes_nothing() {
    support::runtime().block_on(async {
        let db = support::test_database().await;
        let server = support::start_test_server(&db, None).await.unwrap();
        let mcp = McpClient::connect(open_http(server.url.clone()), ConnectOptions::default()).await.unwrap();
        let never = CancellationToken::new();

        // A wait ends on the record, long before its 20 s deadline.
        let started = Instant::now();
        let (waited, recorded) = tokio::join!(mcp.call_tool("wait_for_signal", wait_args(20_000), &never), async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            server.signals.record("telegram", "first").await.unwrap()
        });
        let waited: Value = serde_json::from_str(&waited.unwrap()).unwrap();
        assert_eq!(waited, json!({ "pending": 1 }));
        assert!(started.elapsed() < Duration::from_secs(5), "woken by the signal: {:?}", started.elapsed());
        let next: Value =
            serde_json::from_str(&mcp.call_tool("get_next_signal", Map::new(), &never).await.unwrap()).unwrap();
        assert_eq!(next["signal"]["id"], json!(recorded));

        // An abandoned wait: the caller cancels it, then a signal arrives.
        let cancel = CancellationToken::new();
        let abandon = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            abandon.cancel();
        });
        let started = Instant::now();
        assert!(mcp.call_tool("wait_for_signal", wait_args(20_000), &cancel).await.is_err());
        assert!(started.elapsed() < Duration::from_secs(5), "a cancelled wait returns at once");
        let second = server.signals.record("telegram", "second").await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let next: Value =
            serde_json::from_str(&mcp.call_tool("get_next_signal", Map::new(), &never).await.unwrap()).unwrap();
        assert_eq!(next["signal"]["id"], json!(second), "the abandoned wait took nothing");

        // Past the cap the server refuses instead of outliving the request.
        let refused = mcp.call_tool("wait_for_signal", wait_args(60_000), &never).await;
        assert!(refused.is_err() || refused.as_deref().is_ok_and(|t| t.contains("50000")), "{refused:?}");

        mcp.close().await;
        db.drop_all().await;
    });
}
