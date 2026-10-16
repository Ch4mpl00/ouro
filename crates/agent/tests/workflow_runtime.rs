// Rust-only: compile / execute / runner scenarios through the real engine,
// beyond the one-to-one ports in workflow_*.rs.
mod support;
use agent::{
    codex::Codex,
    db::{MemoryStore, TraceStore},
    decisions::decision_client,
    providers::{JsonApi, assistant},
    tracing::{Scope, TraceStart},
    workflow::*,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use support::*;
use tokio_util::sync::CancellationToken;
const SKILLS: &[(&str, &str)] = &[
    ("planner", "---\ntools: []\n---\nPLANNER"),
    ("s1", "---\ntools: [t1]\n---\nDOMAIN"),
    ("composer", "---\ntools: []\n---\nCOMPOSER"),
];
fn runtime(f: &Fixture) -> Arc<WorkflowRunner> {
    create_workflow_runner(f.engine.clone(), vec!["s1".into()]).unwrap()
}
fn trace_scope(f: &Fixture, id: &str) -> (Arc<dyn agent::tracing::Trace>, Scope) {
    let trace = f.engine.deps.tracer.trace(TraceStart { id: id.into(), name: id.into(), ..Default::default() });
    let scope: Scope = trace.clone();
    (trace, scope)
}
fn execution_context(f: &Fixture, initial: Value) -> (Arc<dyn agent::tracing::Trace>, ExecContext) {
    let (trace, scope) = trace_scope(f, "execution");
    (
        trace,
        ExecContext {
            session: context(),
            store: VariableStore::new(initial.as_object().unwrap().clone()),
            scope,
            signal_label: "scheduler:42".into(),
            cancel: CancellationToken::new(),
        },
    )
}
fn plan(value: Value) -> Workflow {
    serde_json::from_value(json!({"version":1,"steps":value})).unwrap()
}
fn signal() -> WorkflowSignal {
    WorkflowSignal {
        id: 42,
        source: "scheduler".into(),
        content: "scheduled task".into(),
        env_context: Some("delivery to chat 42".into()),
    }
}
#[tokio::test]
async fn compiler_retries_validation_feedback_and_tags_only_accepted_plan() {
    let model = Arc::new(Scripted::new(vec![
        assistant("{broken"),
        assistant("{\"version\":1,\"steps\":[{\"kind\":\"tool\",\"tool\":\"wrong\",\"args\":{}}]}"),
        assistant("{\"version\":1,\"steps\":[{\"kind\":\"terminal\"}]}"),
    ]));
    let f = fixture(model.clone(), &["t1"], SKILLS);
    let runner = runtime(&f);
    let (trace, scope) = trace_scope(&f, "compile");
    let result = runner.compile(&signal(), &context().env, &scope, None, &CancellationToken::new()).await.unwrap();
    assert!(matches!(result, CompileResult::Ok { attempts: 3, .. }));
    trace.end();
    let seen = model.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 3);
    assert!(seen[0].messages[0]["content"].as_str().unwrap().contains("<tools>"));
    assert!(seen[0].messages[0]["content"].as_str().unwrap().contains("set_memory(key: string, value: string)"));
    assert!(!seen[0].messages[0]["content"].as_str().unwrap().contains("scheduled task"));
    assert!(seen[0].messages[1]["content"].as_str().unwrap().contains("delivery to chat 42"));
    assert!(seen[1].messages[3]["content"].as_str().unwrap().contains("invalid JSON"));
    assert!(seen[2].messages[5]["content"].as_str().unwrap().contains("wrong"));
    let (_, obs) = f.db.get_trace("compile").unwrap().unwrap();
    let attempts: Vec<_> = obs.iter().filter(|o| o.name.starts_with("attempt-")).collect();
    assert_eq!(attempts.len(), 3);
    assert!(attempts[0].meta("judge_node").is_none());
    assert!(attempts[1].meta("judge_node").is_none());
    assert_eq!(attempts[2].meta("judge_node"), Some(&json!("planner")));
    f.engine.shutdown().await;
}
#[tokio::test]
async fn compiler_missing_skill_and_api_failure_never_execute_tools() {
    for missing in [true, false] {
        let model = Arc::new(Scripted::new(vec![]));
        let f = fixture(model.clone(), &["t1"], if missing { &[] } else { SKILLS });
        let runner = runtime(&f);
        let (trace, scope) = trace_scope(&f, "compile");
        let result = runner.compile(&signal(), &context().env, &scope, None, &CancellationToken::new()).await.unwrap();
        assert!(matches!(result, CompileResult::Failed { reason: "skill_not_found", attempts: 0, .. }) == missing);
        assert!(f.mcp.called.lock().unwrap().is_empty());
        assert_eq!(model.seen.lock().unwrap().len(), usize::from(!missing));
        trace.end();
        f.engine.shutdown().await;
    }
}
#[tokio::test]
async fn compiler_exhaustion_is_bounded_and_cancellation_is_not_a_fallback() {
    for (response, reason) in [("bad JSON", "invalid_json"), ("{\"version\":1,\"steps\":[]}", "schema_invalid")] {
        let model = Arc::new(Scripted::new(vec![assistant(response); 3]));
        let f = fixture(model.clone(), &["t1"], SKILLS);
        let runner = runtime(&f);
        let (trace, scope) = trace_scope(&f, "compile");
        assert!(
            matches!(runner.compile(&signal(),&context().env,&scope,None,&CancellationToken::new()).await.unwrap(),CompileResult::Failed{reason:r,attempts:3,..} if r==reason)
        );
        trace.end();
        f.engine.shutdown().await;
    }
    let f = fixture(Arc::new(Scripted::new(vec![])), &["t1"], SKILLS);
    let runner = runtime(&f);
    let (trace, scope) = trace_scope(&f, "cancel");
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(runner.compile(&signal(), &context().env, &scope, None, &cancel).await.is_err());
    trace.end();
    f.engine.shutdown().await;
}
#[tokio::test]
async fn tool_binding_parse_rules_set_memory_and_terminal_are_preserved() {
    for raw in ["{\"a\":7}", "[1,2]", "text", "12", "null", " {\"a\":7}", "{broken"] {
        let f = fixture(Arc::new(Scripted::new(vec![])), &["t1"], SKILLS);
        let runner = runtime(&f);
        let (trace, ctx) = execution_context(&f, json!({"raw":raw}));
        let workflow = plan(
            json!([{"kind":"tool","tool":"t1","args":{"output":"${raw}"},"bind":"data"},{"kind":"tool","tool":"set_memory","args":{"key":"watermark","value":"today"},"bind":"ack"},{"kind":"terminal"},{"kind":"tool","tool":"t1","args":{}}]),
        );
        assert!(matches!(runner.execute(&workflow, &ctx).await.unwrap(), ExecResult::Ok { .. }));
        assert_eq!(ctx.store.get("data").unwrap(), parse_tool_json(raw));
        assert_eq!(ctx.store.get("ack").unwrap(), json!({"ok":true,"key":"watermark"}));
        assert_eq!(f.db.get("watermark").unwrap().as_deref(), Some("today"));
        assert_eq!(f.mcp.called.lock().unwrap().len(), 1);
        trace.end();
        let (_, obs) = f.db.get_trace("execution").unwrap().unwrap();
        assert_eq!(obs.iter().find(|o| o.name == "step[0]:tool").unwrap().output, parse_tool_json(raw));
        f.engine.shutdown().await;
    }
}
#[tokio::test]
async fn executor_stops_after_each_failure_and_never_runs_later_side_effects() {
    for (step, reason, initial) in [
        (json!({"kind":"tool","tool":"t1","args":{"q":"${missing}"}}), "missing_binding", json!({})),
        (json!({"kind":"tool","tool":"t1","args":{},"bind":"old"}), "duplicate_binding", json!({"old":42})),
        (json!({"kind":"tool","tool":"t_err","args":{}}), "tool_error", json!({})),
        (json!({"kind":"tool","tool":"t_throw","args":{}}), "tool_error", json!({})),
        (json!({"kind":"tool","tool":"set_memory","args":{"key":"x","value":42}}), "tool_error", json!({})),
        (
            json!({"kind":"llm_compose","preset":"base","skill":"missing","input":{},"bind":"x"}),
            "skill_not_found",
            json!({}),
        ),
        (json!({"kind":"llm_compose","preset":"base","prompt":"q","input":{},"bind":"x"}), "llm_error", json!({})),
        (
            json!({"kind":"decide","preset":"filter","items":"${items}","prompt":"q","bind":"x"}),
            "decision_error",
            json!({"items":[]}),
        ),
        (json!({"kind":"code_agent","task":"q","bind":"x"}), "step_failed", json!({})),
    ] {
        let f = fixture(Arc::new(Scripted::new(vec![])), &["t1", "t_err", "t_throw"], SKILLS);
        let runner = runtime(&f);
        let (trace, ctx) = execution_context(&f, initial);
        let workflow = plan(json!([step,{"kind":"tool","tool":"set_memory","args":{"key":"late","value":"bad"}}]));
        assert!(
            matches!(runner.execute(&workflow,&ctx).await.unwrap(),ExecResult::Failed{reason:r,step_index:0,..} if r==reason),
            "{reason}"
        );
        assert!(f.db.get("late").unwrap().is_none());
        trace.end();
        f.engine.shutdown().await;
    }
}
#[tokio::test]
async fn compose_prepends_composer_and_patch_and_substitutes_structured_input() {
    for output in ["{\"answer\":42}", "[1,2]", "prose", "12", "{broken"] {
        let model = Arc::new(Scripted::new(vec![assistant(output)]));
        let f = fixture(model.clone(), &["t1"], SKILLS);
        std::fs::create_dir(f.root.path().join("skills")).unwrap();
        std::fs::write(f.root.path().join("skills/s1.patch.md"), "LESSON").unwrap();
        let runner = runtime(&f);
        let (trace, ctx) = execution_context(&f, json!({"posts":[{"title":"a"}]}));
        let workflow = plan(
            json!([{"kind":"llm_compose","preset":"base","skill":"s1","prompt":"${posts}","input":{"data":"${posts}"},"bind":"answer"}]),
        );
        assert!(matches!(runner.execute(&workflow, &ctx).await.unwrap(), ExecResult::Ok { .. }));
        let seen = model.seen.lock().unwrap().clone();
        assert!(seen[0].tools.is_empty());
        let system = seen[0].messages[0]["content"].as_str().unwrap();
        assert!(system.starts_with("COMPOSER\n\nDOMAIN"));
        assert!(system.ends_with("<!-- improver-patch -->\nLESSON\n"));
        let user = seen[0].messages[1]["content"].as_str().unwrap();
        assert!(user.starts_with("[\n"));
        assert!(user.contains("<data>\n["));
        assert_eq!(ctx.store.get("answer").unwrap(), parse_tool_json(output));
        trace.end();
        f.engine.shutdown().await;
    }
}
#[tokio::test]
async fn planner_patch_is_appended_after_cached_tools_and_skills() {
    let model = Arc::new(Scripted::new(vec![assistant("{\"version\":1,\"steps\":[{\"kind\":\"terminal\"}]}")]));
    let f = fixture(model.clone(), &["t1"], SKILLS);
    std::fs::create_dir(f.root.path().join("skills")).unwrap();
    std::fs::write(f.root.path().join("skills/planner.patch.md"), "LESSON").unwrap();
    let runner = runtime(&f);
    let (trace, scope) = trace_scope(&f, "compile");
    runner.compile(&signal(), &context().env, &scope, None, &CancellationToken::new()).await.unwrap();
    let seen = model.seen.lock().unwrap().clone();
    let system = seen[0].messages[0]["content"].as_str().unwrap();
    assert!(system.ends_with("</skills>\n\n<!-- improver-patch -->\nLESSON\n"));
    assert_eq!(system.matches("LESSON").count(), 1);
    trace.end();
    f.engine.shutdown().await;
}
#[tokio::test]
async fn workflow_agent_enforces_skill_intersection_and_closes_children_on_failure() {
    let model = Arc::new(Scripted::new(vec![calls(&[("t2", json!({}))]), assistant("child answer")]));
    let f = fixture(model.clone(), &["t1", "t2"], SKILLS);
    let runner = runtime(&f);
    let (trace, ctx) = execution_context(&f, json!({}));
    let workflow = plan(
        json!([{"kind":"llm_agent","preset":"base","skill":"s1","prompt":"task","tools":["t2"],"maxIterations":2,"bind":"child"}]),
    );
    assert!(matches!(runner.execute(&workflow, &ctx).await.unwrap(), ExecResult::Ok { .. }));
    assert_eq!(ctx.store.get("child").unwrap(), "child answer");
    assert!(f.mcp.called.lock().unwrap().is_empty());
    assert!(
        !model.seen.lock().unwrap()[0]
            .tools
            .iter()
            .any(|t| ["t1", "t2", "invoke_sub_agent"].contains(&t.name.as_str()))
    );
    trace.end();
    f.engine.shutdown().await;
    let f = fixture(Arc::new(Scripted::new(vec![])), &["t1"], SKILLS);
    let runner = runtime(&f);
    let (trace, ctx) = execution_context(&f, json!({}));
    assert!(matches!(runner.execute(&workflow, &ctx).await.unwrap(), ExecResult::Failed { .. }));
    let reopened =
        f.engine.start_loop(agent::agent::LoopOptions::new("scheduler:42__agent:child", context())).await.unwrap();
    reopened.close().await;
    trace.end();
    f.engine.shutdown().await;
}
#[tokio::test]
async fn parallel_failure_cancels_sibling_prevents_late_bindings_and_drains_finalizers() {
    let f = fixture(Arc::new(Scripted::new(vec![])), &["t1", "t_err"], SKILLS);
    let runner = runtime(&f);
    let (trace, ctx) = execution_context(&f, json!({}));
    let workflow = plan(
        json!([{"kind":"parallel","steps":[{"kind":"tool","tool":"t1","args":{"delay":60000},"bind":"late"},{"kind":"tool","tool":"t_err","args":{},"bind":"failed"}]},{"kind":"tool","tool":"set_memory","args":{"key":"later","value":"bad"}}]),
    );
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), runner.execute(&workflow, &ctx))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(result, ExecResult::Failed { reason: "tool_error", .. }));
    assert!(!ctx.store.has("late"));
    assert_eq!(f.mcp.active.load(Ordering::SeqCst), 0);
    assert!(f.db.get("later").unwrap().is_none());
    trace.end();
    let (_, obs) = f.db.get_trace("execution").unwrap().unwrap();
    assert_eq!(obs.iter().filter(|o| o.name.starts_with("parallel[") && o.level == "ERROR").count(), 2);
    f.engine.shutdown().await;
}
#[tokio::test]
async fn replan_carries_exact_bindings_as_context_and_never_replays_gather() {
    let gather = json!({"version":1,"steps":[{"kind":"tool","tool":"t1","args":{"output":"{\"title\":\"data\"}"},"bind":"gathered"},{"kind":"replan","context":["gathered","missing"],"note":"act now"}]});
    let acting = json!({"version":1,"steps":[{"kind":"tool","tool":"set_memory","args":{"key":"result","value":"${context.gathered.title}"}},{"kind":"terminal"}]});
    let model = Arc::new(Scripted::new(vec![assistant(&gather.to_string()), assistant(&acting.to_string())]));
    let f = fixture(model.clone(), &["t1"], SKILLS);
    let runner = runtime(&f);
    let (trace, scope) = trace_scope(&f, "run");
    assert!(matches!(
        runner.run_signal(&signal(), context(), scope, &CancellationToken::new()).await.unwrap(),
        RunResult::Ok { .. }
    ));
    assert_eq!(f.db.get("result").unwrap().as_deref(), Some("data"));
    assert_eq!(f.mcp.called.lock().unwrap().len(), 1);
    let seen = model.seen.lock().unwrap().clone();
    let user = seen[1].messages[1]["content"].as_str().unwrap();
    assert!(user.contains("act now"));
    assert!(user.contains("replan pass 1"));
    assert!(user.contains("\"gathered\""));
    assert!(!user.contains("\"missing\""));
    trace.end();
    f.engine.shutdown().await;
}
#[tokio::test]
async fn runner_replan_exhaustion_and_execute_failure_have_distinct_stages() {
    let model = Arc::new(Scripted::new(vec![
        assistant(
            "{\"version\":1,\"steps\":[{\"kind\":\"replan\",\"context\":[\"missing\"]}]}"
        );
        3
    ]));
    let f = fixture(model.clone(), &["t1"], SKILLS);
    let runner = runtime(&f);
    let (trace, scope) = trace_scope(&f, "run");
    assert!(matches!(
        runner.run_signal(&signal(), context(), scope, &CancellationToken::new()).await.unwrap(),
        RunResult::ReplanExhausted { passes: 3, .. }
    ));
    assert!(model.seen.lock().unwrap()[2].messages[1]["content"].as_str().unwrap().contains("LAST pass"));
    trace.end();
    f.engine.shutdown().await;
}
struct Code(AtomicUsize);
#[async_trait]
impl Codex for Code {
    async fn run(&self, request: Value, _: &CancellationToken) -> anyhow::Result<String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        assert_eq!(request["input"], "data");
        assert!(request["prompt"].as_str().unwrap().ends_with("Task:\ncalculate 7"));
        Ok(" 42\n".into())
    }
}
#[tokio::test]
async fn code_step_substitutes_stdin_and_binds_trimmed_result() {
    let code = Arc::new(Code(AtomicUsize::new(0)));
    let mut f = fixture(Arc::new(Scripted::new(vec![])), &["t1"], SKILLS);
    Arc::get_mut(&mut f.engine).unwrap().deps.codex = code.clone();
    let runner = runtime(&f);
    let (trace, ctx) = execution_context(&f, json!({"n":7}));
    let workflow = plan(json!([{"kind":"code_agent","task":"calculate ${n}","data":"data","bind":"answer"}]));
    assert!(matches!(runner.execute(&workflow, &ctx).await.unwrap(), ExecResult::Ok { .. }));
    assert_eq!(ctx.store.get("answer").unwrap(), "42");
    assert_eq!(code.0.load(Ordering::SeqCst), 1);
    trace.end();
    f.engine.shutdown().await;
}
struct Decisions;
#[async_trait]
impl JsonApi for Decisions {
    async fn post(&self, _: &str, body: Value, _: &CancellationToken) -> anyhow::Result<Value> {
        let answers: serde_json::Map<_, _> = body["questions"]
            .as_object()
            .unwrap()
            .keys()
            .enumerate()
            .map(|(i, k)| (k.clone(), json!({"type":"noul","noul":if i==0{0.8}else{0.1}})))
            .collect();
        Ok(json!({"model":"jev","answers":answers}))
    }
}
#[tokio::test]
async fn decide_filters_array_without_chat_and_records_counts() {
    let mut f = fixture(Arc::new(Scripted::new(vec![])), &["t1"], SKILLS);
    Arc::get_mut(&mut f.engine).unwrap().deps.decisions = Some(decision_client("jev".into(), Arc::new(Decisions)));
    let runner = runtime(&f);
    let (trace, ctx) = execution_context(&f, json!({"items":["a","b"]}));
    let workflow = plan(
        json!([{"kind":"decide","preset":"filter","items":"${items}","prompt":"rules","threshold":0.5,"bind":"kept"}]),
    );
    assert!(matches!(runner.execute(&workflow, &ctx).await.unwrap(), ExecResult::Ok { .. }));
    assert_eq!(ctx.store.get("kept").unwrap(), json!(["a"]));
    trace.end();
    let (_, obs) = f.db.get_trace("execution").unwrap().unwrap();
    assert_eq!(obs.iter().find(|o| o.name == "step[0]:decide").unwrap().output, json!({"kept":1,"of":2}));
    f.engine.shutdown().await;
}
