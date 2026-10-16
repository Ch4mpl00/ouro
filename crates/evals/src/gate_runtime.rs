// Generator-replay runtime for the gate / improver: re-running the generator
// under the recorded model, and lifting a NodeMaterial + its observation into a
// replayable GateNodeTarget. Shared by the improve cycle, the improve worker
// and judge-gate so they can't drift.

use std::sync::Arc;

use agent::providers::{
    DEEPSEEK_BASE_URL, GEMINI_BASE_URL, HttpApi, JsonApi, OPENAI_BASE_URL, RetryOpts, retry_on_transient,
};
use agent::trace_model::Observation;
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::gate::{ChatMessage, GateNodeTarget, Generator, as_text, recorded_messages};
use crate::materials::NodeMaterial;
use crate::schema::NodeKind;

// The OpenAI-compatible endpoint a model name routes to. Built per call from
// env, so a deepseek-only run never needs a GEMINI/OPENAI key.
pub fn api_for(model: &str) -> Arc<dyn JsonApi> {
    let key = |name: &str| std::env::var(name).unwrap_or_default();
    if model.starts_with("deepseek") {
        Arc::new(HttpApi::new(DEEPSEEK_BASE_URL, &key("DEEPSEEK_API_KEY")))
    } else if model.starts_with("gemini") {
        Arc::new(HttpApi::new(GEMINI_BASE_URL, &key("GEMINI_API_KEY")))
    } else {
        Arc::new(HttpApi::new(OPENAI_BASE_URL, &key("OPENAI_API_KEY")))
    }
}

// Rate limits on replay are hard (free-tier Gemini, the gpt-5.4 per-minute
// token budget); the backoff across attempts can outlive a one-minute window.
pub fn replay_retry() -> RetryOpts {
    RetryOpts { max_retries: 5, base_delay_ms: 3000, ..RetryOpts::default() }
}

pub fn to_param(m: &ChatMessage) -> Value {
    let role = match m.role.as_str() {
        "system" => "system",
        "assistant" => "assistant",
        _ => "user",
    };
    json!({ "role": role, "content": as_text(&m.content) })
}

// One completion over `messages`; `reasoning_effort` only when asked.
pub async fn complete(
    model: &str,
    messages: Vec<Value>,
    json_mode: bool,
    reasoning_effort: Option<&str>,
) -> anyhow::Result<String> {
    let mut body = json!({ "model": model, "messages": messages });
    if json_mode {
        body["response_format"] = json!({ "type": "json_object" });
    }
    if let Some(effort) = reasoning_effort {
        body["reasoning_effort"] = effort.into();
    }
    let api = api_for(model);
    let cancel = CancellationToken::new();
    let res =
        retry_on_transient(&replay_retry(), &cancel, |_| {}, || api.post("chat/completions", body.clone(), &cancel))
            .await?;
    Ok(res["choices"][0]["message"]["content"].as_str().unwrap_or_default().to_owned())
}

pub struct ModelGenerator;

#[async_trait]
impl Generator for ModelGenerator {
    async fn run(&self, messages: &[ChatMessage], model: &str, json_mode: bool) -> anyhow::Result<String> {
        complete(model, messages.iter().map(to_param).collect(), json_mode, None).await
    }
}

// Lift a judged node + its recorded observation into a replayable target.
// None when no generator model can be resolved (can't replay).
pub fn build_gate_target(node: &NodeMaterial, obs: Option<&Observation>) -> Option<GateNodeTarget> {
    let model = obs.and_then(|o| o.model.clone()).or_else(|| std::env::var("AGENT_MODEL").ok())?;
    Some(GateNodeTarget {
        observation_id: node.observation_id.clone(),
        kind: node.kind,
        skill: node.skill.clone(),
        label: node.label.clone(),
        contract: node.contract.clone(),
        input_text: node.input_text.clone(),
        original_output: node.output_text.clone(),
        model,
        recorded_input: obs.map(|o| recorded_messages(&o.input)).unwrap_or_default(),
        json_mode: node.kind == NodeKind::Planner,
    })
}
