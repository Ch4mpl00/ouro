// Port of src/providers/retry.test.ts: retry budget, backoff and cancellation.
mod support;
use agent::{
    db::TraceStore,
    generation::{Observation, describe_completion, generation},
    models::ReasoningEffort,
    providers::*,
};
use async_trait::async_trait;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
struct Flaky {
    failures: Vec<u16>,
    calls: AtomicUsize,
    signals: Mutex<Vec<CancellationToken>>,
}
#[async_trait]
impl ChatProvider for Flaky {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Openai
    }
    async fn complete(&self, p: &mut CompletionParams<'_>) -> anyhow::Result<CompletionResult> {
        self.signals.lock().unwrap().push(p.cancel.clone());
        let at = self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(status) = self.failures.get(at) {
            return Err(ApiError::Status { status: *status, message: "offline".into() }.into());
        }
        Ok(CompletionResult { message: assistant("ok"), finish_reason: Some("stop".into()), usage: None })
    }
}
fn flaky(failures: Vec<u16>) -> Arc<Flaky> {
    Arc::new(Flaky { failures, calls: AtomicUsize::new(0), signals: Default::default() })
}
fn params<'a>(messages: &'a mut Vec<Message>, cancel: CancellationToken) -> CompletionParams<'a> {
    CompletionParams {
        model: "test".into(),
        messages,
        reasoning_effort: ReasoningEffort::Disabled,
        tools: Vec::new(),
        response_format: None,
        trace: None,
        cancel,
    }
}
#[tokio::test]
async fn retry_only_transient_errors_and_observe_each_attempt() {
    let inner = flaky(vec![429, 503]);
    let provider: SharedProvider =
        Arc::new(with_retry(inner.clone(), RetryOpts { base_delay_ms: 0, jitter: false, ..Default::default() }));
    let f = support::fixture(provider.clone(), &[], &[]);
    let scope = agent::tracing::Tracer::trace(
        f.engine.deps.tracer.as_ref(),
        agent::tracing::TraceStart { id: "retry".into(), name: "retry".into(), ..Default::default() },
    );
    let ctx: agent::tracing::Scope = scope.clone();
    let result = generation(
        &provider,
        &mut params(&mut Vec::new(), CancellationToken::new()),
        &ctx,
        Observation { name: "call".into(), ..Default::default() },
        None,
        describe_completion,
    )
    .await
    .unwrap();
    assert_eq!(result.message["content"], "ok");
    assert_eq!(inner.calls.load(Ordering::SeqCst), 3);
    scope.end();
    let (_, obs) = f.db.get_trace("retry").unwrap().unwrap();
    let retries: Vec<_> = obs.iter().filter(|o| o.name == "llm_retry").collect();
    assert_eq!(retries.len(), 2);
    assert!(retries.iter().all(|o| o.level == "WARNING"));
    assert_eq!(retries[0].metadata.as_ref().unwrap()["attempt"], 1);
    assert_eq!(retries[1].metadata.as_ref().unwrap()["status"], 503);
    f.engine.shutdown().await;
}
#[tokio::test]
async fn permanent_failures_and_exhaustion() {
    for (failures, max_retries, expected) in [(vec![400], 4, 1), (vec![429, 429, 429], 2, 3)] {
        let inner = flaky(failures);
        let p =
            with_retry(inner.clone(), RetryOpts { max_retries, base_delay_ms: 0, jitter: false, ..Default::default() });
        assert!(p.complete(&mut params(&mut Vec::new(), CancellationToken::new())).await.is_err());
        assert_eq!(inner.calls.load(Ordering::SeqCst), expected);
    }
    for status in [408, 429, 500, 503] {
        assert!(is_transient(&ApiError::Status { status, message: "bad".into() }.into()));
    }
    for status in [400, 401, 403, 404] {
        assert!(!is_transient(&ApiError::Status { status, message: "bad".into() }.into()));
    }
    assert!(is_transient(&ApiError::Connection("reset".into()).into()));
    assert!(!is_transient(&anyhow::anyhow!("programming error")));
    let opts = RetryOpts { base_delay_ms: 1000, max_delay_ms: 3000, jitter: false, ..Default::default() };
    assert_eq!((1..6).map(|i| retry_delay(&opts, i)).collect::<Vec<_>>(), [1000, 2000, 3000, 3000, 3000]);
}
#[tokio::test]
async fn deadline_covers_backoff_and_signals_actual_provider() {
    let inner = flaky(vec![503, 503]);
    let p: SharedProvider =
        Arc::new(with_retry(inner.clone(), RetryOpts { base_delay_ms: 10000, jitter: false, ..Default::default() }));
    let f = support::fixture(p.clone(), &[], &[]);
    let trace = f.engine.deps.tracer.trace(agent::tracing::TraceStart {
        id: "deadline".into(),
        name: "deadline".into(),
        ..Default::default()
    });
    let scope: agent::tracing::Scope = trace.clone();
    let error = generation(
        &p,
        &mut params(&mut Vec::new(), CancellationToken::new()),
        &scope,
        Observation { name: "call".into(), ..Default::default() },
        Some(Duration::from_millis(10)),
        describe_completion,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("Generation timed out after 10ms"));
    assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
    assert!(inner.signals.lock().unwrap()[0].is_cancelled());
    trace.end();
    let (_, obs) = f.db.get_trace("deadline").unwrap().unwrap();
    let generation = obs.iter().find(|o| o.name == "call").unwrap();
    assert_eq!(generation.level, "ERROR");
    assert_eq!(generation.output, json!({"error":"Generation timed out after 10ms"}));
    f.engine.shutdown().await;
}
#[tokio::test]
async fn cancellation_stops_backoff_without_another_request() {
    let inner = flaky(vec![503, 503]);
    let provider =
        Arc::new(with_retry(inner.clone(), RetryOpts { base_delay_ms: 60000, jitter: false, ..Default::default() }));
    let cancel = CancellationToken::new();
    let task_cancel = cancel.clone();
    let task = tokio::spawn(async move { provider.complete(&mut params(&mut Vec::new(), task_cancel)).await });
    while inner.calls.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    cancel.cancel();
    assert!(task.await.unwrap().is_err());
    assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
}
