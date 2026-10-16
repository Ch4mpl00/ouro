// Port of src/providers/retry.test.ts, one test per case. Effect's TestClock
// becomes tokio's paused clock.
mod support;
use agent::{
    db::TraceStore,
    generation::{Observation, describe_completion, generation},
    models::ReasoningEffort,
    providers::*,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

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

// Lets spawned tasks run up to their next await on the paused clock.
async fn settle() {
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn uses_exponential_delays_and_stops_exactly_at_the_retry_budget() {
    let calls = Arc::new(AtomicUsize::new(0));
    let events = Arc::new(Mutex::new(Vec::<RetryInfo>::new()));
    let opts = RetryOpts { max_retries: 2, base_delay_ms: 1000, jitter: false, ..RetryOpts::default() };
    let task = {
        let (calls, events) = (calls.clone(), events.clone());
        tokio::spawn(async move {
            retry_on_transient(
                &opts,
                &CancellationToken::new(),
                |info| events.lock().unwrap().push(info.clone()),
                || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    async { Err::<(), _>(ApiError::Status { status: 503, message: "offline".into() }.into()) }
                },
            )
            .await
        })
    };
    settle().await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_millis(999)).await;
    settle().await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_millis(1)).await;
    settle().await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    tokio::time::advance(Duration::from_millis(2000)).await;
    let err = task.await.unwrap().unwrap_err();
    assert_eq!(err.to_string(), "503 offline");
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    let events = events.lock().unwrap();
    assert_eq!(events.iter().map(|e| e.delay_ms).collect::<Vec<_>>(), [1000, 2000]);
    assert_eq!(events.iter().map(|e| e.attempt).collect::<Vec<_>>(), [1, 2]);
}

#[tokio::test(start_paused = true)]
async fn cancels_the_backoff_without_making_another_request() {
    let calls = Arc::new(AtomicUsize::new(0));
    let cancel = CancellationToken::new();
    let task = {
        let (calls, cancel) = (calls.clone(), cancel.clone());
        tokio::spawn(async move {
            let opts = RetryOpts { base_delay_ms: 1000, jitter: false, ..RetryOpts::default() };
            retry_on_transient(
                &opts,
                &cancel,
                |_| {},
                || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    async { Err::<(), _>(ApiError::Connection("offline".into()).into()) }
                },
            )
            .await
        })
    };
    settle().await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    cancel.cancel();
    assert!(agent::errors::is_aborted(&task.await.unwrap().unwrap_err()));
    tokio::time::advance(Duration::from_secs(3600)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

// Never answers; remembers the token it was handed.
#[derive(Default)]
struct Hanging {
    signal: Mutex<Option<CancellationToken>>,
}

#[async_trait]
impl ChatProvider for Hanging {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Openai
    }
    async fn complete(&self, p: &mut CompletionParams<'_>) -> anyhow::Result<CompletionResult> {
        *self.signal.lock().unwrap() = Some(p.cancel.clone());
        std::future::pending().await
    }
}

#[tokio::test]
async fn propagates_interruption_through_the_provider_adapter() {
    let inner = Arc::new(Hanging::default());
    let provider = with_retry(inner.clone(), RetryOpts::default());
    let cancel = CancellationToken::new();
    let completed = {
        let cancel = cancel.clone();
        tokio::spawn(async move { provider.complete(&mut params(&mut Vec::new(), cancel)).await })
    };
    while inner.signal.lock().unwrap().is_none() {
        tokio::task::yield_now().await;
    }
    cancel.cancel();
    assert!(completed.await.unwrap().is_err());
    assert!(inner.signal.lock().unwrap().as_ref().unwrap().is_cancelled());
}

struct Failing {
    calls: AtomicUsize,
    signals: Mutex<Vec<CancellationToken>>,
}

#[async_trait]
impl ChatProvider for Failing {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Openai
    }
    async fn complete(&self, p: &mut CompletionParams<'_>) -> anyhow::Result<CompletionResult> {
        self.signals.lock().unwrap().push(p.cancel.clone());
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(ApiError::Status { status: 503, message: "offline".into() }.into())
    }
}

#[tokio::test(start_paused = true)]
async fn applies_the_generation_deadline_to_backoff_as_well_as_http_requests() {
    let inner = Arc::new(Failing { calls: AtomicUsize::new(0), signals: Mutex::default() });
    let provider: SharedProvider =
        Arc::new(with_retry(inner.clone(), RetryOpts { base_delay_ms: 1000, jitter: false, ..RetryOpts::default() }));
    let f = support::fixture(provider.clone(), &[], &[]);
    let trace = f.engine.deps.tracer.trace(agent::tracing::TraceStart {
        id: "deadline".into(),
        name: "deadline".into(),
        ..Default::default()
    });
    let scope: agent::tracing::Scope = trace.clone();
    let error = generation(
        &provider,
        &mut params(&mut Vec::new(), CancellationToken::new()),
        &scope,
        Observation { name: "generation".into(), ..Default::default() },
        Some(Duration::from_millis(10)),
        describe_completion,
    )
    .await
    .unwrap_err();
    assert_eq!(error.to_string(), "Generation timed out after 10ms");
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
    // The provider's own token is cancelled: nothing of the call lives on.
    assert!(inner.signals.lock().unwrap()[0].is_cancelled());
    // Rust-only: the generation records the deadline as its failure.
    trace.end();
    let (_, obs) = f.db.get_trace("deadline").unwrap().unwrap();
    let generation = obs.iter().find(|o| o.name == "generation").unwrap();
    assert_eq!(generation.level, "ERROR");
    assert_eq!(generation.output, json!({"error":"Generation timed out after 10ms"}));
    f.engine.shutdown().await;
}

// A loopback endpoint that always answers 503, counting requests.
async fn offline_server() -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let app = axum::Router::new().route(
        "/chat/completions",
        axum::routing::post(move |axum::Json(_): axum::Json<Value>| {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                (axum::http::StatusCode::SERVICE_UNAVAILABLE, axum::Json(json!({"error":{"message":"offline"}})))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, calls, task)
}

// The TS case checks the SDK's own retries are off; here there are none to
// turn off, so the check is that one retry layer makes exactly 1 + 1 calls.
#[tokio::test]
async fn each_provider_makes_its_request_through_one_retry_layer() {
    for kind in [ProviderKind::Openai, ProviderKind::Gemini, ProviderKind::Deepseek] {
        let (url, calls, server) = offline_server().await;
        let inner = Arc::new(provider(kind, Arc::new(HttpApi::new(&url, "test-key"))));
        let p = with_retry(inner, RetryOpts { max_retries: 1, base_delay_ms: 0, ..RetryOpts::default() });
        let err = p.complete(&mut params(&mut Vec::new(), CancellationToken::new())).await.unwrap_err();
        assert_eq!(err.downcast_ref::<ApiError>().and_then(ApiError::status), Some(503), "{kind:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "{kind:?}");
        server.abort();
    }
}

// Rust-only: the delay schedule's cap.
#[test]
fn delays_double_up_to_the_cap() {
    let opts = RetryOpts { base_delay_ms: 1000, max_delay_ms: 3000, jitter: false, ..RetryOpts::default() };
    assert_eq!((1..6).map(|i| retry_delay(&opts, i)).collect::<Vec<_>>(), [1000, 2000, 3000, 3000, 3000]);
}
