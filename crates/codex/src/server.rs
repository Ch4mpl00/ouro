// The Codex sandbox service: POST /run executes one `codex exec` and returns
// its result; GET /health. Reachable only on the compose network (`expose`).

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value, json};

use crate::runner::{ApprovalPolicy, RunRequest, Sandbox, run_codex};

#[derive(Clone, Copy)]
pub struct ServerOptions {
    pub max_body_bytes: usize,
}

impl Default for ServerOptions {
    fn default() -> Self {
        ServerOptions { max_body_bytes: 10 * 1024 * 1024 }
    }
}

fn send(status: StatusCode, body: Value) -> Response {
    (status, [("content-type", "application/json; charset=utf-8")], body.to_string()).into_response()
}

// The sandboxes a caller may ask for. `danger-full-access` (no sandbox) is
// not on the list: no client needs it — code_agent runs workspace-write, the
// judge read-only — and anything else on the compose network that can reach
// this port must not be able to buy an unsandboxed shell with one field.
fn sandbox(value: Option<&Value>) -> Result<Option<Sandbox>, String> {
    match value {
        None => Ok(None),
        Some(v) if v == "read-only" => Ok(Some(Sandbox::ReadOnly)),
        Some(v) if v == "workspace-write" => Ok(Some(Sandbox::WorkspaceWrite)),
        Some(_) => Err("sandbox must be one of read-only, workspace-write".into()),
    }
}

fn approval_policy(value: Option<&Value>) -> Result<Option<ApprovalPolicy>, String> {
    match value {
        None => Ok(None),
        Some(v) if v == "untrusted" => Ok(Some(ApprovalPolicy::Untrusted)),
        Some(v) if v == "on-request" => Ok(Some(ApprovalPolicy::OnRequest)),
        Some(v) if v == "never" => Ok(Some(ApprovalPolicy::Never)),
        Some(_) => Err("approvalPolicy must be one of untrusted, on-request, never".into()),
    }
}

fn optional_string(value: Option<&Value>, field: &str) -> Result<Option<String>, String> {
    match value {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("{field} must be a string")),
    }
}

pub fn validate_run_request(input: &Value) -> Result<RunRequest, String> {
    let Value::Object(body) = input else { return Err("body must be an object".into()) };
    let prompt = match body.get("prompt") {
        Some(Value::String(p)) if !p.is_empty() => p.clone(),
        _ => return Err("prompt must be a non-empty string".into()),
    };
    let timeout_ms = match body.get("timeoutMs") {
        None => None,
        Some(Value::Number(n)) if n.as_f64().is_some_and(|t| t.is_finite() && t > 0.0) => n.as_f64(),
        Some(_) => return Err("timeoutMs must be a positive number".into()),
    };
    let include_events = match body.get("includeEvents") {
        None => None,
        Some(Value::Bool(b)) => Some(*b),
        Some(_) => return Err("includeEvents must be a boolean".into()),
    };
    let config: Option<Map<String, Value>> = match body.get("config") {
        None => None,
        Some(Value::Object(m)) => Some(m.clone()),
        Some(_) => return Err("config must be an object".into()),
    };
    Ok(RunRequest {
        prompt,
        input: body.get("input").cloned(),
        schema: body.get("schema").cloned(),
        sandbox: sandbox(body.get("sandbox"))?,
        approval_policy: approval_policy(body.get("approvalPolicy"))?,
        cwd: optional_string(body.get("cwd"), "cwd")?,
        model: optional_string(body.get("model"), "model")?,
        timeout_ms,
        include_events,
        config,
    })
}

async fn handle(State(options): State<ServerOptions>, req: Request<Body>) -> Response {
    let (method, path) = (req.method().clone(), req.uri().path().to_owned());
    if method == Method::GET && path == "/health" {
        return send(StatusCode::OK, json!({ "ok": true }));
    }
    if method != Method::POST || path != "/run" {
        return send(StatusCode::NOT_FOUND, json!({ "ok": false, "error": "not found" }));
    }
    // Over the limit: 413 with an answer, never a cut-off connection.
    let Ok(raw) = to_bytes(req.into_body(), options.max_body_bytes).await else {
        let error = format!("request body exceeds {} bytes", options.max_body_bytes);
        return send(StatusCode::PAYLOAD_TOO_LARGE, json!({ "ok": false, "error": error }));
    };
    let parsed: Value = if raw.is_empty() {
        json!({})
    } else {
        match serde_json::from_slice(&raw) {
            Ok(v) => v,
            Err(e) => return send(StatusCode::BAD_REQUEST, json!({ "ok": false, "error": e.to_string() })),
        }
    };
    let run = match validate_run_request(&parsed) {
        Ok(r) => r,
        Err(e) => return send(StatusCode::BAD_REQUEST, json!({ "ok": false, "error": e })),
    };
    match run_codex(&run).await {
        Ok(result) => {
            let status = if result["ok"] == true { StatusCode::OK } else { StatusCode::BAD_GATEWAY };
            send(status, result)
        }
        Err(e) => send(StatusCode::BAD_REQUEST, json!({ "ok": false, "error": e.to_string() })),
    }
}

pub fn router(options: ServerOptions) -> Router {
    Router::new().fallback(handle).with_state(options)
}
