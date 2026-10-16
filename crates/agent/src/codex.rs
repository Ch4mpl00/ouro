// Sandboxed code execution. Shared by the loop and workflow executor.
use crate::errors::or_cancel;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[async_trait]
pub trait Codex: Send + Sync {
    async fn run(&self, request: Value, cancel: &CancellationToken) -> anyhow::Result<String>;
}
pub type SharedCodex = Arc<dyn Codex>;

pub fn codex_client(base_url: &str) -> SharedCodex {
    Arc::new(HttpCodex { client: reqwest::Client::new(), root: base_url.trim_end_matches('/').into() })
}
struct HttpCodex {
    client: reqwest::Client,
    root: String,
}
#[async_trait]
impl Codex for HttpCodex {
    async fn run(&self, request: Value, cancel: &CancellationToken) -> anyhow::Result<String> {
        or_cancel(cancel, async {
            let response = self.client.post(format!("{}/run", self.root)).json(&request).send().await?;
            let status = response.status();
            let body: Value = response.json().await?;
            if !status.is_success() || body["ok"] != true {
                anyhow::bail!(
                    "{}",
                    body["error"]
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| format!("codex service returned {}", status.as_u16()))
                );
            }
            Ok(body["content"].as_str().ok_or_else(|| anyhow::anyhow!("codex service returned no content"))?.to_owned())
        })
        .await
    }
}
const SYSTEM_FRAMING: &str = "You are a code-execution agent in a sandbox with Python (pandas, openpyxl, numpy) and Node available. Write and run whatever code accomplishes the task, then print ONLY the final result to stdout — no source code, no explanation, no markdown fences. If input data is provided it is on stdin.";

pub async fn code_agent(
    codex: &SharedCodex,
    task: &str,
    data: Option<&str>,
    cancel: &CancellationToken,
) -> anyhow::Result<String> {
    let mut request = json!({
        "prompt": format!("{SYSTEM_FRAMING}\n\nTask:\n{task}"),
        "sandbox": "workspace-write", "approvalPolicy": "never", "timeoutMs": 120_000,
    });
    if let Some(data) = data {
        request["input"] = data.into();
    }
    Ok(codex.run(request, cancel).await?.trim().to_owned())
}
