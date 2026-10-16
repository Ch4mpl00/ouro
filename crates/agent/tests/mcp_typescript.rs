// Cross-language contract against packages/mcp's HTTP test process: the same
// fixture used by TS test/mcp-reconnect.test.ts, without importing server code.
// Requires pnpm dependencies, so run explicitly with --ignored.
use agent::mcp::{ConnectOptions, Mcp, McpClient, open_http};
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
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

// ── Port of test/mcp-reconnect.test.ts ──────────────────────────────────────
//
// The client against the real Streamable HTTP transport, served by a process
// that dies the way a container does — SIGKILL — and comes back on the same
// port, or whose single session another client takes over. Rules, from the
// ledger alone:
//   - nothing runs twice: a call the tool began is never replayed, even when
//     the server dies under it;
//   - a call that returned a result ran exactly once, to completion;
//   - a call made while the server is up and stays up succeeds, however many
//     restarts and evictions came before;
//   - a call made while the server is down fails, and starts nothing;
//   - concurrent callers that find the session gone open one session between
//     them, not one each.

#[derive(Clone, Debug)]
enum Step {
    Calls(usize),
    Restart,
    KillMidCall,
    CallWhileDown,
    Evict,
}

fn step() -> impl proptest::strategy::Strategy<Value = Step> {
    use proptest::prelude::*;
    prop_oneof![
        4 => (1usize..=3).prop_map(Step::Calls),
        2 => Just(Step::Restart),
        2 => Just(Step::KillMidCall),
        1 => Just(Step::CallWhileDown),
        1 => Just(Step::Evict),
    ]
}

fn ledger_lines(ledger: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(ledger).unwrap_or_default().lines().map(str::to_owned).collect()
}

async fn until(what: &str, mut check: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while !check() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

struct Generations {
    ledger: std::path::PathBuf,
    port: u16,
    multi: bool,
    generation: u32,
    child: Option<tokio::process::Child>,
}

impl Generations {
    async fn start(&mut self) {
        self.generation += 1;
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut child = tokio::process::Command::new("node")
            .args(["--import", "tsx", "packages/mcp/test/support/mcp-child.ts"])
            .current_dir(root)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", std::env::var("HOME").unwrap_or_default())
            .env("PORT", self.port.to_string())
            .env("LEDGER", &self.ledger)
            .env("GEN", self.generation.to_string())
            .env("MULTI", if self.multi { "1" } else { "0" })
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        tokio::time::timeout(Duration::from_secs(20), async {
            while let Some(line) = lines.next_line().await.unwrap() {
                if line.contains("[mcp-http] listening") {
                    return;
                }
            }
            panic!("MCP generation exited before listening");
        })
        .await
        .unwrap();
        // Keep draining stdout so the child never blocks on a full pipe.
        tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
        self.child = Some(child);
    }

    async fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill().await;
        }
    }
}

struct Outcome {
    ok: bool,
    up: bool,
    text: String,
}

async fn effect(client: &Arc<McpClient>, n: &mut u32, calls: &mut Vec<(String, Outcome)>, up: bool) -> String {
    *n += 1;
    let id = format!("c{n}");
    let outcome = match client.call_tool("effect", args(&id), &CancellationToken::new()).await {
        Ok(text) => Outcome { ok: !text.starts_with("[tool error]"), up, text },
        Err(err) => Outcome { ok: false, up, text: format!("{err:#}") },
    };
    calls.push((id.clone(), outcome));
    id
}

async fn run_history(steps: Vec<Step>, multi: bool, dir: &std::path::Path, witnessed: &Mutex<HashSet<&'static str>>) {
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let ledger = dir.join(format!("ledger-{port}-{}", std::process::id()));
    let url = format!("http://127.0.0.1:{port}/mcp");
    let mut server = Generations { ledger: ledger.clone(), port, multi, generation: 0, child: None };
    server.start().await;
    let client = Arc::new(McpClient::connect(open_http(url.clone()), ConnectOptions::default()).await.unwrap());
    let mut n = 0;
    let mut calls: Vec<(String, Outcome)> = Vec::new();
    let witness = |w: &'static str| {
        witnessed.lock().unwrap().insert(w);
    };
    for s in &steps {
        match s {
            Step::Calls(parallel) => {
                let ids: Vec<String> = (0..*parallel)
                    .map(|_| {
                        n += 1;
                        format!("c{n}")
                    })
                    .collect();
                let results = futures::future::join_all(ids.iter().map(|id| {
                    let client = client.clone();
                    async move { client.call_tool("effect", args(id), &CancellationToken::new()).await }
                }))
                .await;
                for (id, result) in ids.into_iter().zip(results) {
                    let outcome = match result {
                        Ok(text) => Outcome { ok: !text.starts_with("[tool error]"), up: true, text },
                        Err(err) => Outcome { ok: false, up: true, text: format!("{err:#}") },
                    };
                    calls.push((id, outcome));
                }
            }
            Step::Restart => {
                server.kill().await;
                server.start().await;
                witness("restart");
            }
            Step::KillMidCall => {
                n += 1;
                let id = format!("c{n}");
                // A call whose server dies under it is only failed by the
                // request timeout; cancel it sooner, as the supervisor would.
                let cancel = CancellationToken::new();
                let deadline = cancel.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    deadline.cancel();
                });
                let mut a = args(&id);
                a.insert("delayMs".into(), 5000.into());
                let running = {
                    let client = client.clone();
                    tokio::spawn(async move { client.call_tool("effect", a, &cancel).await })
                };
                let start_line = format!("start {id}");
                until(&format!("call {id} to start"), || ledger_lines(&ledger).contains(&start_line)).await;
                server.kill().await;
                let outcome = match running.await.unwrap() {
                    Ok(text) => Outcome { ok: !text.starts_with("[tool error]"), up: false, text },
                    Err(err) => Outcome { ok: false, up: false, text: format!("{err:#}") },
                };
                calls.push((id, outcome));
                server.start().await;
                witness("killed mid-call");
            }
            Step::CallWhileDown => {
                server.kill().await;
                effect(&client, &mut n, &mut calls, false).await;
                server.start().await;
                witness("a call while down");
            }
            Step::Evict => {
                if !multi {
                    let intruder = McpClient::connect(open_http(url.clone()), ConnectOptions::default()).await.unwrap();
                    intruder.close().await;
                    witness("evicted");
                }
            }
        }
    }
    // After everything, MCP is up: calls work again, several at once.
    let (a, b) = {
        n += 2;
        (format!("c{}", n - 1), format!("c{n}"))
    };
    let results = futures::future::join_all([a.clone(), b.clone()].map(|id| {
        let client = client.clone();
        async move { client.call_tool("effect", args(&id), &CancellationToken::new()).await }
    }))
    .await;
    for (id, result) in [a, b].into_iter().zip(results) {
        let outcome = match result {
            Ok(text) => Outcome { ok: !text.starts_with("[tool error]"), up: true, text },
            Err(err) => Outcome { ok: false, up: true, text: format!("{err:#}") },
        };
        calls.push((id, outcome));
    }
    client.close().await;
    server.kill().await;

    let log = ledger_lines(&ledger);
    let count = |line: String| log.iter().filter(|l| **l == line).count();
    let at = format!("{steps:?} multi={multi}");
    for (id, c) in &calls {
        assert!(count(format!("start {id}")) <= 1, "{at}: {id} ran at most once ({})", c.text);
        if c.ok {
            assert_eq!(count(format!("done {id}")), 1, "{at}: {id} returned, so it ran to completion once");
        }
        if c.up {
            assert!(c.ok, "{at}: {id} was made while MCP was up: {}", c.text);
        }
        if !c.up && !c.ok && count(format!("start {id}")) == 0 {
            witness("a call failed and started nothing");
        }
    }
    if multi {
        // One session per generation the client used: a reconnect shared by
        // concurrent callers, not one session each.
        for g in 1..=server.generation {
            assert!(count(format!("session {g}")) <= 1, "{at}: sessions opened on generation {g}");
        }
    }
}

#[test]
#[ignore = "requires pnpm dependencies: cargo test -p agent --test mcp_typescript -- --ignored"]
fn never_runs_a_call_twice_and_serves_every_call_made_while_mcp_is_up() {
    use proptest::test_runner::{Config, TestRunner};
    let runs = std::env::var("PROPERTY_RUNS").ok().and_then(|v| v.parse().ok()).unwrap_or(10);
    let dir = tempfile::tempdir().unwrap();
    let witnessed = Mutex::new(HashSet::new());
    let mut runner = TestRunner::new(Config { cases: runs, failure_persistence: None, ..Config::default() });
    let strategy = (proptest::collection::vec(step(), 1..=6), proptest::bool::ANY);
    runner
        .run(&strategy, |(steps, multi)| {
            let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
            rt.block_on(run_history(steps, multi, dir.path(), &witnessed));
            Ok(())
        })
        .unwrap();
    let witnessed = witnessed.into_inner().unwrap();
    for w in ["restart", "killed mid-call", "a call while down", "a call failed and started nothing"] {
        assert!(witnessed.contains(w), "{w}");
    }
}
