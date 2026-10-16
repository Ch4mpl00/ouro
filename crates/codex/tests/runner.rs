// The Codex service around a stand-in `codex` binary put first on PATH: the
// real spawn, stdin, temp files, exit codes and signals, with the CLI's
// behaviour scripted by the prompt (answer, fail, or hang ignoring SIGTERM
// with a child of its own). And the HTTP front: what a caller may ask for.
//
// The stand-in is this test binary itself, reached through a `codex` symlink:
// run under that name it plays the CLI. The tests run in a re-exec of the
// binary with the symlink's directory first on PATH, so the service finds it
// the way it finds the real one. No libtest harness: main is ours.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use codex_service::runner::{RunRequest, Sandbox, run_codex};
use codex_service::server::{ServerOptions, router};
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};
use serde_json::{Map, Value, json};

const FAKE_PATH_SET: &str = "CODEX_TEST_FAKE_ON_PATH";

type Test = fn(&tokio::runtime::Runtime, &Path);

fn main() {
    let exe = std::env::current_exe().unwrap();
    let argv0 = std::env::args().next().unwrap_or_default();
    if Path::new(&argv0).file_name().is_some_and(|n| n == "codex") {
        fake_codex();
        return;
    }
    if std::env::var_os(FAKE_PATH_SET).is_none() {
        let bin = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(&exe, bin.path().join("codex")).unwrap();
        let path = std::env::join_paths(
            std::iter::once(bin.path().to_path_buf())
                .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())),
        )
        .unwrap();
        let status =
            std::process::Command::new(&exe).env("PATH", path).env(FAKE_PATH_SET, bin.path()).status().unwrap();
        std::process::exit(status.code().unwrap_or(1));
    }
    let bin = PathBuf::from(std::env::var_os(FAKE_PATH_SET).unwrap());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let tests: [(&str, Test); 5] = [
        ("passes the request through to the CLI and reads its answer back", passes_the_request_through),
        ("reports a failed run with the CLI's own error", reports_a_failed_run),
        ("ends a run past its deadline, children included", ends_a_run_past_its_deadline),
        ("answers health and 404s anything else", answers_health_and_404s),
        ("runs a well-formed request and refuses malformed ones", runs_and_refuses),
    ];
    for (name, test) in tests {
        println!("test {name} ...");
        test(&rt, &bin);
        println!("test {name} ... ok");
    }
}

// ── the stand-in CLI ────────────────────────────────────────────────────────

fn fake_codex() {
    use std::io::{Read, Write};
    let args: Vec<String> = std::env::args().skip(1).collect();
    let prompt = args.last().cloned().unwrap_or_default();
    let mut stdin = String::new();
    let _ = std::io::stdin().read_to_string(&mut stdin);
    if prompt.contains("MODE:hang") {
        let pid_file = prompt.split("PIDFILE:").nth(1).and_then(|s| s.split_whitespace().next()).unwrap().to_owned();
        // Left running on purpose: the service must end it.
        #[allow(clippy::zombie_processes)]
        let child = std::process::Command::new("sleep")
            .arg("60")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        std::fs::write(&pid_file, child.id().to_string()).unwrap();
        // A handler for SIGTERM replaces its default action: ignored.
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
            loop {
                term.recv().await;
            }
        });
    }
    let mut out = std::io::stdout();
    if prompt.contains("MODE:fail") {
        writeln!(out, "{}", json!({ "type": "error", "message": "model exploded" })).unwrap();
        std::process::exit(2);
    }
    let i = args.iter().position(|a| a == "--output-schema");
    let schema_path = i.map(|i| args[i + 1].clone());
    let schema = schema_path.as_ref().map(|p| std::fs::read_to_string(p).unwrap());
    let echo = json!({ "args": args, "stdin": stdin, "schema": schema, "schemaPath": schema_path }).to_string();
    writeln!(out, "{}", json!({ "type": "thread.started", "thread_id": "th-1" })).unwrap();
    writeln!(out, "a line that is not JSON").unwrap();
    writeln!(out, "{}", json!({ "type": "item.completed", "item": { "type": "agent_message", "text": echo } }))
        .unwrap();
    writeln!(out, "{}", json!({ "type": "turn.completed", "usage": { "input_tokens": 1 } })).unwrap();
}

// ── the runner ──────────────────────────────────────────────────────────────

fn json_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::from),
        any::<i32>().prop_map(Value::from),
        "\\PC{0,8}".prop_map(Value::from),
    ];
    leaf.prop_recursive(2, 12, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
            prop::collection::btree_map("\\PC{0,6}", inner, 0..4).prop_map(|m| Value::Object(m.into_iter().collect())),
        ]
    })
}

fn request() -> impl Strategy<Value = RunRequest> {
    (
        "\\PC{1,40}",
        proptest::option::of(prop_oneof!["\\PC{0,30}".prop_map(Value::from), json_value()]),
        proptest::option::of(json_value()),
        proptest::option::of(prop_oneof![Just(Sandbox::ReadOnly), Just(Sandbox::WorkspaceWrite)]),
        proptest::option::of(prop_oneof![Just("gpt-5.4".to_owned()), Just("o4-mini".to_owned())]),
    )
        .prop_map(|(prompt, input, schema, sandbox, model)| RunRequest {
            prompt: format!("task: {prompt}"),
            input,
            schema,
            sandbox,
            model,
            ..RunRequest::default()
        })
}

fn passes_the_request_through(rt: &tokio::runtime::Runtime, _: &Path) {
    let mut runner = TestRunner::new(Config { cases: 25, failure_persistence: None, ..Config::default() });
    runner
        .run(&request(), |req| {
            let result = rt.block_on(run_codex(&req)).unwrap();
            assert_eq!(result["ok"], true, "run failed: {result}");
            let echo: Value = serde_json::from_str(result["content"].as_str().unwrap()).unwrap();
            let args: Vec<String> = serde_json::from_value(echo["args"].clone()).unwrap();
            assert_eq!(args.last(), Some(&req.prompt), "the prompt is the last argument, verbatim");
            let flag = |name: &str| args.iter().position(|a| a == name).map(|i| args[i + 1].clone());
            assert_eq!(
                flag("--sandbox").as_deref(),
                Some(req.sandbox.unwrap_or(Sandbox::ReadOnly).as_str()),
                "sandbox"
            );
            assert_eq!(flag("--model"), req.model, "model only when asked");
            assert!(args.iter().any(|a| a == "--ignore-user-config"), "never the user's codex config");
            let want_stdin = match &req.input {
                None => String::new(),
                Some(Value::String(s)) => s.clone(),
                Some(v) => serde_json::to_string_pretty(v).unwrap(),
            };
            assert_eq!(echo["stdin"], want_stdin, "input arrives on stdin");
            match &req.schema {
                None => assert!(echo["schemaPath"].is_null(), "no schema file without a schema"),
                Some(schema) => {
                    let written: Value = serde_json::from_str(echo["schema"].as_str().unwrap()).unwrap();
                    assert_eq!(&written, schema, "the schema file holds the schema");
                    assert!(
                        !Path::new(echo["schemaPath"].as_str().unwrap()).exists(),
                        "the schema file is gone afterwards"
                    );
                }
            }
            assert_eq!(result["threadId"], "th-1");
            assert_eq!(result["usage"], json!({ "input_tokens": 1 }));
            assert_eq!(result["parsed"], echo, "JSON content comes back parsed");
            Ok(())
        })
        .unwrap();
}

fn reports_a_failed_run(rt: &tokio::runtime::Runtime, _: &Path) {
    let result = rt.block_on(run_codex(&RunRequest { prompt: "MODE:fail".into(), ..RunRequest::default() })).unwrap();
    assert_eq!(result["ok"], false);
    assert_eq!(result["code"], 2);
    assert!(result["error"].as_str().unwrap().contains("model exploded"), "{result}");
}

// A run past its deadline ends — even when the CLI ignores SIGTERM — and
// takes the processes it started with it.
fn ends_a_run_past_its_deadline(rt: &tokio::runtime::Runtime, bin: &Path) {
    let pid_file = bin.join(format!("pid-{}", std::process::id()));
    let started = Instant::now();
    let req = RunRequest {
        prompt: format!("MODE:hang PIDFILE:{}", pid_file.display()),
        timeout_ms: Some(500.0),
        ..RunRequest::default()
    };
    let result = rt.block_on(run_codex(&req)).unwrap();
    assert!(started.elapsed() < Duration::from_secs(5), "returns soon after the deadline");
    assert_eq!(result["ok"], false);
    assert!(result["error"].as_str().unwrap().contains("timed out"), "{result}");
    let grandchild: i32 = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    let alive = nix::sys::signal::kill(nix::unistd::Pid::from_raw(grandchild), None).is_ok();
    assert!(!alive, "the CLI's own child is gone too");
}

// ── the HTTP front ──────────────────────────────────────────────────────────

fn serve(rt: &tokio::runtime::Runtime) -> String {
    rt.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router(ServerOptions { max_body_bytes: 4096 })).await });
        base
    })
}

fn answers_health_and_404s(rt: &tokio::runtime::Runtime, _: &Path) {
    let base = serve(rt);
    rt.block_on(async {
        assert_eq!(reqwest::get(format!("{base}/health")).await.unwrap().status(), 200);
        assert_eq!(reqwest::get(format!("{base}/elsewhere")).await.unwrap().status(), 404);
    });
}

fn runs_and_refuses(rt: &tokio::runtime::Runtime, _: &Path) {
    let base = serve(rt);
    let post = |body: String| {
        let url = format!("{base}/run");
        rt.block_on(async move {
            reqwest::Client::new()
                .post(url)
                .header("content-type", "application/json")
                .body(body)
                .send()
                .await
                .unwrap()
                .status()
                .as_u16()
        })
    };
    assert_eq!(
        post(json!({ "prompt": "hello", "sandbox": "workspace-write" }).to_string()),
        200,
        "runs a well-formed request"
    );
    let mut long = Map::new();
    long.insert("prompt".into(), "x".repeat(10_000).into());
    let refused = [
        ("no prompt", json!({}).to_string()),
        ("empty prompt", json!({ "prompt": "" }).to_string()),
        ("not JSON", "{oops".to_owned()),
        ("an array", "[]".to_owned()),
        ("full access asked for", json!({ "prompt": "x", "sandbox": "danger-full-access" }).to_string()),
        ("an unknown sandbox", json!({ "prompt": "x", "sandbox": "yolo" }).to_string()),
        ("an unknown approval policy", json!({ "prompt": "x", "approvalPolicy": "always" }).to_string()),
        ("a timeout that is not a number", json!({ "prompt": "x", "timeoutMs": "soon" }).to_string()),
        ("a body over the limit", Value::Object(long).to_string()),
    ];
    for (label, body) in refused {
        let status = post(body);
        assert!((400..500).contains(&status), "refuses {label}: got {status}");
    }
}
