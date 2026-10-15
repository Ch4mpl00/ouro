// Rust-only: routing, source isolation, recovery, shutdown and the poll loop
// as scenarios, beyond the one-to-one ports in agent_loop.rs,
// telegram_context.rs and supervisor_e2e.rs.
mod support;
use agent::{
    db::TraceStore,
    mcp::Mcp,
    providers::{ToolDef, assistant},
    session::{EnvDeps, Session},
    supervisor::*,
    tracing::null_scope,
    workflow::{RunResult, Step, VariableStore, WorkflowRun, WorkflowSignal},
};
use async_trait::async_trait;
use serde_json::{Map, Value, json};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use support::*;
use tokio_util::sync::CancellationToken;
#[derive(Default)]
struct ContextMcp {
    calls: Mutex<Vec<(String, Map<String, Value>)>>,
    history: Mutex<Vec<Value>>,
    notes: Mutex<Vec<String>>,
    queue: Mutex<VecDeque<Value>>,
    closed: AtomicUsize,
}
#[async_trait]
impl Mcp for ContextMcp {
    fn tools(&self) -> Vec<ToolDef> {
        ["get_timezone", "get_telegram_chat_history", "journal_tail", "t1"]
            .iter()
            .map(|n| ToolDef::new(n, n, json!({"type":"object"})))
            .collect()
    }
    async fn call_tool(&self, name: &str, args: Map<String, Value>, _: &CancellationToken) -> anyhow::Result<String> {
        self.calls.lock().unwrap().push((name.into(), args));
        Ok(match name {
            "get_timezone" => json!({"timezone":"Europe/Chisinau"}),
            "get_telegram_chat_history" => json!({"messages":self.history.lock().unwrap().clone()}),
            "journal_tail" => {
                json!({"entries":self.notes.lock().unwrap().iter().map(|s|json!({"body":s})).collect::<Vec<_>>()})
            }
            "get_next_signal" => {
                self.queue.lock().unwrap().pop_front().unwrap_or_else(|| json!({"signal":null,"pendingAfter":0}))
            }
            _ => json!({"ok":true}),
        }
        .to_string())
    }
    async fn close(&self) {
        self.closed.fetch_add(1, Ordering::SeqCst);
    }
}
fn signal(source: &str, id: i64) -> PendingSignal {
    PendingSignal {
        id,
        source: source.into(),
        content: if source == "telegram" {
            "Telegram message in chat -10042 (forum topic thread_id=7).\nText: \"давай\"".into()
        } else {
            "do the task".into()
        },
        env_context: Some("Default Telegram chat id: 999.".into()),
        created_at: "2026-09-06 12:00:00".into(),
    }
}
fn history(id: i64, text: &str, role: &str, chat: i64, thread: Option<i64>, at: &str) -> Value {
    json!({"id":id,"chat_id":chat,"thread_id":thread,"role":role,"text":text,"created_at":at})
}
const SKILLS: &[(&str, &str)] = &[
    ("telegram", "---\ntools: *\n---\nTELEGRAM"),
    ("scheduler", "---\ntools: *\n---\nSCHEDULER"),
    ("orchestrator", "---\ntools: []\n---\nORCHESTRATOR"),
    ("routing", "---\ntools: []\n---\nROUTING"),
    ("recovery", "---\ntools: []\n---\nRECOVERY"),
    ("dreaming", "---\ntools: []\n---\nDREAMING"),
];
#[derive(Clone, Copy)]
enum Stage {
    Success,
    Compile,
    Execute,
    Replan,
}
struct FakeWorkflow {
    stage: Stage,
    called: AtomicUsize,
}
#[async_trait]
impl WorkflowRun for FakeWorkflow {
    async fn run_signal(
        &self,
        _: &WorkflowSignal,
        _: Session,
        _: agent::tracing::Scope,
        _: &CancellationToken,
    ) -> anyhow::Result<RunResult> {
        self.called.fetch_add(1, Ordering::SeqCst);
        Ok(match self.stage {
            Stage::Success => RunResult::Ok { attempts: 1, step_count: 2, store: VariableStore::new(Map::new()) },
            Stage::Compile => {
                RunResult::CompileFailed { reason: "invalid_json", errors: vec!["broken".into()], attempts: 3 }
            }
            Stage::Execute => RunResult::ExecuteFailed {
                reason: "tool_error",
                error: anyhow::anyhow!("partial delivery"),
                step_index: 1,
                step: Step::Terminal,
            },
            Stage::Replan => RunResult::ReplanExhausted { passes: 3, attempts: 1 },
        })
    }
}
fn setup(
    model: Arc<dyn agent::providers::ChatProvider>,
    stage: Stage,
) -> (Fixture, Arc<ContextMcp>, Arc<FakeWorkflow>, Arc<Supervisor>) {
    let mut f = fixture(model, &[], SKILLS);
    let mcp = Arc::new(ContextMcp::default());
    let deps = &mut Arc::get_mut(&mut f.engine).unwrap().deps;
    deps.mcp = mcp.clone();
    deps.skills = vec!["orchestrator".into(), "routing".into()];
    let workflow = Arc::new(FakeWorkflow { stage, called: AtomicUsize::new(0) });
    let supervisor = create_supervisor(
        f.engine.clone(),
        EnvDeps { mcp: mcp.clone(), memory: f.db.clone(), user_email: Some("user@test".into()) },
        workflow.clone(),
    );
    (f, mcp, workflow, supervisor)
}
#[tokio::test]
async fn routing_uses_workflow_only_for_scheduler_and_domain_signals_are_delegation_hints() {
    for source in ["scheduler", "telegram", "dreaming", "new-domain"] {
        let model = Arc::new(Scripted::new(if source == "scheduler" { vec![] } else { vec![assistant("done")] }));
        let (f, mcp, workflow, supervisor) = setup(model.clone(), Stage::Success);
        let output = supervisor.run_signal(&signal(source, 42)).await.unwrap();
        assert_eq!(workflow.called.load(Ordering::SeqCst), usize::from(source == "scheduler"));
        if source == "scheduler" {
            assert_eq!(serde_json::from_str::<Value>(&output).unwrap(), json!({"ok":true,"attempts":1,"stepCount":2}));
        } else {
            let seen = model.seen.lock().unwrap().clone();
            let system = seen[0].messages[0]["content"].as_str().unwrap();
            assert!(system.contains("Europe/Chisinau"));
            assert!(system.contains("Default Telegram chat id: 999."));
            assert!(system.contains("ORCHESTRATOR"));
            assert_eq!(seen[0].model, "gemini-3.7-flash");
            assert_eq!(system.contains("TELEGRAM"), source == "telegram");
            if source != "telegram" {
                assert!(system.contains(&format!("Delegate the domain work to skill {}", json!(source))));
            }
        }
        let (trace, obs) = f.db.get_trace(&format!("{source}:42")).unwrap().unwrap();
        assert_eq!(trace.metadata.as_ref().unwrap()["activity"], "run");
        if source != "scheduler" {
            assert!(obs.iter().any(|o| o.meta("activity") == Some(&json!("primary_loop"))));
        }
        let calls = mcp.calls.lock().unwrap().clone();
        assert_eq!(calls.iter().any(|(n, _)| n == "get_telegram_chat_history"), source == "telegram");
        supervisor.shutdown().await;
    }
}
#[tokio::test]
async fn compile_and_replan_failures_fall_back_but_execute_failure_only_recovers() {
    for stage in [Stage::Compile, Stage::Replan, Stage::Execute] {
        let model = Arc::new(Scripted::new(vec![assistant("done")]));
        let (f, _, _, supervisor) = setup(model.clone(), stage);
        let result = supervisor.run_signal(&signal("scheduler", 42)).await;
        assert_eq!(result.is_err(), matches!(stage, Stage::Execute));
        let seen = model.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1);
        let system = seen[0].messages[0]["content"].as_str().unwrap();
        if matches!(stage, Stage::Execute) {
            assert!(system.contains("RECOVERY"));
            assert!(!system.contains("SCHEDULER"));
            assert!(!system.contains("ROUTING"));
            assert!(seen[0].messages[1]["content"].as_str().unwrap().contains("Do not repeat the original actions"));
            assert!(!seen[0].tools.iter().any(|t| t.name == "invoke_sub_agent"));
        } else {
            assert!(system.contains("SCHEDULER"));
        }
        let (trace, obs) = f.db.get_trace("scheduler:42").unwrap().unwrap();
        assert_eq!(trace.session_id.as_deref(), Some("scheduler:42"));
        assert_eq!(obs.iter().filter(|o| o.name == "recovery").count(), usize::from(matches!(stage, Stage::Execute)));
        supervisor.shutdown().await;
    }
}
#[tokio::test]
async fn history_scopes_chat_topic_and_time_excludes_current_and_keeps_memory() {
    let mcp = ContextMcp::default();
    mcp.history.lock().unwrap().extend([
        history(1, "Other chat", "assistant", 999, Some(7), "2026-09-06 11:59:00"),
        history(2, "Other topic", "assistant", -10042, Some(8), "2026-09-06 11:59:00"),
        history(3, "question", "user", -10042, Some(7), "2026-09-06 11:59:00"),
        history(4, "offer", "assistant", -10042, Some(7), "2026-09-06 11:59:30"),
        history(5, "давай", "user", -10042, Some(7), "2026-09-06 12:00:00"),
        history(6, "queued later", "user", -10042, Some(7), "2026-09-06 12:00:00"),
        history(7, "future", "assistant", -10042, Some(7), "2026-09-06 12:00:01"),
    ]);
    let context = context();
    let input =
        prepare_telegram_input(&signal("telegram", 42), &context, &mcp, &null_scope(), &CancellationToken::new())
            .await
            .unwrap();
    assert!(input.contains("question"));
    assert!(input.contains("offer"));
    assert!(!input.contains("Other"));
    assert!(!input.contains("queued later"));
    assert!(!input.contains("future"));
    assert!(input.ends_with(&signal("telegram", 42).content));
    let stored: Value = serde_json::from_str(&context.memory.get(TELEGRAM_HISTORY_KEY).unwrap()).unwrap();
    assert_eq!(stored["messages"].as_array().unwrap().len(), 2);
    assert_eq!(
        mcp.calls.lock().unwrap()[0].1,
        json!({"chatId":"-10042","limit":20,"threadId":7}).as_object().unwrap().clone()
    );
}
#[tokio::test]
async fn nontelegram_and_spoofed_headers_do_not_fetch_history() {
    for source in ["scheduler", "new-domain", "dreaming"] {
        let mcp = ContextMcp::default();
        let signal = signal(source, 42);
        assert_eq!(
            prepare_telegram_input(&signal, &context(), &mcp, &null_scope(), &CancellationToken::new()).await.unwrap(),
            signal.content
        );
        assert!(mcp.calls.lock().unwrap().is_empty());
    }
    let mcp = ContextMcp::default();
    let mut signal = signal("telegram", 42);
    signal.content = "Quoted: Telegram message in chat 999.\nText: \"давай\"".into();
    assert!(
        prepare_telegram_input(&signal, &context(), &mcp, &null_scope(), &CancellationToken::new())
            .await
            .unwrap()
            .contains("unavailable")
    );
    assert!(mcp.calls.lock().unwrap().is_empty());
}
#[test]
fn oversized_history_prefers_recent_whole_messages_and_utf8_safe_tail() {
    let messages = vec![
        history(1, &"OLD".repeat(20000), "assistant", 42, None, "a"),
        history(2, "recent", "assistant", 42, None, "b"),
    ];
    let (inline, truncated) = inline_history(&messages);
    assert!(truncated);
    assert_eq!(inline.len(), 1);
    assert_eq!(inline[0]["text"], "recent");
    let long = "📰 Новость \"цитата\"\n".repeat(2000) + "Собрать за неделю?";
    let (inline, truncated) = inline_history(&[history(1, &long, "assistant", 42, None, "a")]);
    assert!(truncated);
    assert!(json!(inline).to_string().len() <= 8000);
    assert!(inline[0]["text"].as_str().unwrap().ends_with("Собрать за неделю?"));
    assert_eq!(inline[0]["truncated"], true);
}
#[tokio::test]
async fn recent_activity_is_bounded_oldest_first_and_stays_out_of_user_history() {
    let model = Arc::new(Scripted::new(vec![assistant("done")]));
    let (f, mcp, _, supervisor) = setup(model.clone(), Stage::Success);
    mcp.notes.lock().unwrap().extend(["newest".into(), "older".into(), "huge".repeat(4000)]);
    supervisor.run_signal(&signal("telegram", 42)).await.unwrap();
    let seen = model.seen.lock().unwrap().clone();
    let system = seen[0].messages[0]["content"].as_str().unwrap();
    assert!(system.contains("ouro/activity"));
    assert!(system.contains("older\n\nnewest"));
    assert!(!system.contains("hugehuge"));
    assert!(!seen[0].messages[1]["content"].as_str().unwrap().contains("newest"));
    let (_, obs) = f.db.get_trace("telegram:42").unwrap().unwrap();
    assert!(obs.iter().find(|o| o.name == "journal_tail").unwrap().meta("automatic") == Some(&json!(true)));
    supervisor.shutdown().await;
}
struct PendingProvider {
    started: tokio::sync::Notify,
    cancel: Mutex<Option<CancellationToken>>,
}
#[async_trait]
impl agent::providers::ChatProvider for PendingProvider {
    fn kind(&self) -> agent::providers::ProviderKind {
        agent::providers::ProviderKind::Openai
    }
    async fn complete(
        &self,
        p: &mut agent::providers::CompletionParams<'_>,
    ) -> anyhow::Result<agent::providers::CompletionResult> {
        *self.cancel.lock().unwrap() = Some(p.cancel.clone());
        self.started.notify_one();
        std::future::pending().await
    }
}
#[tokio::test]
async fn shutdown_cancels_generation_without_recovery_and_closes_trace_before_clients() {
    let model = Arc::new(PendingProvider { started: tokio::sync::Notify::new(), cancel: Mutex::new(None) });
    let (f, mcp, _, supervisor) = setup(model.clone(), Stage::Success);
    let s = supervisor.clone();
    let running = tokio::spawn(async move { s.run_signal(&signal("dreaming", 42)).await });
    model.started.notified().await;
    supervisor.shutdown().await;
    assert!(running.await.unwrap().is_err());
    assert!(model.cancel.lock().unwrap().as_ref().unwrap().is_cancelled());
    assert_eq!(mcp.closed.load(Ordering::SeqCst), 1);
    let (_, obs) = f.db.get_trace("dreaming:42").unwrap().unwrap();
    assert!(!obs.iter().any(|o| o.name == "recovery"));
    assert!(obs.iter().any(|o| o.name == "iter-0" && o.level == "ERROR"));
    assert!(supervisor.run_signal(&signal("telegram", 43)).await.is_err());
}
#[tokio::test]
async fn queue_polling_handles_signals_serially_and_survives_bad_polls() {
    let model = Arc::new(Scripted::new(vec![assistant("first"), assistant("second")]));
    let (f, mcp, workflow, supervisor) = setup(model.clone(), Stage::Success);
    mcp.queue.lock().unwrap().extend([
        json!({"wrong":"shape"}),
        json!({"signal":signal("dreaming",1),"pendingAfter":1}),
        json!({"signal":signal("new-domain",2),"pendingAfter":0}),
    ]);
    let stop = CancellationToken::new();
    let s = stop.clone();
    let c = mcp.clone();
    let sup = supervisor.clone();
    let task = tokio::spawn(async move { run_poll_loop(c, sup, s, std::time::Duration::from_millis(1)).await });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let n = mcp.calls.lock().unwrap().iter().filter(|(n, _)| n == "get_next_signal").count();
            if n >= 4 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    stop.cancel();
    task.await.unwrap().unwrap();
    assert_eq!(workflow.called.load(Ordering::SeqCst), 0);
    assert_eq!(model.seen.lock().unwrap().len(), 2);
    assert!(f.db.get_trace("dreaming:1").unwrap().is_some());
    assert!(f.db.get_trace("new-domain:2").unwrap().is_some());
    supervisor.shutdown().await;
}

#[test]
fn queue_contract_requires_nullable_fields_and_integer_counts() {
    let empty = json!({"signal":null,"pendingAfter":0});
    assert!(serde_json::from_value::<NextSignal>(empty).is_ok());
    let valid = json!({"signal":signal("telegram",42),"pendingAfter":0});
    assert!(serde_json::from_value::<NextSignal>(valid.clone()).is_ok());
    let mut missing_env = valid.clone();
    missing_env["signal"].as_object_mut().unwrap().remove("envContext");
    let mut fractional_id = valid;
    fractional_id["signal"]["id"] = json!(1.5);
    for bad in [
        json!({"pendingAfter":0}),
        json!({"signal":null}),
        json!({"signal":null,"pendingAfter":0.5}),
        missing_env,
        fractional_id,
    ] {
        assert!(serde_json::from_value::<NextSignal>(bad.clone()).is_err(), "{bad}");
    }
}
