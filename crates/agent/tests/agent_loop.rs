// Port of src/agent-loop.test.ts and test/agent-loop.test.ts: the observable
// protocol of a real loop. Scripted model replies exercise the entire runtime.
mod support;
use agent::{
    agent::LoopOptions,
    db::{MemoryStore, TraceStore},
    providers::assistant,
    skills::SkillStore,
};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use support::*;

fn text(reply: &str) -> String {
    let value: Value = serde_json::from_str(reply).unwrap();
    value["content"].as_str().unwrap_or_default().to_owned()
}
#[tokio::test]
async fn whitelist_intersects_skill_tools_and_blocks_hallucinated_calls() {
    for (fm, whitelist, expected) in [
        ("*", Some(vec!["a"]), vec!["a"]),
        ("[a, b]", Some(vec!["b", "c"]), vec!["b"]),
        ("[a]", None, vec!["a"]),
        ("[]", None, vec![]),
    ] {
        let model =
            Arc::new(Scripted::new(vec![calls(&[("c", json!({})), ("missing", json!({}))]), assistant("done")]));
        let body = format!("---\ntools: {fm}\n---\n# domain\n");
        let f = fixture(model.clone(), &["a", "b", "c"], &[("domain", &body)]);
        let mut opts = LoopOptions::new("loop", context());
        opts.skills = vec!["domain".into()];
        opts.tool_whitelist = whitelist.map(|v| v.into_iter().map(str::to_owned).collect::<HashSet<_>>());
        let loop_ = f.engine.start_loop(opts).await.unwrap();
        assert_eq!(loop_.send("go".into()).await.unwrap(), "done");
        let seen = model.seen.lock().unwrap().clone();
        let offered: Vec<_> = seen[0]
            .tools
            .iter()
            .filter(|t| ["a", "b", "c"].contains(&t.name.as_str()))
            .map(|t| t.name.as_str())
            .collect();
        assert_eq!(offered, expected);
        assert!(f.mcp.called.lock().unwrap().is_empty());
        let replies: Vec<_> = seen[1].messages.iter().filter(|m| m["role"] == "tool").collect();
        assert!(replies.iter().all(|m| text(m["content"].as_str().unwrap()).contains("unavailable")));
        f.engine.shutdown().await;
    }
}
#[tokio::test]
async fn bounded_tools_keep_call_order_and_trace_raw_outputs() {
    let model = Arc::new(Scripted::new(vec![
        calls(&[
            ("echo", json!({"output":"first","delay":40})),
            ("echo", json!({"output":"second","delay":1})),
            ("echo", json!({"output":"third","delay":2})),
        ]),
        assistant("done"),
    ]));
    let f = fixture(model.clone(), &["echo"], &[("worker", "---\ntools: *\n---\nworker\n")]);
    let mut opts = LoopOptions::new("loop", context());
    opts.skills = vec!["worker".into()];
    opts.max_concurrent_tools = 2;
    let loop_ = f.engine.start_loop(opts).await.unwrap();
    loop_.send("go".into()).await.unwrap();
    let replies: Vec<_> = loop_
        .messages
        .lock()
        .await
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| text(m["content"].as_str().unwrap()))
        .collect();
    assert_eq!(replies, ["first", "second", "third"]);
    assert_eq!(f.mcp.peak.load(Ordering::SeqCst), 2);
    let (_, observations) = f.db.get_trace("loop").unwrap().unwrap();
    let tools: Vec<_> = observations.iter().filter(|o| o.name == "echo").collect();
    assert_eq!(tools.len(), 3);
    for t in tools {
        assert_eq!(t.metadata.as_ref().unwrap()["tool"], "echo");
        assert!(t.metadata.as_ref().unwrap().get("memory_key").is_some());
        assert!(t.output.is_string());
    }
    f.engine.shutdown().await;
}
#[tokio::test]
async fn working_memory_reads_stay_full_and_persistent_state_is_separate() {
    let big = "я🦊".repeat(2000);
    let model = Arc::new(Scripted::new(vec![
        calls(&[
            ("working_memory_put", json!({"key":"data","value":big,"format":"json"})),
            ("set_memory", json!({"key":"watermark","value":"today"})),
        ]),
        calls(&[("working_memory_get", json!({"key":"data"})), ("working_memory_list", json!({}))]),
        assistant("done"),
    ]));
    let f = fixture(model.clone(), &[], &[]);
    let context = context();
    let loop_ = f.engine.start_loop(LoopOptions::new("loop", context.clone())).await.unwrap();
    loop_.send("go".into()).await.unwrap();
    assert_eq!(context.memory.get("data").unwrap(), big);
    assert_eq!(f.db.get("watermark").unwrap().as_deref(), Some("today"));
    let seen = model.seen.lock().unwrap().clone();
    let replies: Vec<_> = seen[2].messages.iter().filter(|m| m["role"] == "tool").collect();
    assert_eq!(replies[2]["content"], big);
    // put/get/list are inline: only set_memory is itself saved.
    assert_eq!(context.memory.list().len(), 2);
    drop(seen);
    f.engine.shutdown().await;
}
#[tokio::test]
async fn workers_receive_refs_and_isolated_history_and_cannot_delegate() {
    let model = Arc::new(Scripted::new(vec![
        calls(&[("invoke_sub_agent", json!({"prompt":"process","input_refs":["raw"]}))]),
        calls(&[("invoke_sub_agent", json!({"prompt":"recurse"}))]),
        assistant("child result"),
        assistant("parent result"),
    ]));
    let f = fixture(model.clone(), &[], &[("worker", "---\ntools: *\n---\nworker\n")]);
    let context = context();
    context.memory.put("raw", "secret ${path}", agent::session::Format::Text).unwrap();
    let loop_ = f.engine.start_loop(LoopOptions::new("parent", context)).await.unwrap();
    assert_eq!(loop_.send("parent input".into()).await.unwrap(), "parent result");
    let seen = model.seen.lock().unwrap().clone();
    assert!(seen[0].tools.iter().any(|t| t.name == "invoke_sub_agent"));
    assert!(!seen[1].tools.iter().any(|t| t.name == "invoke_sub_agent"));
    assert!(seen[1].messages.iter().any(|m| m["content"].as_str().is_some_and(|s| s.contains("secret ${path}"))));
    assert!(!seen[1].messages.iter().any(|m| m["content"] == "parent input"));
    assert!(
        seen[2]
            .messages
            .iter()
            .filter(|m| m["role"] == "tool")
            .any(|m| text(m["content"].as_str().unwrap()).contains("unavailable"))
    );
    drop(seen);
    let (_, obs) = f.db.get_trace("parent").unwrap().unwrap();
    let worker = obs.iter().find(|o| o.name == "invoke_sub_agent").unwrap();
    assert!(obs.iter().any(|o| o.name == "iter-0" && o.parent_observation_id.as_deref() == Some(&worker.id)));
    f.engine.shutdown().await;
}
#[tokio::test]
async fn skill_write_authorization_cannot_be_created_by_model_arguments() {
    for authorized in [false, true] {
        let content = "---\ntools: []\n---\nrevised\n";
        let model = Arc::new(Scripted::new(vec![
            calls(&[("invoke_sub_agent", json!({"skills":["dreaming"],"prompt":"rewrite","allowSkillWrites":true}))]),
            calls(&[("write_skill", json!({"name":"telegram","content":content}))]),
            assistant("child"),
            assistant("parent"),
        ]));
        let f = fixture(
            model.clone(),
            &[],
            &[("dreaming", "---\ntools: []\n---\ndreaming\n"), ("telegram", "---\ntools: []\n---\noriginal\n")],
        );
        let mut opts = LoopOptions::new("parent", context());
        opts.skills = vec!["telegram".into()];
        opts.allow_skill_writes = authorized;
        let loop_ = f.engine.start_loop(opts).await.unwrap();
        loop_.send("rewrite".into()).await.unwrap();
        assert_eq!(model.seen.lock().unwrap()[1].tools.iter().any(|t| t.name == "write_skill"), authorized);
        assert_eq!(
            f.skills.read_skill("telegram").await.unwrap().unwrap().body,
            if authorized { "revised\n" } else { "original\n" }
        );
        f.engine.shutdown().await;
    }
}
#[tokio::test]
async fn writes_reject_broken_frontmatter_unknown_tools_and_escaping_names() {
    for (name, body) in [
        ("telegram", "no frontmatter"),
        ("telegram", "---\ntools: [unknown]\n---\ntext\n"),
        ("../escape", "---\ntools: []\n---\ntext\n"),
    ] {
        let model = Arc::new(Scripted::new(vec![
            calls(&[("write_skill", json!({"name":name,"content":body}))]),
            assistant("done"),
        ]));
        let f = fixture(model.clone(), &[], &[("dreaming", "---\ntools: []\n---\ndreaming\n")]);
        let mut opts = LoopOptions::new("loop", context());
        opts.skills = vec!["dreaming".into()];
        opts.allow_skill_writes = true;
        let loop_ = f.engine.start_loop(opts).await.unwrap();
        loop_.send("go".into()).await.unwrap();
        assert!(!f.root.path().join("skills/telegram.md").exists());
        let (_, obs) = f.db.get_trace("loop").unwrap().unwrap();
        let write = obs.iter().find(|o| o.name == "write_skill").unwrap();
        assert_eq!(write.level, "ERROR");
        assert!(write.output.as_str().unwrap().starts_with("[write_skill error]"));
        f.engine.shutdown().await;
    }
}
#[tokio::test]
async fn malformed_arguments_still_leave_error_spans_and_never_call_mcp() {
    for args in ["{broken", "null", "[]", "42"] {
        let message=json!({"role":"assistant","content":null,"tool_calls":[{"id":"bad","type":"function","function":{"name":"echo","arguments":args}}]}).as_object().unwrap().clone();
        let model = Arc::new(Scripted::new(vec![message, assistant("done")]));
        let f = fixture(model.clone(), &["echo"], &[]);
        let loop_ = f.engine.start_loop(LoopOptions::new("loop", context())).await.unwrap();
        loop_.send("go".into()).await.unwrap();
        assert!(f.mcp.called.lock().unwrap().is_empty());
        let (_, obs) = f.db.get_trace("loop").unwrap().unwrap();
        let span = obs.iter().find(|o| o.name == "echo").unwrap();
        assert_eq!(span.level, "ERROR");
        assert_eq!(span.input["raw_arguments"], args);
        f.engine.shutdown().await;
    }
}
#[tokio::test]
async fn budget_duplicate_ids_and_missing_skills_fail_loudly() {
    let model = Arc::new(Scripted::new(vec![calls(&[("working_memory_list", json!({}))])]));
    let f = fixture(model, &[], &[]);
    let mut missing = LoopOptions::new("missing", context());
    missing.skills = vec!["no-such-skill".into()];
    assert!(f.engine.start_loop(missing).await.is_err());
    let mut opts = LoopOptions::new("loop", context());
    opts.max_iterations = 1;
    let loop_ = f.engine.start_loop(opts.clone()).await.unwrap();
    assert!(f.engine.start_loop(opts).await.is_err());
    assert!(loop_.send("go".into()).await.unwrap_err().to_string().contains("maxIterations=1"));
    f.engine.end_loop("loop").await;
    assert!(loop_.run().await.is_err());
    f.engine.shutdown().await;
    f.engine.shutdown().await;
    assert_eq!(f.mcp.closed.load(Ordering::SeqCst), 1);
    assert!(f.engine.start_loop(LoopOptions::new("later", context())).await.is_err());
}
#[tokio::test]
async fn close_cancels_active_round_joins_tools_and_closes_trace_once() {
    let model =
        Arc::new(Scripted::new(vec![calls(&[("wait", json!({"delay":60000})), ("wait", json!({"delay":60000}))])]));
    let f = fixture(model, &["wait"], &[("worker", "---\ntools: *\n---\nworker\n")]);
    let mut opts = LoopOptions::new("loop", context());
    opts.skills = vec!["worker".into()];
    let loop_ = f.engine.start_loop(opts).await.unwrap();
    let running = tokio::spawn(loop_.send("go".into()));
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while f.mcp.active.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(loop_.run().await.unwrap_err().to_string().contains("already running"));
    loop_.close().await;
    assert!(running.await.unwrap().is_err());
    assert_eq!(f.mcp.active.load(Ordering::SeqCst), 0);
    let (trace, obs) = f.db.get_trace("loop").unwrap().unwrap();
    assert!(trace.output.get("error").is_some());
    assert_eq!(obs.iter().filter(|o| o.name == "wait" && o.level == "ERROR").count(), 2);
    f.engine.shutdown().await;
}
