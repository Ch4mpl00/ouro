// Cross-language contract against packages/mcp's HTTP test process: the same
// fixture used by TS test/mcp-reconnect.test.ts, without importing server code.
// Requires pnpm dependencies, so run explicitly with --ignored.
use agent::mcp::{ConnectOptions, Mcp, McpClient, open_http};
use serde_json::{Map, Value, json};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_util::sync::CancellationToken;

struct ChildServer {
    child: tokio::process::Child,
    port: u16,
}
impl Drop for ChildServer {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}
impl ChildServer {
    async fn start(ledger: &std::path::Path, port: u16, multi: bool) -> Self {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut child = tokio::process::Command::new("node")
            .args(["--import", "tsx", "packages/mcp/test/support/mcp-child.ts"])
            .current_dir(root)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("PORT", port.to_string())
            .env("LEDGER", ledger)
            .env("GEN", "test")
            .env("MULTI", if multi { "1" } else { "0" })
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let port = tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(line) = lines.next_line().await.unwrap() {
                if let Some(address) = line.strip_prefix("[mcp-http] listening on 0.0.0.0:") {
                    return address.split(['/', ' ']).next().unwrap().parse::<u16>().unwrap();
                }
            }
            panic!("TS test process exited before listening");
        })
        .await
        .unwrap();
        Self { child, port }
    }
    async fn kill(&mut self) {
        self.child.kill().await.unwrap();
    }
    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/mcp", self.port)
    }
}
fn args(id: &str) -> Map<String, Value> {
    json!({"id":id}).as_object().unwrap().clone()
}
async fn exercise(multi: bool) {
    let dir = tempfile::tempdir().unwrap();
    let ledger = dir.path().join("ledger");
    let mut server = ChildServer::start(&ledger, 0, multi).await;
    let client = Arc::new(McpClient::connect(open_http(server.url()), ConnectOptions::default()).await.unwrap());
    let cancel = CancellationToken::new();
    for round in 0..3 {
        if !multi {
            let intruder = McpClient::connect(open_http(server.url()), ConnectOptions::default()).await.unwrap();
            intruder.close().await;
        }
        let result = client.call_tool("effect", args(&format!("c{round}")), &cancel).await.unwrap();
        assert_eq!(serde_json::from_str::<Value>(&result).unwrap()["done"], format!("c{round}"));
    }
    let mut waiting = args("interrupted");
    waiting.insert("delayMs".into(), 5000.into());
    let task_cancel = CancellationToken::new();
    let signal = task_cancel.clone();
    let c = client.clone();
    let running = tokio::spawn(async move { c.call_tool("effect", waiting, &signal).await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while !std::fs::read_to_string(&ledger).unwrap_or_default().lines().any(|l| l == "start interrupted") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let port = server.port;
    server.kill().await;
    task_cancel.cancel();
    assert!(running.await.unwrap().is_err());
    server = ChildServer::start(&ledger, port, multi).await;
    let results = futures::future::join_all((0..3).map(|i| {
        let client = client.clone();
        async move { client.call_tool("effect", args(&format!("after-{i}")), &CancellationToken::new()).await }
    }))
    .await;
    for (i, result) in results.into_iter().enumerate() {
        assert_eq!(serde_json::from_str::<Value>(&result.unwrap()).unwrap()["done"], format!("after-{i}"));
    }
    client.close().await;
    server.kill().await;
    let ledger = std::fs::read_to_string(ledger).unwrap();
    let lines: Vec<_> = ledger.lines().collect();
    for id in ["c0", "c1", "c2", "interrupted", "after-0", "after-1", "after-2"] {
        assert_eq!(lines.iter().filter(|l| **l == format!("start {id}")).count(), 1, "{id} starts once");
        assert_eq!(
            lines.iter().filter(|l| **l == format!("done {id}")).count(),
            usize::from(id != "interrupted"),
            "{id} completion"
        );
    }
}
#[tokio::test]
#[ignore = "requires pnpm dependencies: cargo test -p agent --test mcp_typescript -- --ignored"]
async fn rust_client_recovers_from_ts_evictions_and_process_restarts() {
    exercise(false).await;
}
#[tokio::test]
#[ignore = "requires pnpm dependencies: cargo test -p agent --test mcp_typescript -- --ignored"]
async fn rust_client_recovers_from_ts_multi_session_process_restarts() {
    exercise(true).await;
}
