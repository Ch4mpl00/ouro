// One-to-one port of test/agent-loop.test.ts: generated failure sequences and
// model turns, against the runtime, with a plain expected conversation model.
mod support;
use agent::{
    agent::LoopOptions,
    providers::{ApiError, Message, RetryOpts, assistant, retry_on_transient, role, tool_calls},
};
use proptest::prelude::*;
use proptest::test_runner::{TestCaseError, TestCaseResult};
use serde_json::{Map, Value, json};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
#[derive(Clone, Debug)]
enum Turn {
    Answer(String),
    Calls(Vec<(String, String)>),
}
fn turn() -> impl Strategy<Value = Turn> {
    let name = prop::sample::select(vec!["t_ok", "t_ok", "t_ok", "t_err", "t_throw", "t_hidden", "no_such_tool"]);
    let args = prop_oneof![6=>".{0,20}".prop_map(|q|json!({"q":q}).to_string()),1=>prop::sample::select(vec!["","{}","{broken","null","[1,2]","\"text\""]).prop_map(str::to_owned)];
    prop_oneof![3=>prop::collection::vec((name,args),1..5).prop_map(|calls|Turn::Calls(calls.into_iter().map(|(n,a)|(n.into(),a)).collect())),1=>".{0,20}".prop_map(Turn::Answer)]
}
fn message(turn: &Turn, at: usize) -> Message {
    match turn{
    Turn::Answer(text)=>assistant(text),
    Turn::Calls(calls)=>json!({"role":"assistant","content":null,"tool_calls":calls.iter().enumerate().map(|(i,(name,args))|json!({"id":format!("call_{at}_{i}"),"type":"function","function":{"name":name,"arguments":args}})).collect::<Vec<_>>()}).as_object().unwrap().clone(),
}
}
fn args(raw: &str) -> Option<Map<String, Value>> {
    serde_json::from_str::<Value>(if raw.is_empty() { "{}" } else { raw }).ok().and_then(|v| v.as_object().cloned())
}
async fn exercise(script: Vec<Turn>, max: usize) -> TestCaseResult {
    let replies: Vec<_> = script.iter().enumerate().map(|(i, t)| message(t, i)).collect();
    let model = Arc::new(support::Scripted::new(replies.clone()));
    let f = support::fixture(
        model.clone(),
        &["t_ok", "t_err", "t_throw", "t_hidden"],
        &[("s", "---\ntools: *\n---\nskill\n")],
    );
    let mut options = LoopOptions::new("loop", support::context());
    options.skills = vec!["s".into()];
    options.tool_whitelist = Some(["t_ok", "t_err", "t_throw"].into_iter().map(str::to_owned).collect());
    options.max_iterations = max;
    let loop_ = f.engine.start_loop(options).await.map_err(|e| TestCaseError::fail(e.to_string()))?;
    let settled = loop_.send("start".into()).await;
    f.engine.end_loop("loop").await;
    f.engine.shutdown().await;
    let seen = model.seen.lock().unwrap().clone();
    let dispatched = f.mcp.called.lock().unwrap().clone();
    let answer_at = script.iter().position(|t| matches!(t, Turn::Answer(_)));
    let want = match answer_at {
        Some(at) if at < max => at + 1,
        _ => max.min(script.len() + 1),
    };
    prop_assert_eq!(seen.len(), want);
    if let Some(at) = answer_at.filter(|at| *at < max) {
        let Turn::Answer(text) = &script[at] else { unreachable!() };
        let answer = settled.ok();
        prop_assert_eq!(answer.as_ref(), Some(text));
    } else {
        prop_assert!(settled.is_err());
    }
    for (k, request) in seen.iter().enumerate() {
        for (i, m) in request.messages.iter().enumerate() {
            let calls = tool_calls(m);
            if role(m) != "assistant" || calls.is_empty() {
                continue;
            }
            let answered = &request.messages[i + 1..i + 1 + calls.len()];
            prop_assert!(answered.iter().all(|m| role(m) == "tool"));
            prop_assert_eq!(
                answered.iter().map(|m| m["tool_call_id"].as_str().unwrap()).collect::<Vec<_>>(),
                calls.iter().map(|c| c.id.as_str()).collect::<Vec<_>>()
            );
        }
        if let Some(next) = seen.get(k + 1) {
            prop_assert_eq!(&next.messages[..request.messages.len()], request.messages.as_slice());
            prop_assert_eq!(&next.messages[request.messages.len()], &replies[k]);
        }
    }
    let want_dispatch: Vec<_> = replies
        .iter()
        .take(seen.len().min(replies.len()))
        .flat_map(tool_calls)
        .filter_map(|c| {
            if ["t_ok", "t_err", "t_throw"].contains(&c.name.as_str()) {
                args(&c.arguments).map(|a| (c.name, a))
            } else {
                None
            }
        })
        .collect();
    prop_assert_eq!(dispatched, want_dispatch);
    Ok(())
}
proptest! {
    #![proptest_config(ProptestConfig::with_cases(150))]
    #[test]
    fn conversation_is_append_only_and_dispatches_exactly_allowed_object_calls(script in prop::collection::vec(turn(),1..7),max in 1usize..6){
        let runtime=tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(exercise(script,max))?;
    }
    #[test]
    fn retries_exactly_the_transient_failures_within_budget(script in prop::collection::vec(0u8..4,6..7),max in 0u32..5){
        let runtime=tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let stop=script.iter().position(|n|*n!=1).map(|i|i+1).unwrap_or(usize::MAX).min(max as usize+1);
        let mut calls=0;let mut events=Vec::new();
        let result=runtime.block_on(retry_on_transient(&RetryOpts{max_retries:max,base_delay_ms:1,max_delay_ms:4,jitter:false},&CancellationToken::new(),|e|events.push(e.clone()),||{let i=calls;calls+=1;let outcome=script[i];async move{match outcome{0=>Ok(i),1=>Err(ApiError::Status{status:503,message:"offline".into()}.into()),2=>Err(ApiError::Status{status:400,message:"permanent".into()}.into()),_=>Err(anyhow::anyhow!("plain error"))}}}));
        prop_assert_eq!(calls,stop);prop_assert_eq!(result.is_ok(),script[stop-1]==0);
        prop_assert_eq!(events.iter().map(|e|e.attempt).collect::<Vec<_>>(),(1..stop as u32).collect::<Vec<_>>());
        prop_assert_eq!(events.iter().map(|e|e.delay_ms).collect::<Vec<_>>(),(0..stop-1).map(|i|(1u64<<i).min(4)).collect::<Vec<_>>());
    }
}
