// The executable specification's machinery. The specification itself is
// specs/*.feature — plain-English scenarios a person reads and edits. This
// file only teaches cucumber what each sentence means, by driving the real
// server over MCP:
//
// Sections:
//   1. environment — one Postgres (Testcontainers) + the real server, in-process
//   2. world       — per-scenario state: the MCP session, the last result
//   3. calls       — generic steps: call a tool, check the result
//   4. memory      — project / document sentences
//   5. signals     — the queue
//   6. scheduler   — time zone, tasks, ticks
//   7. surface     — the tool list
//   8. main        — sequential run, a clean database per scenario
//
//   cargo test --test specs                      (Docker needed for Postgres)
//   TEST_DATABASE_URL=postgres://… cargo test --test specs

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use cucumber::gherkin::Step;
use cucumber::writer::Stats as _;
use cucumber::{World, given, then, when};
use mcp_tools::db::Db;
use mcp_tools::embeddings::{Embedder, SharedEmbedder};
use mcp_tools::gmail::GmailModule;
use mcp_tools::knowledge::KnowledgeRepository;
use mcp_tools::memory::{MemoryService, PgMemoryStore};
use mcp_tools::monobank::Monobank;
use mcp_tools::news::NewsRepository;
use mcp_tools::pg::{self, PgPool};
use mcp_tools::scheduler::{self, Scheduler};
use mcp_tools::server::{Deps, HttpOptions, McpTools};
use mcp_tools::settings::Settings;
use mcp_tools::signals::Signals;
use mcp_tools::skills::SkillCatalog;
use mcp_tools::telegram::{TelegramConfig, TelegramModule};
use mcp_tools::userbot::Userbot;
use mcp_tools::{fetch, toolsets};
use rmcp::model::CallToolRequestParams;
use rmcp::service::{RunningService, ServiceError};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use testcontainers_modules::postgres::Postgres;
use tokio_util::sync::CancellationToken;

// ── 1. environment ───────────────────────────────────────────────────────────

// Deterministic stand-in for the embedding model: a bag-of-words vector, so
// distance tracks word overlap. Recall works without OpenAI.
struct WordEmbedder;

#[async_trait]
impl Embedder for WordEmbedder {
    async fn embed_batch(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .map(|text| {
                let mut v = vec![0f32; 1536];
                for token in text.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()) {
                    let hash = token.chars().fold(0u64, |h, c| (h * 31 + u64::from(c)) % 1536);
                    v[hash as usize] += 1.0;
                }
                let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(f32::EPSILON);
                v.iter().map(|x| x / norm).collect()
            })
            .collect())
    }
}

// Built once; every scenario talks to the same server and wipes the data
// before it starts.
struct Env {
    url: String,
    pool: PgPool,
    db: Db,
    signals: Signals,
    scheduler: Scheduler,
}

static ENV: OnceLock<Env> = OnceLock::new();

fn env() -> &'static Env {
    ENV.get().expect("environment is set up before the first scenario")
}

async fn start_server(database_url: &str) -> anyhow::Result<Env> {
    let pool = pg::connect(database_url)?;
    pg::migrate(&pool).await?;
    let db = Db::connect(database_url).await?;
    let http = reqwest::Client::new();
    let embedder: SharedEmbedder = Arc::new(WordEmbedder);
    let settings = Settings::new(db.clone());
    let signals = Signals::new(db.clone());
    let scheduler = Scheduler::new(db.clone(), settings.clone());
    let deps = Arc::new(Deps {
        settings,
        signals: signals.clone(),
        scheduler: scheduler.clone(),
        // No bot token: sending fails as a tool error, nothing leaves the box.
        telegram: TelegramModule::new(TelegramConfig::default(), http.clone(), db.clone()),
        gmail: GmailModule::new(db.clone(), http.clone()),
        monobank: Monobank::new(http.clone()),
        userbot: Userbot::new(db.clone()),
        skills: SkillCatalog::new(PathBuf::from("skills"), PathBuf::from("../../skills.default")),
        fetcher: fetch::guarded_client(),
        storage_dir: std::env::temp_dir().join("mcp-specs-storage"),
        news: NewsRepository::new(pool.clone(), embedder.clone()),
        knowledge: KnowledgeRepository::new(pool.clone(), embedder.clone()),
        memory: MemoryService::new(Arc::new(PgMemoryStore::new(pool.clone())), embedder),
        memory_actor: "spec".into(),
        gateway: None,
    });
    let router = toolsets::compose_router(&toolsets::parse_toolsets(None)?.names);
    let port = std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
    let options = HttpOptions { port, allowed_hosts: Vec::new(), multi_session: false };
    tokio::spawn(mcp_tools::server::serve_http(
        move || McpTools::new(deps.clone(), router.clone()),
        options,
        CancellationToken::new(),
    ));
    let url = format!("http://127.0.0.1:{port}/mcp");
    for _ in 0..100 {
        if reqwest::Client::new().post(&url).send().await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    Ok(Env { url, pool, db, signals, scheduler })
}

// Every table a scenario can touch, emptied: each scenario starts from a
// freshly migrated database.
async fn wipe() -> anyhow::Result<()> {
    let env = env();
    env.pool
        .get()
        .await?
        .batch_execute(
            "TRUNCATE news_items, knowledge_base_notes, memory_projects, memory_project_docs,
                      memory_doc_patches, memory_facts, memory_index RESTART IDENTITY CASCADE",
        )
        .await?;
    env.db
        .client()
        .await?
        .batch_execute(
            "TRUNCATE signals, scheduled_tasks, settings, telegram_messages, telegram_kv, gmail_kv,
                      integration_account RESTART IDENTITY",
        )
        .await?;
    Ok(())
}

// ── 2. world ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
enum Outcome {
    Ok(Value),
    // The tool ran and failed (an `isError` result the model reads).
    ToolError(String),
    // The call never reached the tool: bad parameters, unknown tool.
    Rejected { code: i32, message: String },
}

#[derive(World)]
#[world(init = Self::new)]
struct Spec {
    client: Option<RunningService<RoleClient, ()>>,
    last: Option<Outcome>,
    remembered: HashMap<String, Value>,
    delivered: Vec<i64>,
    queued: Vec<i64>,
    last_task: Option<i64>,
}

impl std::fmt::Debug for Spec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Spec").field("last", &self.last).field("remembered", &self.remembered).finish()
    }
}

impl Spec {
    fn new() -> Self {
        Self {
            client: None,
            last: None,
            remembered: HashMap::new(),
            delivered: Vec::new(),
            queued: Vec::new(),
            last_task: None,
        }
    }

    async fn client(&mut self) -> &RunningService<RoleClient, ()> {
        if self.client.is_none() {
            let transport = StreamableHttpClientTransport::with_client(
                reqwest::Client::new(),
                StreamableHttpClientTransportConfig::with_uri(env().url.as_str()),
            );
            self.client = Some(().serve(transport).await.expect("MCP session opens"));
        }
        self.client.as_ref().expect("connected")
    }

    // `{{name}}` → a value remembered earlier in the scenario.
    fn substitute(&self, text: &str) -> String {
        self.remembered.iter().fold(text.to_owned(), |acc, (name, value)| {
            let plain = value.as_str().map_or_else(|| value.to_string(), str::to_owned);
            acc.replace(&format!("{{{{{name}}}}}"), &plain)
        })
    }

    async fn call(&mut self, tool: &str, args: Value) -> Outcome {
        let mut params = CallToolRequestParams::new(tool.to_owned());
        if let Value::Object(map) = args {
            params = params.with_arguments(map);
        }
        let outcome = match self.client().await.call_tool(params).await {
            Ok(result) => {
                let text = result.content.first().and_then(|c| c.as_text()).map_or("", |t| t.text.as_str()).to_owned();
                if result.is_error == Some(true) {
                    Outcome::ToolError(text)
                } else {
                    Outcome::Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
                }
            }
            Err(ServiceError::McpError(err)) => Outcome::Rejected { code: err.code.0, message: err.message.to_string() },
            Err(other) => panic!("MCP transport failed: {other}"),
        };
        self.last = Some(outcome.clone());
        outcome
    }

    fn result(&self) -> &Value {
        match &self.last {
            Some(Outcome::Ok(value)) => value,
            other => panic!("expected a successful call, got {other:?}"),
        }
    }
}

// ── 3. calls ─────────────────────────────────────────────────────────────────

fn docstring(step: &Step) -> &str {
    step.docstring.as_deref().expect("this step needs a \"\"\" block").trim_start_matches('\n')
}

fn table(step: &Step) -> &[Vec<String>] {
    &step.table.as_ref().expect("this step needs a | table |").rows
}

fn json_arg(spec: &Spec, step: &Step) -> Value {
    let raw = spec.substitute(docstring(step));
    serde_json::from_str(&raw).unwrap_or_else(|err| panic!("arguments are not JSON ({err}):\n{raw}"))
}

// "doc.body", "failures.0.reason"
fn at<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(value, |v, key| match key.parse::<usize>() {
        Ok(i) if v.is_array() => v.get(i),
        _ => v.get(key),
    })
}

// A table cell: JSON when it parses as JSON (2, true, null, "…", […]), the
// bare text otherwise.
fn cell(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_owned()))
}

// Every key in `expected` must be present in `actual` with a matching value;
// extra keys in `actual` are fine. Arrays match element by element.
fn subset(expected: &Value, actual: &Value) -> bool {
    match (expected, actual) {
        (Value::Object(e), Value::Object(a)) => e.iter().all(|(k, v)| a.get(k).is_some_and(|av| subset(v, av))),
        (Value::Array(e), Value::Array(a)) => e.len() == a.len() && e.iter().zip(a).all(|(x, y)| subset(x, y)),
        _ => expected == actual,
    }
}

#[given(expr = "I call {string}")]
#[when(expr = "I call {string}")]
async fn call_bare(spec: &mut Spec, tool: String) {
    spec.call(&tool, json!({})).await;
}

#[given(expr = "I call {string} with:")]
#[when(expr = "I call {string} with:")]
async fn call_with(spec: &mut Spec, tool: String, step: &Step) {
    let args = json_arg(spec, step);
    spec.call(&tool, args).await;
}

#[then("the result is:")]
async fn result_is(spec: &mut Spec, step: &Step) {
    let result = spec.result().clone();
    for row in table(step).iter().skip(1) {
        let (path, expected) = (&row[0], cell(&spec.substitute(&row[1])));
        let actual = at(&result, path).unwrap_or_else(|| panic!("no `{path}` in the result:\n{result:#}"));
        assert_eq!(actual, &expected, "`{path}` in the result:\n{result:#}");
    }
}

#[then("the result includes:")]
async fn result_includes(spec: &mut Spec, step: &Step) {
    let expected = json_arg(spec, step);
    let actual = spec.result();
    assert!(subset(&expected, actual), "expected the result to include\n{expected:#}\nbut it was\n{actual:#}");
}

#[then(expr = "the call fails with {string}")]
async fn fails_with(spec: &mut Spec, text: String) {
    match &spec.last {
        Some(Outcome::ToolError(message)) => assert!(message.contains(&text), "tool error was: {message}"),
        other => panic!("expected a tool error containing {text:?}, got {other:?}"),
    }
}

#[then("the call is rejected as invalid parameters")]
async fn rejected(spec: &mut Spec) {
    match &spec.last {
        Some(Outcome::Rejected { code: -32602, message }) => assert!(!message.is_empty()),
        other => panic!("expected an invalid-params rejection, got {other:?}"),
    }
}

#[given(expr = "I remember {string} as {string}")]
#[then(expr = "I remember {string} as {string}")]
async fn remember(spec: &mut Spec, path: String, name: String) {
    let value = at(spec.result(), &path).unwrap_or_else(|| panic!("no `{path}` to remember")).clone();
    spec.remembered.insert(name, value);
}

// ── 4. memory ────────────────────────────────────────────────────────────────

#[given(expr = "a project {string} titled {string}")]
async fn project(spec: &mut Spec, slug: String, title: String) {
    let outcome = spec.call("create_project", json!({ "slug": slug, "title": title })).await;
    assert!(matches!(&outcome, Outcome::Ok(v) if v["ok"] == true), "creating the project: {outcome:?}");
}

#[given(expr = "{string} in {string} reads:")]
async fn document_exists(spec: &mut Spec, doc: String, project: String, step: &Step) {
    let body = docstring(step).to_owned();
    let outcome = spec.call("write_doc", json!({ "project": project, "doc": doc, "body": body })).await;
    assert!(matches!(&outcome, Outcome::Ok(v) if v["ok"] == true), "writing the document: {outcome:?}");
}

#[then(expr = "{string} in {string} reads:")]
async fn document_reads(spec: &mut Spec, doc: String, project: String, step: &Step) {
    let expected = docstring(step).to_owned();
    let saved = spec.last.clone();
    spec.call("read_doc", json!({ "project": project, "doc": doc })).await;
    let body = spec.result()["doc"]["body"].as_str().unwrap_or_default().to_owned();
    spec.last = saved;
    assert_eq!(body, expected, "the document's text");
}

#[then(expr = "{string} in {string} is at version {int}")]
async fn document_version(spec: &mut Spec, doc: String, project: String, version: i64) {
    let saved = spec.last.clone();
    spec.call("read_doc", json!({ "project": project, "doc": doc })).await;
    let actual = spec.result()["doc"]["version"].as_i64();
    spec.last = saved;
    assert_eq!(actual, Some(version), "the document's version");
}

// ── 5. signals ───────────────────────────────────────────────────────────────

#[given("the signal queue holds:")]
async fn queue_holds(spec: &mut Spec, step: &Step) {
    for row in table(step).iter().skip(1) {
        spec.queued.push(env().signals.record(&row[0], &row[1]).await.expect("signal recorded"));
    }
}

#[given(expr = "the signal queue holds {int} signals")]
async fn queue_holds_many(spec: &mut Spec, count: usize) {
    for i in 0..count {
        spec.queued.push(env().signals.record("telegram", &format!("message {i}")).await.expect("signal recorded"));
    }
}

#[when(expr = "{int} agents take signals at the same time until the queue is empty")]
async fn concurrent_pop(spec: &mut Spec, agents: usize) {
    spec.client().await;
    let client = spec.client.as_ref().expect("connected");
    let workers = (0..agents).map(|_| async {
        let mut got = Vec::new();
        loop {
            let result = client.call_tool(CallToolRequestParams::new("get_next_signal")).await.expect("call");
            let text = &result.content[0].as_text().expect("text").text;
            let value: Value = serde_json::from_str(text).expect("json");
            match value["signal"]["id"].as_i64() {
                Some(id) => got.push(id),
                None => break got,
            }
        }
    });
    spec.delivered = futures::future::join_all(workers).await.into_iter().flatten().collect();
}

#[then("every signal was delivered exactly once")]
async fn exactly_once(spec: &mut Spec) {
    let mut counts: HashMap<i64, usize> = HashMap::new();
    for id in &spec.delivered {
        *counts.entry(*id).or_default() += 1;
    }
    let mut twice: Vec<i64> = counts.iter().filter(|(_, n)| **n > 1).map(|(id, _)| *id).collect();
    let mut lost: Vec<i64> = spec.queued.iter().copied().filter(|id| !counts.contains_key(id)).collect();
    twice.sort_unstable();
    lost.sort_unstable();
    assert!(
        twice.is_empty() && lost.is_empty(),
        "{} of {} signals were delivered more than once ({twice:?}); lost: {lost:?}",
        twice.len(),
        spec.queued.len()
    );
}

// ── 6. scheduler ─────────────────────────────────────────────────────────────

fn instant(text: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(text).unwrap_or_else(|_| panic!("not an ISO time: {text}")).with_timezone(&Utc)
}

#[given(expr = "the user's time zone is {string}")]
async fn time_zone(spec: &mut Spec, zone: String) {
    let outcome = spec.call("set_timezone", json!({ "tz": zone })).await;
    assert!(matches!(&outcome, Outcome::Ok(v) if v["ok"] == true), "setting the time zone: {outcome:?}");
}

#[given(expr = "a {word} task {string} saying {string} created at {string}")]
async fn task(spec: &mut Spec, kind: String, cron: String, prompt: String, created: String) {
    let recurring = match kind.as_str() {
        "recurring" => true,
        "one-shot" => false,
        other => panic!("a task is recurring or one-shot, not {other}"),
    };
    let row = env().scheduler.insert(&cron, recurring, &prompt, None).await.expect("task inserted");
    env()
        .db
        .client()
        .await
        .expect("db")
        .execute("UPDATE scheduled_tasks SET created_at = $1 WHERE id = $2", &[&instant(&created), &row.id])
        .await
        .expect("backdated");
    spec.last_task = Some(row.id);
}

#[given(expr = "it last fired for the slot {string}")]
async fn last_fired(spec: &mut Spec, slot: String) {
    let id = spec.last_task.expect("a task was created earlier in the scenario");
    env()
        .db
        .client()
        .await
        .expect("db")
        .execute("UPDATE scheduled_tasks SET last_run_at = $1 WHERE id = $2", &[&instant(&slot).timestamp(), &id])
        .await
        .expect("marked fired");
}

#[given(expr = "the scheduler ticks at {string}")]
#[when(expr = "the scheduler ticks at {string}")]
async fn tick(_spec: &mut Spec, now: String) {
    scheduler::tick(&env().scheduler, &env().signals, instant(&now)).await.expect("tick");
}

#[then(expr = "{int} signal(s) is/are waiting")]
async fn waiting(_spec: &mut Spec, count: i64) {
    assert_eq!(env().signals.count_pending().await.expect("count"), count, "signals waiting in the queue");
}

#[then(expr = "the next signal is a {string} signal for the slot {string}")]
async fn next_signal_slot(spec: &mut Spec, source: String, slot: String) {
    spec.call("get_next_signal", json!({})).await;
    let signal = spec.result()["signal"].clone();
    assert_eq!(signal["source"], source, "the signal's source: {signal:#}");
    let content = signal["content"].as_str().unwrap_or_default();
    assert!(content.contains(&format!("Slot: {slot}")), "the signal's slot:\n{content}");
}

// ── 7. surface ───────────────────────────────────────────────────────────────

#[then("the server offers exactly these tools:")]
async fn offers_tools(spec: &mut Spec, step: &Step) {
    let mut expected: Vec<String> = docstring(step).split_whitespace().map(str::to_owned).collect();
    expected.sort();
    let mut actual: Vec<String> =
        spec.client().await.list_all_tools().await.expect("tools/list").into_iter().map(|t| t.name.into_owned()).collect();
    actual.sort();
    assert_eq!(actual, expected, "the tool surface");
}

// ── 8. main ──────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    // A container unless a throwaway database is named; held until the run
    // ends so it is removed afterwards.
    let mut container: Option<ContainerAsync<Postgres>> = None;
    let url = match std::env::var("TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            let pg = Postgres::default()
                .with_name("pgvector/pgvector")
                .with_tag("pg16")
                .start()
                .await
                .expect("Docker is needed for the specs (or set TEST_DATABASE_URL)");
            let port = pg.get_host_port_ipv4(5432).await.expect("mapped port");
            container = Some(pg);
            format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres")
        }
    };
    let env = start_server(&url).await.expect("server starts");
    let _ = ENV.set(env);

    let result = Spec::cucumber()
        // One scenario at a time: they share the server and its database.
        .max_concurrent_scenarios(1)
        // A sentence nobody taught the harness is a failure, not a skip: the
        // specification must never pass on words that check nothing.
        .fail_on_skipped()
        .before(|_, _, _, _| Box::pin(async { wipe().await.expect("clean database") }))
        .run("specs")
        .await;
    let failed = result.execution_has_failed() || result.parsing_errors() > 0;
    drop(container);
    if failed {
        std::process::exit(1);
    }
}
