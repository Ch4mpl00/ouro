// Port of tracing/tee.test.ts, plus OTLP/local read-shape checks.
use agent::{
    db::{AgentDb, TraceStore},
    langfuse::{Attr, FinishedSpan, LangfuseTracer, SpanSink, otlp_body},
    local_recorder::local_recorder_tracer,
    tracing::*,
};
use serde_json::json;
use std::sync::{Arc, Mutex};
#[derive(Default)]
struct Sink(Mutex<Vec<FinishedSpan>>);
impl SpanSink for Sink {
    fn finished(&self, span: FinishedSpan) {
        self.0.lock().unwrap().push(span);
    }
}
#[tokio::test]
async fn tee_forces_primary_ids_and_records_nested_spans_usage_and_merged_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(AgentDb::open(dir.path().join("agent.db")).unwrap());
    let sink = Arc::new(Sink::default());
    let tracer =
        tee_tracer(Arc::new(LangfuseTracer::with_sink(sink.clone())), Arc::new(local_recorder_tracer(db.clone())));
    let trace = tracer.trace(TraceStart {
        id: "ignored".into(),
        name: "signal:telegram".into(),
        kind: SpanKind::Agent,
        session_id: Some("telegram:42".into()),
        tags: Some(vec!["telegram".into()]),
        metadata: Some(json!({"skills":["telegram"],"signal_source":"telegram"}).as_object().unwrap().clone()),
    });
    trace.update(ContextUpdate::input("user input"));
    let generation =
        trace.generation(GenerationStart { name: "attempt-1".into(), model: "m".into(), ..Default::default() });
    let span = trace.span(
        SpanStart::new("step[0]:tool", SpanKind::Tool).metadata(json!({"tool":"echo"}).as_object().unwrap().clone()),
    );
    let inner = span.generation(GenerationStart {
        name: "llm_compose:x".into(),
        model: "m".into(),
        metadata: Some(json!({"skill":"digest"}).as_object().unwrap().clone()),
        ..Default::default()
    });
    generation.end(GenerationEnd {
        output: Some(json!("{}")),
        usage: Some(TokenUsage { input: 100, output: 20, total: 120, cached: Some(64) }),
        ..Default::default()
    });
    inner.end(GenerationEnd {
        output: Some(json!("ok")),
        metadata: Some(json!({"judge_node":"compose"}).as_object().unwrap().clone()),
        ..Default::default()
    });
    span.end(SpanEnd::output("ok"));
    trace.update(ContextUpdate::output("answer"));
    trace.end();
    let (stored, obs) = db.get_trace(&trace.id()).unwrap().unwrap();
    assert_eq!(stored.id, trace.id());
    assert_eq!(stored.session_id.as_deref(), Some("telegram:42"));
    assert_eq!(stored.input, "user input");
    assert_eq!(stored.output, "answer");
    for (name, id) in [("attempt-1", generation.id()), ("step[0]:tool", span.id()), ("llm_compose:x", inner.id())] {
        assert_eq!(obs.iter().find(|o| o.name == name).unwrap().id, id);
    }
    let inner_obs = obs.iter().find(|o| o.name == "llm_compose:x").unwrap();
    assert_eq!(inner_obs.parent_observation_id.as_deref(), Some(span.id().as_str()));
    assert_eq!(inner_obs.metadata.as_ref().unwrap()["judge_node"], "compose");
    assert_eq!(obs.iter().find(|o| o.name == "attempt-1").unwrap().usage_details.as_ref().unwrap()["cached"], 64);
    let wire = sink.0.lock().unwrap().clone();
    assert_eq!(wire.len(), 4);
    let root = wire.iter().find(|s| s.parent_span_id.is_none()).unwrap();
    assert_eq!(root.trace_id, stored.id);
    assert!(root.attributes.iter().any(|(k, v)| k == "session.id" && *v == Attr::Str("telegram:42".into())));
    let body = otlp_body(&wire);
    assert_eq!(body["resourceSpans"][0]["scopeSpans"][0]["scope"]["name"], "langfuse-sdk");
    assert_eq!(body["resourceSpans"][0]["scopeSpans"][0]["spans"].as_array().unwrap().len(), 4);
    assert!(body.to_string().contains("langfuse.observation.usage_details"));
    drop(wire);
    tracer.shutdown().await;
}
#[tokio::test]
async fn error_and_retry_event_attributes_match_local_records() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(AgentDb::open(dir.path().join("agent.db")).unwrap());
    let tracer = local_recorder_tracer(db.clone());
    let trace = tracer.trace(TraceStart { id: "error".into(), name: "error".into(), ..Default::default() });
    trace.event(EventStart {
        name: "llm_retry".into(),
        level: Some(Level::Warning),
        metadata: Some(json!({"attempt":1}).as_object().unwrap().clone()),
        ..Default::default()
    });
    let span = trace.span(SpanStart::new("tool", SpanKind::Tool));
    span.end(SpanEnd::error("failure").with_output("[tool error] failure"));
    trace.end();
    let (_, obs) = db.get_trace("error").unwrap().unwrap();
    assert_eq!(obs.iter().find(|o| o.name == "llm_retry").unwrap().level, "WARNING");
    let tool = obs.iter().find(|o| o.name == "tool").unwrap();
    assert_eq!(tool.level, "ERROR");
    assert_eq!(tool.status_message.as_deref(), Some("failure"));
    assert_eq!(tool.output, "[tool error] failure");
}
