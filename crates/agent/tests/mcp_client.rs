// Port of src/mcp-client.test.ts and reconnect contract over real HTTP.
// This crate never imports MCP server internals: the SDK fixture speaks only
// the public protocol, including cancellation and a lost response after a tool.
use agent::mcp::*;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService,
    session::{SessionManager, local::LocalSessionManager},
};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::{Map, Value, json};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn recording_sleep() -> (Sleep, Arc<Mutex<Vec<u64>>>) {
    let delays = Arc::new(Mutex::new(Vec::new()));
    let log = delays.clone();
    (
        Arc::new(move |delay| {
            log.lock().unwrap().push(delay);
            Box::pin(async {})
        }),
        delays,
    )
}
#[tokio::test]
async fn connect_retry_policies_and_fatal_errors() {
    for (policy, failures, expected) in [
        (CONNECT_RETRY, 2, vec![500, 1000]),
        (RETRY_UNTIL_UP, 11, vec![1000, 2000, 4000, 8000, 16000, 30000, 30000, 30000, 30000, 30000, 30000]),
        (ConnectRetryPolicy { max_retries: Some(3), base_delay_ms: 10, max_delay_ms: 100 }, 4, vec![10, 20, 40]),
    ] {
        let (sleep, delays) = recording_sleep();
        let mut calls = 0;
        let mut seen = Vec::new();
        let result = connect_with_retry(
            || {
                calls += 1;
                let n = calls;
                async move {
                    if n <= failures {
                        anyhow::bail!("HTTP 500 Already connected to a transport")
                    }
                    Ok("connected")
                }
            },
            policy,
            &sleep,
            |r| seen.push(r.attempt),
        )
        .await;
        assert_eq!(*delays.lock().unwrap(), expected);
        assert_eq!(seen.len(), expected.len());
        assert_eq!(result.is_ok(), failures < 4 || policy.max_retries.is_none());
    }
    let (sleep, delays) = recording_sleep();
    let mut calls = 0;
    connect_with_retry(
        || {
            calls += 1;
            let n = calls;
            async move {
                if n < 250 {
                    anyhow::bail!("connect refused")
                }
                Ok(())
            }
        },
        RETRY_UNTIL_UP,
        &sleep,
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(calls, 250);
    assert_eq!(delays.lock().unwrap().len(), 249);
    for status in [400, 401, 403, 404] {
        assert!(is_fatal_connect_error(&anyhow::anyhow!("HTTP {status}: rejected")));
    }
    for status in [408, 429, 500, 503] {
        assert!(!is_fatal_connect_error(&anyhow::anyhow!("HTTP {status}: offline")));
    }
    assert!(is_fatal_connect_error(&anyhow::anyhow!("Auth required")));
    let (sleep, delays) = recording_sleep();
    assert!(
        connect_with_retry(
            || async { Err::<(), _>(anyhow::anyhow!("HTTP 404 not found")) },
            RETRY_UNTIL_UP,
            &sleep,
            |_| {}
        )
        .await
        .is_err()
    );
    assert!(delays.lock().unwrap().is_empty());
}
#[derive(Default)]
struct State {
    ledger: Mutex<Vec<String>>,
    began: tokio::sync::Notify,
    cancelled: AtomicUsize,
    lose_reply: AtomicBool,
    offline: AtomicBool,
    startup_failures: AtomicUsize,
}
#[derive(Clone)]
struct Probe(Arc<State>);
impl ServerHandler for Probe {
    fn get_info(&self) -> ServerConfig {
        let mut info = ServerConfig::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info
    }
    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(vec![Tool::new(
            "effect",
            "records a side effect",
            Arc::new(json!({"type":"object"}).as_object().unwrap().clone()),
        )]))
    }
    async fn call_tool(
        &self,
        p: CallToolRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let args = p.arguments.unwrap_or_default();
        let id = args.get("id").and_then(Value::as_str).unwrap_or("unknown");
        self.0.ledger.lock().unwrap().push(format!("start {id}"));
        self.0.began.notify_one();
        if args.get("wait") == Some(&Value::Bool(true)) {
            ctx.ct.cancelled().await;
            self.0.cancelled.fetch_add(1, Ordering::SeqCst);
            return Ok(CallToolResult::success(Vec::new()).into());
        }
        self.0.ledger.lock().unwrap().push(format!("done {id}"));
        if args.get("error") == Some(&Value::Bool(true)) {
            return Ok(CallToolResult::error(vec![ContentBlock::text("bad action")]).into());
        }
        Ok(CallToolResult::success(vec![ContentBlock::text("ok")]).into())
    }
}
struct Server {
    url: String,
    state: Arc<State>,
    manager: Arc<LocalSessionManager>,
    task: tokio::task::JoinHandle<()>,
    stop: CancellationToken,
}
impl Server {
    async fn new() -> Self {
        let state = Arc::new(State::default());
        let manager = Arc::new(LocalSessionManager::default());
        let stop = CancellationToken::new();
        let probe = Probe(state.clone());
        let mut config = StreamableHttpServerConfig::default().disable_allowed_hosts();
        config.cancellation_token = stop.clone();
        let service = StreamableHttpService::new(move || Ok(probe.clone()), manager.clone(), config);
        let filter_state = state.clone();
        let app = axum::Router::new().nest_service("/mcp", service).layer(axum::middleware::from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                let state = filter_state.clone();
                async move {
                    let startup = state
                        .startup_failures
                        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                        .is_ok();
                    if startup || state.offline.load(Ordering::SeqCst) {
                        return axum::response::Response::builder()
                            .status(503)
                            .body(axum::body::Body::from("offline"))
                            .unwrap();
                    }
                    let is_post = req.method() == axum::http::Method::POST;
                    let response = next.run(req).await;
                    if is_post && state.lose_reply.swap(false, Ordering::SeqCst) {
                        // The action completed, but its reply got lost. This text
                        // must not be mistaken for a definitive session rejection.
                        return axum::response::Response::builder()
                            .status(503)
                            .body(axum::body::Body::from("Session not found upstream after effect"))
                            .unwrap();
                    }
                    response
                }
            },
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { url: format!("http://{addr}/mcp"), state, manager, task, stop }
    }
    async fn evict(&self) {
        let ids: Vec<_> = self.manager.sessions.read().await.keys().cloned().collect();
        for id in ids {
            self.manager.close_session(&id).await.unwrap();
        }
    }
    async fn connect(&self) -> (Arc<McpClient>, Arc<AtomicUsize>) {
        let open_http = open_http(self.url.clone());
        let connections = Arc::new(AtomicUsize::new(0));
        let count = connections.clone();
        let open: Open = Arc::new(move || {
            count.fetch_add(1, Ordering::SeqCst);
            open_http()
        });
        let (sleep, _) = recording_sleep();
        (
            Arc::new(
                McpClient::connect(
                    open,
                    ConnectOptions {
                        startup_retry: ConnectRetryPolicy { base_delay_ms: 0, max_delay_ms: 0, ..CONNECT_RETRY },
                        sleep,
                    },
                )
                .await
                .unwrap(),
            ),
            connections,
        )
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.cancel();
        self.task.abort();
    }
}
fn args(id: &str) -> Map<String, Value> {
    json!({"id":id}).as_object().unwrap().clone()
}
#[tokio::test]
async fn real_http_handshake_survives_startup_503_and_yields_tool_errors_as_text() {
    let server = Server::new().await;
    server.state.startup_failures.store(3, Ordering::SeqCst);
    let (client, connections) = server.connect().await;
    assert!(connections.load(Ordering::SeqCst) > 1);
    assert_eq!(client.tools()[0].name, "effect");
    assert_eq!(client.call_tool("effect", args("one"), &CancellationToken::new()).await.unwrap(), "ok");
    let mut a = args("error");
    a.insert("error".into(), true.into());
    assert_eq!(client.call_tool("effect", a, &CancellationToken::new()).await.unwrap(), "[tool error] bad action");
    client.close().await;
    assert!(client.call_tool("effect", args("closed"), &CancellationToken::new()).await.is_err());
}
#[tokio::test]
async fn session_evictions_share_one_reconnect_and_never_duplicate_effects() {
    let server = Server::new().await;
    let (client, connections) = server.connect().await;
    for round in 0..4 {
        server.evict().await;
        let calls = (0..3).map(|i| {
            let client = client.clone();
            async move { client.call_tool("effect", args(&format!("{round}-{i}")), &CancellationToken::new()).await }
        });
        for result in futures::future::join_all(calls).await {
            assert_eq!(result.unwrap(), "ok");
        }
        assert_eq!(connections.load(Ordering::SeqCst), round + 2);
    }
    let ledger = server.state.ledger.lock().unwrap().clone();
    assert_eq!(ledger.len(), 24);
    assert_eq!(ledger.iter().collect::<std::collections::HashSet<_>>().len(), 24);
    client.close().await;
}
#[tokio::test]
async fn forwards_cancellation_to_server_without_reconnecting() {
    let server = Server::new().await;
    let (client, connections) = server.connect().await;
    let cancel = CancellationToken::new();
    let c = client.clone();
    let signal = cancel.clone();
    let mut a = args("wait");
    a.insert("wait".into(), true.into());
    let task = tokio::spawn(async move { c.call_tool("effect", a, &signal).await });
    tokio::time::timeout(Duration::from_secs(2), server.state.began.notified()).await.unwrap();
    cancel.cancel();
    assert!(task.await.unwrap().is_err());
    tokio::time::timeout(Duration::from_secs(2), async {
        while server.state.cancelled.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(connections.load(Ordering::SeqCst), 1);
    client.close().await;
}
#[tokio::test]
async fn a_lost_reply_after_a_side_effect_is_never_replayed() {
    let server = Server::new().await;
    let (client, connections) = server.connect().await;
    server.state.lose_reply.store(true, Ordering::SeqCst);
    assert!(client.call_tool("effect", args("once"), &CancellationToken::new()).await.is_err());
    assert_eq!(*server.state.ledger.lock().unwrap(), ["start once", "done once"]);
    assert_eq!(connections.load(Ordering::SeqCst), 1);
    assert_eq!(client.call_tool("effect", args("next"), &CancellationToken::new()).await.unwrap(), "ok");
    client.close().await;
}
#[tokio::test]
async fn wrong_http_path_fails_without_retrying() {
    let server = Server::new().await;
    let open = open_http(server.url.replace("/mcp", "/wrong"));
    let (sleep, delays) = recording_sleep();
    let result = McpClient::connect(open, ConnectOptions { startup_retry: RETRY_UNTIL_UP, sleep }).await;
    assert!(result.is_err());
    assert!(delays.lock().unwrap().is_empty());
}
