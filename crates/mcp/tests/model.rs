// Differential test: the real memory server against the reference model in
// model/ (Lean). The model is the specification — read model/MemoryModel/
// Model.lean, not this file. This file only generates calls, sends each one
// to both, and compares the answers:
//
//   1. calls      — what a generated call is, and how it reads a document
//   2. sides      — the model process and the server, answering in one shape
//   3. sequential — random call sequences, answer for answer
//   4. concurrent — overlapping calls from several sessions, then a
//                   linearizability check in the model
//   5. main
//
//   cargo test --test model                       (Docker + Lean toolchain)
//   MODEL_CASES=500 cargo test --test model       (more cases; default 100)
//   MODEL_ONLY=sequential|concurrent              (one half)
//
// A failure prints the smallest sequence that still fails (proptest shrinks
// it), with what the model expected and what the server answered.

mod support;

use std::cell::RefCell;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::Instant;

use proptest::prelude::*;
use proptest::test_runner::{Config, TestCaseError, TestError, TestRunner};
use serde_json::{Value, json};
use support::{Client, Outcome, Server, connect, database, start_server};

// ── 1. calls ─────────────────────────────────────────────────────────────────

const DOCS: [&str; 2] = ["a.md", "b.md"];
const PROJECT: &str = "p";
const NO_PATCH: usize = 1_000_000;

// Few distinct characters, so quotes collide, overlap and nearly match
// ("ё"/"е", "—"/"-") as often as possible.
const ALPHABET: [char; 11] = ['a', 'b', 'c', 'x', ' ', '\n', '#', 'ё', 'е', '—', '-'];

// A generated call is abstract: "a quote from the current text", "the
// current version". It is made concrete against the model's view of the
// document just before it is sent, so quotes actually match.
#[derive(Debug, Clone)]
enum Step {
    Write { doc: usize, body: String, version: Version },
    Append { doc: usize, text: String },
    Patch { doc: usize, version: Version, edits: Vec<(Quote, String)> },
    Revert { doc: usize, patch: PatchPick, rollback: bool },
    Read { doc: usize },
    History { doc: usize },
}

#[derive(Debug, Clone)]
enum Version {
    Current,
    Stale,
    Ahead,
    Omitted,
}

#[derive(Debug, Clone)]
enum Quote {
    // A slice of the current text: start and length, wrapped to fit.
    Slice(usize, usize),
    Literal(String),
}

#[derive(Debug, Clone)]
enum PatchPick {
    Newest,
    Index(usize),
    Missing,
}

// The concrete call, as both sides receive it.
#[derive(Debug, Clone)]
enum Call {
    Write { doc: &'static str, body: String, expected: Option<u32> },
    Append { doc: &'static str, text: String },
    Patch { doc: &'static str, expected: u32, edits: Vec<(String, String)> },
    // `index` for the model; `pid` is the same patch's id on the server.
    Revert { doc: &'static str, index: usize, pid: String, rollback: bool },
    Read { doc: &'static str },
    History { doc: &'static str },
}

fn text(max: usize) -> impl Strategy<Value = String> {
    proptest::collection::vec(proptest::sample::select(&ALPHABET[..]), 0..=max).prop_map(String::from_iter)
}

fn step(docs: usize) -> impl Strategy<Value = Step> {
    let doc = 0..docs;
    let version = prop_oneof![6 => Just(Version::Current), 2 => Just(Version::Stale), 1 => Just(Version::Ahead), 1 => Just(Version::Omitted)];
    let quote = prop_oneof![
        4 => (0..64usize, 1..6usize).prop_map(|(at, len)| Quote::Slice(at, len)),
        1 => text(3).prop_map(Quote::Literal),
    ];
    let pick = prop_oneof![3 => Just(PatchPick::Newest), 3 => (0..8usize).prop_map(PatchPick::Index), 1 => Just(PatchPick::Missing)];
    prop_oneof![
        3 => (doc.clone(), text(12), version.clone()).prop_map(|(doc, body, version)| Step::Write { doc, body, version }),
        3 => (doc.clone(), text(6)).prop_map(|(doc, text)| Step::Append { doc, text }),
        5 => (doc.clone(), version, proptest::collection::vec((quote, text(4)), 1..3))
            .prop_map(|(doc, version, edits)| Step::Patch { doc, version, edits }),
        2 => (doc.clone(), pick, proptest::bool::weighted(0.3))
            .prop_map(|(doc, patch, rollback)| Step::Revert { doc, patch, rollback }),
        1 => doc.clone().prop_map(|doc| Step::Read { doc }),
        1 => doc.prop_map(|doc| Step::History { doc }),
    ]
}

impl Call {
    fn model_json(&self) -> Value {
        match self {
            Call::Write { doc, body, expected } => json!({ "op": "write", "doc": doc, "body": body, "expected": expected }),
            Call::Append { doc, text } => json!({ "op": "append", "doc": doc, "text": text }),
            Call::Patch { doc, expected, edits } => json!({
                "op": "patch", "doc": doc, "expected": expected,
                "edits": edits.iter().map(|(old, new)| json!({ "old": old, "new": new })).collect::<Vec<_>>(),
            }),
            Call::Revert { doc, index, rollback, .. } => {
                json!({ "op": "revert", "doc": doc, "index": index, "rollback": rollback })
            }
            Call::Read { doc } => json!({ "op": "read", "doc": doc }),
            Call::History { doc } => json!({ "op": "history", "doc": doc }),
        }
    }

    fn tool(&self) -> (&'static str, Value) {
        match self {
            Call::Write { doc, body, expected } => {
                let mut args = json!({ "project": PROJECT, "doc": doc, "body": body });
                if let Some(v) = expected {
                    args["expected_version"] = json!(v);
                }
                ("write_doc", args)
            }
            Call::Append { doc, text } => ("append_doc", json!({ "project": PROJECT, "doc": doc, "text": text })),
            Call::Patch { doc, expected, edits } => (
                "patch_doc",
                json!({
                    "project": PROJECT, "doc": doc, "expected_version": expected,
                    "edits": edits.iter().map(|(old, new)| json!({ "old": old, "new": new })).collect::<Vec<_>>(),
                }),
            ),
            Call::Revert { doc, pid, rollback, .. } => {
                ("revert_patch", json!({ "project": PROJECT, "doc": doc, "patch_id": pid, "rollback": rollback }))
            }
            Call::Read { doc } => ("read_doc", json!({ "project": PROJECT, "doc": doc })),
            Call::History { doc } => ("doc_history", json!({ "project": PROJECT, "doc": doc, "limit": 100 })),
        }
    }
}

impl std::fmt::Display for Call {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.model_json())
    }
}

// What the model says a document looks like right now.
struct View {
    body: Vec<char>,
    version: u32,
    patches: usize,
}

// ── 2. sides ─────────────────────────────────────────────────────────────────

struct Model {
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Model {
    fn start() -> Self {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("model");
        let lake = which_lake();
        let status = Command::new(&lake).arg("build").current_dir(&dir).status().expect("lake runs");
        assert!(status.success(), "the Lean model does not build (cd crates/mcp/model && lake build)");
        let mut child = Command::new(dir.join(".lake/build/bin/memory-model"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("model starts");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        Model { _child: child, stdin, stdout }
    }

    fn ask(&mut self, request: Value) -> Value {
        writeln!(self.stdin, "{request}").expect("model stdin");
        self.stdin.flush().expect("model stdin");
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("model stdout");
        let answer: Value = serde_json::from_str(&line).expect("model answers JSON");
        assert!(answer.get("protocolError").is_none(), "model rejected {request}: {answer}");
        answer
    }

    fn step(&mut self, call: &Call) -> Value {
        self.ask(json!({ "step": call.model_json() }))
    }

    fn view(&mut self, doc: &'static str) -> View {
        let read = self.step(&Call::Read { doc });
        let history = self.step(&Call::History { doc });
        View {
            body: read["body"].as_str().unwrap_or("").chars().collect(),
            version: read["version"].as_u64().unwrap_or(0) as u32,
            patches: history["history"].as_array().map_or(0, Vec::len),
        }
    }
}

fn which_lake() -> PathBuf {
    if let Ok(path) = std::env::var("LAKE") {
        return path.into();
    }
    let elan = PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".elan/bin/lake");
    if elan.exists() { elan } else { PathBuf::from("lake") }
}

// The server's answer in the model's shape: only what the contract fixes.
// Messages, ids, timestamps and near-match suggestions are left out.
fn normalise(call: &Call, outcome: Outcome) -> Value {
    let value = match outcome {
        Outcome::Ok(value) => value,
        Outcome::Rejected { .. } => return json!({ "rejected": true }),
        // Not a memory answer at all (a database error): never valid.
        Outcome::ToolError(message) => return json!({ "failed": message }),
    };
    if value["ok"] != json!(true) {
        let error = value["error"].as_str().unwrap_or("?");
        let mut out = json!({ "ok": false, "error": error });
        match error {
            // A compare-and-swap lost to a concurrent write: no current version.
            "version_conflict" if value.get("currentVersion").is_none() => out["error"] = json!("raced"),
            "version_conflict" | "version_required" => out["currentVersion"] = value["currentVersion"].clone(),
            "revert_conflict" => out["rollbackToVersion"] = value["rollbackToVersion"].clone(),
            _ => {}
        }
        return out;
    }
    match call {
        Call::Read { .. } => json!({ "ok": true, "body": value["doc"]["body"], "version": value["doc"]["version"] }),
        Call::History { .. } => json!({
            "ok": true,
            "history": value["history"].as_array().into_iter().flatten().map(|p| json!({
                "kind": p["kind"], "versionBefore": p["versionBefore"], "versionAfter": p["versionAfter"],
            })).collect::<Vec<_>>(),
        }),
        _ => json!({ "ok": true, "version": value["version"], "sizeBytes": value["sizeBytes"] }),
    }
}

// Patch ids on the server, oldest first — index i is the model's patch i.
async fn patch_ids(client: &Client, doc: &str) -> Vec<String> {
    let outcome = support::call(client, "doc_history", json!({ "project": PROJECT, "doc": doc, "limit": 100 })).await;
    let Outcome::Ok(value) = outcome else { return Vec::new() };
    let mut ids: Vec<String> =
        value["history"].as_array().into_iter().flatten().filter_map(|p| p["patchId"].as_str().map(str::to_owned)).collect();
    ids.reverse();
    ids
}

async fn concretise(step: &Step, model: &mut Model, client: &Client) -> Call {
    let doc_of = |i: usize| DOCS[i];
    let expected = |version: &Version, view: &View| match version {
        Version::Current => Some(view.version),
        Version::Stale => Some(view.version.saturating_sub(1)),
        Version::Ahead => Some(view.version + 1),
        Version::Omitted => None,
    };
    match step {
        Step::Write { doc, body, version } => {
            let view = model.view(doc_of(*doc));
            Call::Write { doc: doc_of(*doc), body: body.clone(), expected: expected(version, &view) }
        }
        Step::Append { doc, text } => Call::Append { doc: doc_of(*doc), text: text.clone() },
        Step::Patch { doc, version, edits } => {
            let view = model.view(doc_of(*doc));
            let edits = edits
                .iter()
                .map(|(quote, new)| {
                    let old = match quote {
                        Quote::Literal(text) => text.clone(),
                        Quote::Slice(_, _) if view.body.is_empty() => "zz".into(),
                        Quote::Slice(at, len) => {
                            let start = at % view.body.len();
                            let end = (start + len).min(view.body.len());
                            view.body[start..end].iter().collect()
                        }
                    };
                    (old, new.clone())
                })
                .collect();
            Call::Patch { doc: doc_of(*doc), expected: expected(version, &view).unwrap_or(0), edits }
        }
        Step::Revert { doc, patch, rollback } => {
            let view = model.view(doc_of(*doc));
            let index = match patch {
                PatchPick::Newest => view.patches.saturating_sub(1),
                PatchPick::Index(i) => *i,
                PatchPick::Missing => view.patches + 5,
            };
            let ids = patch_ids(client, doc_of(*doc)).await;
            match ids.get(index) {
                Some(pid) => Call::Revert { doc: doc_of(*doc), index, pid: pid.clone(), rollback: *rollback },
                // An id that never exists ("z" is not hex), and an index the
                // model will never reach, even if patches arrive meanwhile.
                None => Call::Revert { doc: doc_of(*doc), index: NO_PATCH, pid: "pa:zzzzzz".into(), rollback: *rollback },
            }
        }
        Step::Read { doc } => Call::Read { doc: doc_of(*doc) },
        Step::History { doc } => Call::History { doc: doc_of(*doc) },
    }
}

async fn on_server(client: &Client, call: &Call) -> Value {
    let (tool, args) = call.tool();
    normalise(call, support::call(client, tool, args).await)
}

async fn fresh_start(server: &Server, model: &mut Model, client: &Client) {
    server
        .pool
        .get()
        .await
        .expect("pool")
        .batch_execute(
            "TRUNCATE memory_projects, memory_project_docs, memory_doc_patches, memory_index RESTART IDENTITY CASCADE",
        )
        .await
        .expect("clean memory");
    let created = support::call(client, "create_project", json!({ "slug": PROJECT, "title": "Model" })).await;
    assert!(matches!(created, Outcome::Ok(_)), "project created: {created:?}");
    model.ask(json!({ "reset": true }));
}

// ── 3. sequential ────────────────────────────────────────────────────────────

// Runs the steps on both sides; the transcript of every call so far is the
// failure report.
async fn sequential(steps: &[Step], model: &mut Model, client: &Client) -> Result<Vec<String>, String> {
    let mut transcript = Vec::new();
    for step in steps {
        let call = concretise(step, model, client).await;
        let expected = model.step(&call);
        let actual = on_server(client, &call).await;
        transcript.push(format!("  {call}\n      → {actual}"));
        if expected != actual {
            return Err(format!("{}\n  model expected: {expected}", transcript.join("\n")));
        }
    }
    Ok(transcript)
}

// ── 4. concurrent ────────────────────────────────────────────────────────────

struct Event {
    session: usize,
    call: Call,
    answer: Value,
    start: u128,
    end: u128,
}

// The steps after `prefix` run in several sessions at once, all against one
// document. Every session's calls are made concrete against the same
// snapshot, so they collide: two patches of the same version, a revert racing
// a patch.
async fn concurrent(
    prefix: &[Step],
    sessions: &[Vec<Step>],
    model: &mut Model,
    client: &Client,
    url: &str,
) -> Result<(), String> {
    let transcript = sequential(prefix, model, client).await?;
    let mut planned = Vec::new();
    for steps in sessions {
        let mut calls = Vec::new();
        for step in steps {
            calls.push(concretise(step, model, client).await);
        }
        planned.push(calls);
    }

    let clients = futures::future::join_all(planned.iter().map(|_| connect(url))).await;
    let t0 = Instant::now();
    let runs = planned.iter().zip(&clients).enumerate().map(|(session, (calls, client))| async move {
        let mut events = Vec::new();
        for call in calls {
            let start = t0.elapsed().as_micros();
            let answer = on_server(client, call).await;
            let end = t0.elapsed().as_micros();
            events.push(Event { session, call: call.clone(), answer, start, end });
        }
        events
    });
    let mut events: Vec<Event> = futures::future::join_all(runs).await.into_iter().flatten().collect();
    // Afterwards, one look at each document: the final state must be one the
    // linearization reaches.
    for doc in DOCS {
        for call in [Call::Read { doc }, Call::History { doc }] {
            let start = t0.elapsed().as_micros();
            let answer = on_server(client, &call).await;
            events.push(Event { session: usize::MAX, call, answer, start, end: t0.elapsed().as_micros() });
        }
    }
    for c in clients {
        let _ = c.cancel().await;
    }

    let history: Vec<Value> = events
        .iter()
        .map(|e| json!({ "call": e.call.model_json(), "answer": e.answer, "start": e.start as u64, "end": e.end as u64 }))
        .collect();
    let verdict = model.ask(json!({ "linearizable": history }));
    if verdict["linearizable"] == json!(true) {
        return Ok(());
    }
    events.sort_by_key(|e| e.start);
    let lines: Vec<String> = events
        .iter()
        .map(|e| {
            let who = if e.session == usize::MAX { "after".into() } else { format!("s{}", e.session) };
            format!("  {who:>5} [{:>7}–{:>7}µs] {}\n          → {}", e.start, e.end, e.call, e.answer)
        })
        .collect();
    Err(format!(
        "not linearizable — no order of these calls makes the model give these answers.\nprefix:\n{}\nconcurrent:\n{}",
        transcript.join("\n"),
        lines.join("\n")
    ))
}

// ── 5. main ──────────────────────────────────────────────────────────────────

fn cases() -> u32 {
    std::env::var("MODEL_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(100)
}

fn report<T: std::fmt::Debug>(name: &str, result: Result<(), TestError<T>>) -> bool {
    match result {
        Ok(()) => {
            println!("{name}: {} cases passed", cases());
            true
        }
        Err(TestError::Fail(reason, minimal)) => {
            println!("{name}: FAILED\n{reason}\n\nminimal input: {minimal:#?}");
            false
        }
        Err(TestError::Abort(reason)) => {
            println!("{name}: aborted: {reason}");
            false
        }
    }
}

fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("runtime");
    let (url, container) = rt.block_on(database());
    let server = rt.block_on(start_server(&url, Some("memory"))).expect("server starts");
    let client = rt.block_on(connect(&server.url));
    let model = RefCell::new(Model::start());
    let config = Config { cases: cases(), failure_persistence: None, ..Config::default() };

    let only = std::env::var("MODEL_ONLY").unwrap_or_default();
    let sequential_ok = only == "concurrent" || report(
        "sequential",
        TestRunner::new(config.clone()).run(&proptest::collection::vec(step(DOCS.len()), 1..30), |steps| {
            let model = &mut model.borrow_mut();
            rt.block_on(async {
                fresh_start(&server, model, &client).await;
                sequential(&steps, model, &client).await.map(|_| ()).map_err(TestCaseError::fail)
            })
        }),
    );

    let sessions = proptest::collection::vec(proptest::collection::vec(step(1), 1..4), 2..5);
    let concurrent_ok = only == "sequential" || report(
        "concurrent",
        TestRunner::new(config).run(&(proptest::collection::vec(step(1), 0..6), sessions), |(prefix, sessions)| {
            let model = &mut model.borrow_mut();
            rt.block_on(async {
                fresh_start(&server, model, &client).await;
                concurrent(&prefix, &sessions, model, &client, &server.url).await.map_err(TestCaseError::fail)
            })
        }),
    );

    rt.block_on(async { drop(container) });
    if !(sequential_ok && concurrent_ok) {
        std::process::exit(1);
    }
}
