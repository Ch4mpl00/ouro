// One `codex exec` run: the request becomes CLI flags, a schema file and
// stdin; the CLI's JSONL stdout becomes the answer. The process runs in its
// own group, so a timeout ends the commands codex started along with codex.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sandbox {
    ReadOnly,
    WorkspaceWrite,
}

impl Sandbox {
    pub fn as_str(self) -> &'static str {
        match self {
            Sandbox::ReadOnly => "read-only",
            Sandbox::WorkspaceWrite => "workspace-write",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalPolicy {
    Untrusted,
    OnRequest,
    Never,
}

impl ApprovalPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            ApprovalPolicy::Untrusted => "untrusted",
            ApprovalPolicy::OnRequest => "on-request",
            ApprovalPolicy::Never => "never",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct RunRequest {
    pub prompt: String,
    // Present-but-null is kept: it reaches stdin as `null`.
    pub input: Option<Value>,
    pub schema: Option<Value>,
    pub sandbox: Option<Sandbox>,
    pub approval_policy: Option<ApprovalPolicy>,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub timeout_ms: Option<f64>,
    pub include_events: Option<bool>,
    pub config: Option<Map<String, Value>>,
}

fn stdin_text(input: &Value) -> String {
    match input {
        Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    }
}

fn json_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

// A `-c key=value` value in TOML syntax.
pub fn toml_value(value: &Value) -> String {
    match value {
        Value::String(s) => json_string(s),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Array(items) => format!("[{}]", items.iter().map(toml_value).collect::<Vec<_>>().join(", ")),
        Value::Object(map) => {
            let entries: Vec<String> =
                map.iter().map(|(k, v)| format!("{} = {}", json_string(k), toml_value(v))).collect();
            format!("{{ {} }}", entries.join(", "))
        }
        Value::Null => "\"\"".into(),
    }
}

struct Parsed {
    events: Vec<Value>,
    content: String,
    usage: Option<Value>,
    thread_id: Option<String>,
    error_message: Option<String>,
}

fn parse_jsonl(stdout: &str) -> Parsed {
    let mut parsed =
        Parsed { events: Vec::new(), content: String::new(), usage: None, thread_id: None, error_message: None };
    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        // Codex --json is JSONL; a line that is not JSON stays out of the
        // events, and stdout is the content if no agent_message arrives.
        let Ok(event) = serde_json::from_str::<Value>(line) else { continue };
        match event["type"].as_str() {
            Some("thread.started") => {
                if let Some(id) = event["thread_id"].as_str() {
                    parsed.thread_id = Some(id.to_owned());
                }
            }
            Some("item.completed") => {
                if event["item"]["type"] == "agent_message"
                    && let Some(text) = event["item"]["text"].as_str()
                {
                    parsed.content = text.to_owned();
                }
            }
            Some("turn.completed") => parsed.usage = Some(event["usage"].clone()),
            // The failure reason (usage limit, auth, model error) arrives as a
            // JSONL error event on stdout; stderr carries only progress.
            Some("error") => {
                if let Some(message) = event["message"].as_str() {
                    parsed.error_message = Some(message.to_owned());
                }
            }
            _ => {}
        }
        parsed.events.push(event);
    }
    if parsed.content.is_empty() {
        parsed.content = stdout.trim().to_owned();
    }
    parsed
}

// How long a timed-out run gets to exit on SIGTERM before SIGKILL.
const KILL_GRACE: Duration = Duration::from_millis(2_000);

fn signal_name(raw: i32) -> Value {
    Signal::try_from(raw).map(|s| Value::from(s.as_str())).unwrap_or(Value::Null)
}

// The answer as the HTTP front sends it: `ok: true` with the content, or
// `ok: false` with why.
pub async fn run_codex(req: &RunRequest) -> anyhow::Result<Value> {
    let work_dir = match &req.cwd {
        Some(dir) => PathBuf::from(dir),
        None => std::env::current_dir()?,
    };
    let timeout = Duration::from_millis(req.timeout_ms.unwrap_or(600_000.0) as u64);
    let tmp = std::env::temp_dir().join(format!("codex-run-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&tmp).await?;
    let result = run_in(req, &work_dir, timeout, &tmp).await;
    let _ = tokio::fs::remove_dir_all(&tmp).await;
    result
}

async fn run_in(req: &RunRequest, work_dir: &Path, timeout: Duration, tmp: &Path) -> anyhow::Result<Value> {
    let mut args: Vec<String> = vec![
        "exec".into(),
        "--ephemeral".into(),
        "--json".into(),
        "--skip-git-repo-check".into(),
        // Host-independent behavior: the host's personal config.toml (model,
        // reasoning effort, MCP servers) must not leak into service runs.
        // Auth still comes from CODEX_HOME; per-run knobs arrive via req.
        "--ignore-user-config".into(),
        "--sandbox".into(),
        req.sandbox.unwrap_or(Sandbox::ReadOnly).as_str().into(),
        // `codex exec` has no --ask-for-approval flag (it's interactive-only);
        // the equivalent knob is the approval_policy config key.
        "-c".into(),
        format!("approval_policy={}", json_string(req.approval_policy.unwrap_or(ApprovalPolicy::Never).as_str())),
    ];
    if let Some(model) = &req.model {
        args.push("--model".into());
        args.push(model.clone());
    }
    if let Some(schema) = &req.schema {
        let path = tmp.join(format!("{}.schema.json", uuid::Uuid::new_v4()));
        tokio::fs::write(&path, serde_json::to_string_pretty(schema)?).await?;
        args.push("--output-schema".into());
        args.push(path.to_string_lossy().into_owned());
    }
    for (key, value) in req.config.iter().flatten() {
        args.push("-c".into());
        args.push(format!("{key}={}", toml_value(value)));
    }
    args.push(req.prompt.clone());

    // Its own process group, so a timeout can end the commands codex started
    // along with codex itself.
    let mut child = Command::new("codex")
        .args(&args)
        .current_dir(work_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()?;
    let pgid = child.id().map(|id| Pid::from_raw(id as i32));

    let mut stdin = child.stdin.take();
    let input = req.input.as_ref().map(stdin_text);
    let feed = async move {
        if let Some(mut pipe) = stdin.take() {
            if let Some(text) = input {
                let _ = pipe.write_all(text.as_bytes()).await;
            }
            let _ = pipe.shutdown().await;
        }
    };
    let mut out = child.stdout.take();
    let mut err = child.stderr.take();
    let read_out = async move {
        let mut s = String::new();
        if let Some(pipe) = out.as_mut() {
            let _ = pipe.read_to_string(&mut s).await;
        }
        s
    };
    let read_err = async move {
        let mut s = String::new();
        if let Some(pipe) = err.as_mut() {
            let _ = pipe.read_to_string(&mut s).await;
        }
        s
    };

    // Past the deadline: SIGTERM to the whole group, then SIGKILL if it is
    // still there after a grace period. SIGTERM alone let a CLI that ignores
    // it (or a command it started) hold the request open forever.
    let kill_group = move |signal: Signal| {
        if let Some(pgid) = pgid {
            let _ = killpg(pgid, signal);
        }
    };
    let wait = async {
        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);
        tokio::select! {
            status = child.wait() => (status, false),
            _ = &mut deadline => {
                kill_group(Signal::SIGTERM);
                tokio::select! {
                    status = child.wait() => (status, true),
                    _ = tokio::time::sleep(KILL_GRACE) => {
                        kill_group(Signal::SIGKILL);
                        (child.wait().await, true)
                    }
                }
            }
        }
    };
    let ((), stdout, stderr, (status, timed_out)) = tokio::join!(feed, read_out, read_err, wait);
    let status = status?;

    use std::os::unix::process::ExitStatusExt;
    let parsed = parse_jsonl(&stdout);
    let events = if req.include_events == Some(true) { Some(Value::Array(parsed.events.clone())) } else { None };
    if !status.success() {
        let code = status.code();
        let signal = status.signal();
        let reason = if timed_out {
            let by =
                signal.and_then(|s| signal_name(s).as_str().map(str::to_owned)).unwrap_or_else(|| "SIGTERM".into());
            format!("codex timed out after {}ms (killed with {by})", timeout.as_millis())
        } else if let Some(s) = signal {
            format!("codex killed by {}", signal_name(s).as_str().unwrap_or("a signal"))
        } else {
            format!("codex exited with code {}", code.map_or("?".into(), |c| c.to_string()))
        };
        let detail = parsed.error_message.clone().or_else(|| Some(stderr.trim().to_owned()).filter(|s| !s.is_empty()));
        let mut failure = json!({
            "ok": false,
            "error": match detail { Some(d) => format!("{reason}: {d}"), None => reason },
            "code": code,
            "signal": signal.map_or(Value::Null, signal_name),
            "stdout": stdout,
            "stderr": stderr,
        });
        if let Some(events) = events {
            failure["events"] = events;
        }
        return Ok(failure);
    }

    let mut ok = json!({ "ok": true, "content": parsed.content });
    if let Ok(value) = serde_json::from_str::<Value>(parsed.content.trim()) {
        ok["parsed"] = value;
    }
    if let Some(usage) = parsed.usage {
        ok["usage"] = usage;
    }
    if let Some(thread) = parsed.thread_id {
        ok["threadId"] = thread.into();
    }
    if let Some(events) = events {
        ok["events"] = events;
    }
    ok["stderr"] = stderr.into();
    Ok(ok)
}
