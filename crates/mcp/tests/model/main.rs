// Differential test: the real memory server against the reference model. A
// port of packages/mcp/model/server.check.ts; the model (model.rs) is the
// specification — read that, not this file. This file only generates calls,
// sends each one to both, and compares the answers:
//
//   1. environment — Postgres + the real server over MCP (tests/support),
//                    a restricted multi-session instance like mcp-tunnel
//   2. sequential  — random call sequences, answer for answer
//   3. concurrent  — overlapping calls from several sessions, then a
//                    linearizability check against the model
//
//   cargo test --test model                       (Docker, or TEST_DATABASE_URL)
//   MODEL_CASES=500 cargo test --test model       (more cases; default 200)
//
// properties.rs checks the model itself (default 1000 cases). A failure
// prints the smallest input that still fails (proptest shrinks it) and the
// transcript: every call, the server's answer, and what the model expected.

#[path = "../support/mod.rs"]
mod support;

mod answers;
mod calls;
mod linearizable;
mod model;
mod properties;

use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use answers::{Answer, model_answer, normalise, same_answer};
use calls::{Call, DOCS, PROJECT, Step, concretise, describe_call, step, to_tool};
use linearizable::{Event, linearizable};
use model::{Cmd, World};
use proptest::test_runner::{Config, TestCaseError, TestRunner};
use serde_json::json;
use support::{Client, Outcome, Server, TestDatabase, call, connect, runtime, start_test_server};

fn cases() -> u32 {
    std::env::var("MODEL_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(200)
}

// ── 1. environment ───────────────────────────────────────────────────────────

struct Env {
    _db: TestDatabase,
    server: Server,
    client: Client,
}

fn env() -> &'static Env {
    static ENV: OnceLock<Env> = OnceLock::new();
    ENV.get_or_init(|| {
        runtime().block_on(async {
            let db = support::test_database().await;
            let server = start_test_server(&db, Some("memory")).await.expect("server starts");
            let client = connect(&server.url).await;
            Env { _db: db, server, client }
        })
    })
}

fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

// Patch ids on the server, oldest first — index i is the model's patch i.
async fn patch_ids(c: &Client, doc: &str) -> Vec<String> {
    let Outcome::Ok(value) = call(c, "doc_history", json!({ "project": PROJECT, "doc": doc, "limit": 100 })).await
    else {
        return Vec::new();
    };
    let mut ids: Vec<String> = value["history"]
        .as_array()
        .map(|h| h.iter().filter_map(|p| p["patchId"].as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    ids.reverse();
    ids
}

async fn concretise_on_server(s: &Step, world: &World, c: &Client) -> Call {
    let ids = match s {
        Step::Revert { .. } => Some(patch_ids(c, s.doc()).await),
        _ => None,
    };
    concretise(s, world, ids.as_deref())
}

async fn on_server(c: &Client, request: &Call) -> Answer {
    let (tool, args) = to_tool(request);
    normalise(request, call(c, tool, args).await)
}

// An empty memory with the one project, on both sides.
async fn fresh_start(env: &Env) -> World {
    env.server
        .pool
        .get()
        .await
        .unwrap()
        .batch_execute(
            "TRUNCATE memory_projects, memory_project_docs, memory_doc_patches, memory_index RESTART IDENTITY CASCADE",
        )
        .await
        .unwrap();
    let created = call(&env.client, "create_project", json!({ "slug": PROJECT, "title": "Model" })).await;
    assert!(matches!(created, Outcome::Ok(_)), "creating the project: {created:?}");
    World::new()
}

// ── 2. sequential ────────────────────────────────────────────────────────────

// Runs the steps on both sides; the transcript of every call so far is the
// failure report. Returns the model's world afterwards.
async fn sequential(
    steps: &[Step],
    start: World,
    client: &Client,
    transcript: &mut Vec<String>,
) -> Result<World, String> {
    let mut world = start;
    for s in steps {
        let request = concretise_on_server(s, &world, client).await;
        let (next, expected) = model_answer(&world, &request);
        let actual = on_server(client, &request).await;
        transcript.push(format!("  {}\n      → {actual}", describe_call(&request)));
        if !same_answer(&expected, &actual) {
            return Err(format!(
                "the server and the model disagree:\n{}\n  model expected: {expected}",
                transcript.join("\n")
            ));
        }
        world = next;
    }
    Ok(world)
}

#[test]
fn answers_every_call_sequence_the_way_the_model_does() {
    let _serial = serial();
    let env = env();
    TestRunner::new(Config::with_cases(cases()))
        .run(&proptest::collection::vec(step(DOCS.len()), 1..=30), |steps| {
            runtime().block_on(async {
                let start = fresh_start(env).await;
                sequential(&steps, start, &env.client, &mut Vec::new()).await.map(|_| ()).map_err(TestCaseError::fail)
            })
        })
        .unwrap();
}

// ── 3. concurrent ────────────────────────────────────────────────────────────

// The sessions' steps are made concrete against the same snapshot, so they
// collide: two patches of the same version, a revert racing a patch.
async fn concurrent(env: &Env, prefix: &[Step], sessions: &[Vec<Step>]) -> Result<(), String> {
    let mut transcript = Vec::new();
    let start = fresh_start(env).await;
    let world = sequential(prefix, start, &env.client, &mut transcript).await?;
    let mut planned: Vec<Vec<Call>> = Vec::new();
    for steps in sessions {
        let mut calls = Vec::new();
        for s in steps {
            calls.push(concretise_on_server(s, &world, &env.client).await);
        }
        planned.push(calls);
    }

    let mut clients = Vec::new();
    for _ in &planned {
        clients.push(connect(&env.server.url).await);
    }
    let t0 = Instant::now();
    let ms = move || t0.elapsed().as_secs_f64() * 1000.0;
    let runs = planned.iter().enumerate().map(|(session, calls)| {
        let client = &clients[session];
        async move {
            let mut events = Vec::new();
            for request in calls {
                let start = ms();
                let answer = on_server(client, request).await;
                events.push(Event { who: format!("s{session}"), call: request.clone(), answer, start, end: ms() });
            }
            events
        }
    });
    let mut events: Vec<Event> = futures::future::join_all(runs).await.into_iter().flatten().collect();
    for c in clients {
        let _ = c.cancel().await;
    }

    // Afterwards, one look at each document: the final state must be one the
    // linearization reaches.
    for doc in DOCS {
        for cmd in [Cmd::Read { doc: doc.to_owned() }, Cmd::History { doc: doc.to_owned() }] {
            let request = Call { cmd, pid: None };
            let start = ms();
            let answer = on_server(&env.client, &request).await;
            events.push(Event { who: "after".into(), call: request, answer, start, end: ms() });
        }
    }

    if linearizable(&events, &world) {
        return Ok(());
    }
    events.sort_by(|a, b| a.start.total_cmp(&b.start));
    let lines: Vec<String> = events
        .iter()
        .map(|e| {
            format!(
                "  {:>5} [{:.1}–{:.1}ms] {}\n          → {}",
                e.who,
                e.start,
                e.end,
                describe_call(&e.call),
                e.answer
            )
        })
        .collect();
    Err(format!(
        "not linearizable — no order of these calls makes the model give these answers.\nprefix:\n{}\nconcurrent:\n{}",
        transcript.join("\n"),
        lines.join("\n")
    ))
}

#[test]
fn behaves_as_if_overlapping_calls_ran_one_at_a_time() {
    let _serial = serial();
    let env = env();
    let session = proptest::collection::vec(step(1), 1..=3);
    let strategy = (proptest::collection::vec(step(1), 0..=6), proptest::collection::vec(session, 2..=4));
    TestRunner::new(Config::with_cases(cases()))
        .run(&strategy, |(prefix, sessions)| {
            runtime().block_on(concurrent(env, &prefix, &sessions)).map_err(TestCaseError::fail)
        })
        .unwrap();
}
