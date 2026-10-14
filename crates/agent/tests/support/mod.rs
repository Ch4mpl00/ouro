#![allow(dead_code)]
use agent::{
    agent::{Engine, EngineDeps, Providers, create_engine},
    codex::Codex,
    db::AgentDb,
    local_recorder::local_recorder_tracer,
    mcp::Mcp,
    models::default_presets,
    providers::{ChatProvider, CompletionParams, CompletionResult, Message, ProviderKind, ToolDef},
    session::{EnvData, Session, session},
    skills::FsSkillStore,
};
use async_trait::async_trait;
use serde_json::{Map, Value, json};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

pub fn context() -> Session {
    session(
        "telegram:42",
        EnvData {
            now: "2026-09-06T12:00:00Z".parse().unwrap(),
            timezone: "Europe/Chisinau".into(),
            user_email: None,
            news_last_read_at: None,
            tech_last_read_at: None,
        },
    )
}
pub fn calls(calls: &[(&str, Value)]) -> Message {
    json!({"role": "assistant", "content": null, "tool_calls": calls.iter().enumerate().map(|(i,(name,args))| json!({"id": format!("call-{i}"), "type": "function", "function": {"name": name, "arguments": args.to_string()}})).collect::<Vec<_>>()}).as_object().unwrap().clone()
}
#[derive(Clone)]
pub struct Seen {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
    pub model: String,
}
pub struct Scripted {
    pub script: Mutex<VecDeque<Message>>,
    pub seen: Mutex<Vec<Seen>>,
}
impl Scripted {
    pub fn new(messages: Vec<Message>) -> Self {
        Self { script: Mutex::new(messages.into()), seen: Mutex::new(Vec::new()) }
    }
}
#[async_trait]
impl ChatProvider for Scripted {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Openai
    }
    async fn complete(&self, p: &mut CompletionParams<'_>) -> anyhow::Result<CompletionResult> {
        self.seen.lock().unwrap().push(Seen {
            messages: p.messages.clone(),
            tools: p.tools.clone(),
            model: p.model.clone(),
        });
        let message = self.script.lock().unwrap().pop_front().ok_or_else(|| anyhow::anyhow!("script exhausted"))?;
        Ok(CompletionResult { message, finish_reason: Some("stop".into()), usage: None })
    }
}
#[derive(Default)]
pub struct FakeMcp {
    pub defs: Vec<ToolDef>,
    pub called: Mutex<Vec<(String, Map<String, Value>)>>,
    pub active: AtomicUsize,
    pub peak: AtomicUsize,
    pub closed: AtomicUsize,
}
#[async_trait]
impl Mcp for FakeMcp {
    fn tools(&self) -> Vec<ToolDef> {
        self.defs.clone()
    }
    async fn call_tool(
        &self,
        name: &str,
        args: Map<String, Value>,
        cancel: &CancellationToken,
    ) -> anyhow::Result<String> {
        self.called.lock().unwrap().push((name.into(), args.clone()));
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(active, Ordering::SeqCst);
        let result = agent::errors::or_cancel(cancel, async {
            tokio::time::sleep(std::time::Duration::from_millis(
                args.get("delay").and_then(Value::as_u64).unwrap_or(0),
            ))
            .await;
            match name {
                "t_throw" => Err(anyhow::anyhow!("connection reset")),
                "t_err" => Ok("[tool error] refused".into()),
                "t_ok" => Ok(format!("ok {}", Value::Object(args.clone()))),
                _ => Ok(args.get("output").and_then(Value::as_str).unwrap_or("ok").to_owned()),
            }
        })
        .await;
        self.active.fetch_sub(1, Ordering::SeqCst);
        result
    }
    async fn close(&self) {
        self.closed.fetch_add(1, Ordering::SeqCst);
    }
}
pub struct NoCodex;
#[async_trait]
impl Codex for NoCodex {
    async fn run(&self, _: Value, _: &CancellationToken) -> anyhow::Result<String> {
        anyhow::bail!("unexpected codex call")
    }
}
pub struct Fixture {
    pub root: tempfile::TempDir,
    pub db: Arc<AgentDb>,
    pub skills: Arc<FsSkillStore>,
    pub mcp: Arc<FakeMcp>,
    pub engine: Arc<Engine>,
}
pub fn fixture(provider: Arc<dyn ChatProvider>, definitions: &[&str], skills: &[(&str, &str)]) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("skills.default")).unwrap();
    for (name, body) in skills {
        std::fs::write(root.path().join("skills.default").join(format!("{name}.md")), body).unwrap();
    }
    let db = Arc::new(AgentDb::open(root.path().join("agent.db")).unwrap());
    let store = Arc::new(FsSkillStore::new(root.path()));
    let mcp = Arc::new(FakeMcp {
        defs: definitions.iter().map(|name| ToolDef::new(name, name, json!({"type":"object"}))).collect(),
        ..Default::default()
    });
    let engine = create_engine(EngineDeps {
        providers: Providers {
            openai: provider.clone(),
            gemini: provider.clone(),
            deepseek: provider,
            openrouter: None,
        },
        mcp: mcp.clone(),
        presets: default_presets(),
        skills: Vec::new(),
        skill_store: store.clone(),
        memory: db.clone(),
        tracer: Arc::new(local_recorder_tracer(db.clone())),
        codex: Arc::new(NoCodex),
        decisions: None,
    });
    Fixture { root, db, skills: store, mcp, engine }
}
