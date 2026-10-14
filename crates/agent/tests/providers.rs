// Port of src/providers/providers.test.ts: requests and reply/usage contracts.
use agent::{models::ReasoningEffort as Effort, providers::*};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
#[derive(Default)]
struct Api {
    bodies: Mutex<Vec<Value>>,
    reply: Value,
}
#[async_trait]
impl JsonApi for Api {
    async fn post(&self, _: &str, body: Value, _: &CancellationToken) -> anyhow::Result<Value> {
        self.bodies.lock().unwrap().push(body);
        Ok(self.reply.clone())
    }
}
fn api(usage: Value) -> Arc<Api> {
    Arc::new(Api {
        bodies: Default::default(),
        reply: json!({"choices":[{"message":{"role":"assistant","content":"ok","reasoning_content":"keep","extra":"untouched"},"finish_reason":"stop"}],"usage":usage}),
    })
}
async fn complete(
    kind: ProviderKind,
    effort: Effort,
    api: Arc<Api>,
    messages: &mut Vec<Message>,
) -> anyhow::Result<CompletionResult> {
    let p = provider(kind, api);
    p.complete(&mut CompletionParams {
        model: "openrouter/qwen/qwen3".into(),
        messages,
        reasoning_effort: effort,
        tools: vec![ToolDef::new("t", "tool", json!({"type":"object"}))],
        response_format: Some(json!({"type":"json_object"})),
        trace: None,
        cancel: CancellationToken::new(),
    })
    .await
}
#[tokio::test]
async fn provider_effort_mapping_and_passthrough() {
    for (kind, effort, key, value) in [
        (ProviderKind::Openai, Effort::Max, "reasoning_effort", Value::Null),
        (ProviderKind::Deepseek, Effort::Disabled, "thinking", json!({"type":"disabled"})),
        (ProviderKind::Deepseek, Effort::Medium, "reasoning_effort", json!("medium")),
        (ProviderKind::Gemini, Effort::Disabled, "reasoning_effort", Value::Null),
        (ProviderKind::Gemini, Effort::Low, "reasoning_effort", json!("low")),
        (ProviderKind::Gemini, Effort::Max, "reasoning_effort", json!("high")),
        (ProviderKind::Openrouter, Effort::Max, "reasoning", json!({"effort":"high"})),
        (ProviderKind::Openrouter, Effort::Disabled, "reasoning", Value::Null),
    ] {
        let api = api(
            json!({"prompt_tokens":100,"completion_tokens":20,"total_tokens":120,"prompt_tokens_details":{"cached_tokens":64}}),
        );
        let r = complete(kind, effort, api.clone(), &mut vec![user("hello")]).await.unwrap();
        let bodies = api.bodies.lock().unwrap();
        let body = &bodies[0];
        assert_eq!(body[key], value);
        assert_eq!(body["tools"][0]["function"]["name"], "t");
        assert_eq!(body["response_format"], json!({"type":"json_object"}));
        assert_eq!(r.message["extra"], "untouched");
        assert_eq!(r.finish_reason.as_deref(), Some("stop"));
        assert_eq!(r.usage.as_ref().unwrap().input, 100);
        if kind == ProviderKind::Openrouter {
            assert_eq!(body["model"], "qwen/qwen3");
        }
    }
}
#[tokio::test]
async fn deepseek_repairs_history_only_when_thinking_and_keeps_reasoning() {
    let api = api(json!({"prompt_tokens":100,"completion_tokens":20,"total_tokens":120,"prompt_cache_hit_tokens":80}));
    let mut history = vec![user("hello"), assistant("prior")];
    complete(ProviderKind::Deepseek, Effort::Disabled, api.clone(), &mut history).await.unwrap();
    assert!(history[1].get("reasoning_content").is_none());
    let result = complete(ProviderKind::Deepseek, Effort::Max, api.clone(), &mut history).await.unwrap();
    assert_eq!(history[1]["reasoning_content"], "");
    assert!(history[0].get("reasoning_content").is_none());
    history.push(result.message);
    complete(ProviderKind::Deepseek, Effort::High, api.clone(), &mut history).await.unwrap();
    assert_eq!(history[2]["reasoning_content"], "keep");
    assert_eq!(result.usage.unwrap().cached, Some(80));
}
#[tokio::test]
async fn no_tools_or_usage_and_missing_choices() {
    let api = api(Value::Null);
    let p = provider(ProviderKind::Openai, api.clone());
    let r = p
        .complete(&mut CompletionParams {
            model: "test".into(),
            messages: &mut Vec::new(),
            reasoning_effort: Effort::Disabled,
            tools: Vec::new(),
            response_format: None,
            trace: None,
            cancel: CancellationToken::new(),
        })
        .await
        .unwrap();
    assert!(r.usage.is_none());
    assert!(api.bodies.lock().unwrap()[0].get("tools").is_none());
    let bad = Arc::new(Api { reply: json!({"model":"test","choices":[]}), ..Default::default() });
    assert!(
        complete(ProviderKind::Openai, Effort::Disabled, bad, &mut Vec::new())
            .await
            .unwrap_err()
            .to_string()
            .contains("no choices")
    );
}
