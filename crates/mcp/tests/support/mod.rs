// Shared by the black-box test targets (specs, model): a Postgres, the real
// server in-process, and an MCP client that calls it.
//
//   1. database — Testcontainers pgvector, or TEST_DATABASE_URL
//   2. server   — every dependency wired as main.rs does, nothing external
//   3. client   — one MCP session, a call and its outcome

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use mcp_tools::db::Db;
use mcp_tools::embeddings::{Embedder, SharedEmbedder};
use mcp_tools::fetch::{GuardedFetch, SharedFetch};
use mcp_tools::gmail::GmailModule;
use mcp_tools::knowledge::KnowledgeRepository;
use mcp_tools::memory::{MemoryService, PgMemoryStore};
use mcp_tools::monobank::Monobank;
use mcp_tools::news::NewsRepository;
use mcp_tools::pg::{self, PgPool};
use mcp_tools::scheduler::Scheduler;
use mcp_tools::server::{Deps, HttpOptions, McpTools};
use mcp_tools::settings::Settings;
use mcp_tools::signals::Signals;
use mcp_tools::skills::SkillCatalog;
use mcp_tools::telegram::{TelegramConfig, TelegramModule};
use mcp_tools::toolsets;
use mcp_tools::userbot::{SharedAccount, Userbot};
use rmcp::model::CallToolRequestParams;
use rmcp::service::{RunningService, ServiceError};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{RoleClient, ServiceExt};
use serde_json::Value;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use tokio_util::sync::CancellationToken;

// ── 1. database ──────────────────────────────────────────────────────────────

// A container unless a throwaway database is named. Hold the container until
// the run ends; dropping it removes it.
pub async fn database() -> (String, Option<ContainerAsync<Postgres>>) {
    if let Ok(url) = std::env::var("TEST_DATABASE_URL") {
        return (url, None);
    }
    let pg = Postgres::default()
        .with_name("pgvector/pgvector")
        .with_tag("pg16")
        .start()
        .await
        .expect("Docker is needed (or set TEST_DATABASE_URL)");
    let port = pg.get_host_port_ipv4(5432).await.expect("mapped port");
    (format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres"), Some(pg))
}

// A throwaway database pair for one test file, as packages/mcp's
// test/support/database.ts: TEST_DATABASE_URL named → a fresh news database
// and its own state database inside that server, dropped afterwards;
// otherwise a container of our own.
pub struct TestDatabase {
    pub url: String,
    pub state_url: String,
    admin: Option<String>,
    names: Vec<String>,
    _container: Option<ContainerAsync<Postgres>>,
}

impl TestDatabase {
    pub async fn drop_all(self) {
        let Some(admin) = self.admin else { return };
        let Ok(pool) = pg::connect(&admin) else { return };
        let Ok(client) = pool.get().await else { return };
        for name in self.names {
            let _ = client.batch_execute(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)")).await;
        }
    }
}

fn with_database(server_url: &str, name: &str) -> String {
    let mut url = url::Url::parse(server_url).expect("a database URL");
    url.set_path(&format!("/{name}"));
    url.to_string()
}

pub async fn test_database() -> TestDatabase {
    let (server, container) = database().await;
    if container.is_some() {
        let state_url = with_database(&server, "mcp_state");
        return TestDatabase { url: server, state_url, admin: None, names: Vec::new(), _container: container };
    }
    let name = format!("t_{:016x}", rand::random::<u64>());
    let pool = pg::connect(&server).expect("TEST_DATABASE_URL must parse");
    pool.get().await.expect("test server").batch_execute(&format!("CREATE DATABASE {name}")).await.expect("create");
    TestDatabase {
        url: with_database(&server, &name),
        state_url: with_database(&server, &format!("{name}_state")),
        admin: Some(server),
        names: vec![format!("{name}_state"), name],
        _container: None,
    }
}

// ── 2. server ────────────────────────────────────────────────────────────────

// Deterministic stand-in for the embedding model: a bag-of-words vector, so
// distance tracks word overlap. Recall works without OpenAI.
struct WordEmbedder;

#[async_trait]
impl Embedder for WordEmbedder {
    async fn embed_batch(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(word_vectors(texts))
    }
}

pub fn word_vectors(texts: &[String]) -> Vec<Vec<f32>> {
    texts
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
        .collect()
}

// Property tests drive one server across many cases, so the server, its
// database and every case run on one runtime that outlives each test fn.
pub fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RUNTIME.get_or_init(|| tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("runtime"))
}

// The number of generated cases, PROPERTY_RUNS when set (as in the TS suites).
pub fn property_runs(default: u32) -> u32 {
    std::env::var("PROPERTY_RUNS").ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

pub struct Server {
    pub url: String,
    pub pool: PgPool,
    pub db: Db,
    pub signals: Signals,
    pub scheduler: Scheduler,
}

// `toolsets`: the MCP_TOOLSETS value (None = every group). A restricted
// instance is multi-session, as in production.
pub async fn start_server(database_url: &str, toolsets: Option<&str>) -> anyhow::Result<Server> {
    let db = Db::connect(database_url).await?;
    serve(database_url, db, toolsets, Arc::new(WordEmbedder), "spec", None).await
}

// A server on a TestDatabase pair, as TS startServer(db.url, toolsets).
pub async fn start_test_server(database: &TestDatabase, toolsets: Option<&str>) -> anyhow::Result<Server> {
    start_test_server_as(database, toolsets, "spec").await
}

// The same, with the instance's MCP_MEMORY_ACTOR.
pub async fn start_test_server_as(
    database: &TestDatabase,
    toolsets: Option<&str>,
    actor: &str,
) -> anyhow::Result<Server> {
    let db = Db::connect_state(&database.url, &database.state_url).await?;
    serve(&database.url, db, toolsets, Arc::new(WordEmbedder), actor, None).await
}

// The same, with the Telegram account played by a stand-in.
pub async fn start_test_server_with_account(
    database: &TestDatabase,
    toolsets: Option<&str>,
    account: SharedAccount,
) -> anyhow::Result<Server> {
    let db = Db::connect_state(&database.url, &database.state_url).await?;
    serve_with(&database.url, db, toolsets, Arc::new(WordEmbedder), "spec", None, Some(account)).await
}

// The same, with the guarded fetch behind fetch_url and the news providers —
// tests narrow what counts as private so a local server can play the public
// internet.
pub async fn start_test_server_with_fetch(
    database: &TestDatabase,
    toolsets: Option<&str>,
    fetcher: SharedFetch,
) -> anyhow::Result<Server> {
    let db = Db::connect_state(&database.url, &database.state_url).await?;
    serve(&database.url, db, toolsets, Arc::new(WordEmbedder), "spec", Some(fetcher)).await
}

pub fn word_embedder() -> SharedEmbedder {
    Arc::new(WordEmbedder)
}

async fn serve(
    database_url: &str,
    db: Db,
    toolsets: Option<&str>,
    embedder: SharedEmbedder,
    actor: &str,
    fetcher: Option<SharedFetch>,
) -> anyhow::Result<Server> {
    serve_with(database_url, db, toolsets, embedder, actor, fetcher, None).await
}

#[allow(clippy::too_many_arguments)]
async fn serve_with(
    database_url: &str,
    db: Db,
    toolsets: Option<&str>,
    embedder: SharedEmbedder,
    actor: &str,
    fetcher: Option<SharedFetch>,
    account: Option<SharedAccount>,
) -> anyhow::Result<Server> {
    let pool = pg::connect(database_url)?;
    pg::migrate(&pool).await?;
    let http = reqwest::Client::new();
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
        userbot: account.unwrap_or_else(|| Arc::new(Userbot::new(db.clone()))),
        skills: SkillCatalog::new(PathBuf::from("skills"), PathBuf::from("../../skills.default")),
        fetcher: fetcher.unwrap_or_else(|| Arc::new(GuardedFetch::new())),
        storage_dir: std::env::temp_dir().join("mcp-specs-storage"),
        news: NewsRepository::new(pool.clone(), embedder.clone()),
        knowledge: KnowledgeRepository::new(pool.clone(), embedder.clone()),
        memory: MemoryService::new(Arc::new(PgMemoryStore::new(pool.clone())), embedder),
        memory_actor: actor.into(),
        gateway: None,
    });
    let scope = toolsets::parse_toolsets(toolsets)?;
    let router = toolsets::compose_router(&scope.names);
    let port = std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
    let options = HttpOptions { port, allowed_hosts: Vec::new(), multi_session: toolsets.is_some() };
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
    Ok(Server { url, pool, db, signals, scheduler })
}

// ── 3. client ────────────────────────────────────────────────────────────────

pub type Client = RunningService<RoleClient, ()>;

pub async fn connect(url: &str) -> Client {
    let transport = StreamableHttpClientTransport::with_client(
        reqwest::Client::new(),
        StreamableHttpClientTransportConfig::with_uri(url),
    );
    ().serve(transport).await.expect("MCP session opens")
}

#[derive(Debug, Clone)]
pub enum Outcome {
    Ok(Value),
    // The tool ran and failed (an `isError` result the model reads).
    ToolError(String),
    // The call never reached the tool: bad parameters, unknown tool.
    Rejected { code: i32, message: String },
}

pub async fn call(client: &Client, tool: &str, args: Value) -> Outcome {
    let mut params = CallToolRequestParams::new(tool.to_owned());
    if let Value::Object(map) = args {
        params = params.with_arguments(map);
    }
    match client.call_tool(params).await {
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
    }
}
