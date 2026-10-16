// Inspect a Langfuse trace from the terminal.
//
//   langfuse-trace <sessionId>            — all traces in a session, summarised
//   langfuse-trace <sessionId> --raw      — also dump full input/output JSON
//   langfuse-trace <traceId> --by-id      — fetch a single trace by its trace id
use agent::trace_model::{Observation, ObservationType, Trace};
use evals::env::load_env;
use evals::improver::truncate;
use evals::langfuse_api::{Langfuse, encode};
use evals::materials::millis;
use serde_json::Value;

fn to_text(x: &Value, max: usize) -> String {
    match x {
        Value::Null => "—".into(),
        Value::String(s) => truncate(s, max),
        other => truncate(&serde_json::to_string_pretty(other).unwrap_or_default(), max),
    }
}

fn fmt_cost(n: f64) -> String {
    if n == 0.0 {
        "—".into()
    } else if n < 0.01 {
        format!("${n:.5}")
    } else {
        format!("${n:.4}")
    }
}

// Some observations (an open span, one the API returns without timing) have
// no latency — that doesn't stop the render.
fn fmt_sec(s: Option<f64>) -> String {
    s.map_or("—".into(), |s| format!("{s:.2}s"))
}

fn meta_text(t: &Trace, key: &str) -> String {
    match t.metadata.as_ref().and_then(|m| m.get(key)) {
        None | Some(Value::Null) => "—".into(),
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
    }
}

fn render_trace(t: &Trace, observations: &[Observation], latency: Option<f64>, raw: bool) {
    let skills = match t.metadata.as_ref().and_then(|m| m.get("skills")) {
        Some(Value::Array(s)) => s.iter().map(|x| x.as_str().unwrap_or("").to_owned()).collect::<Vec<_>>().join(","),
        _ => "—".into(),
    };
    println!(
        "\n=== {}  ({} / {} · {})  ===",
        t.name,
        meta_text(t, "preset"),
        meta_text(t, "model"),
        meta_text(t, "reasoning_effort")
    );
    println!(
        "id={}  session={}  skills=[{skills}]  tags=[{}]",
        t.id,
        t.session_id.as_deref().unwrap_or("—"),
        t.tags.join(",")
    );
    println!(
        "latency={}  cost={}  observations={}  @ {}",
        fmt_sec(latency),
        fmt_cost(t.total_cost),
        observations.len(),
        t.timestamp
    );
    println!("\nINPUT");
    println!("{}", to_text(&t.input, if raw { 100_000 } else { 600 }));

    let mut sorted: Vec<&Observation> = observations.iter().collect();
    sorted.sort_by_key(|o| millis(&o.start_time));
    println!("\nFLOW");
    for o in sorted {
        // The root placeholder (the trace-level span) — skipped whatever its type.
        if o.name == t.name && o.parent_observation_id.is_none() {
            continue;
        }
        let lvl = if o.level != "DEFAULT" { format!(" [{}]", o.level) } else { String::new() };
        // GENERATION / EMBEDDING are the only model calls (carry usage); every
        // other type is a span-like unit of work.
        if matches!(o.kind, ObservationType::Generation | ObservationType::Embedding) {
            let content = o.output["content"].as_str().unwrap_or("").trim().to_owned();
            let cached =
                o.usage_details.as_ref().and_then(|d| d.get("cached")).and_then(Value::as_u64).filter(|c| *c > 0);
            let usage = o.usage.as_ref().map_or("—".into(), |u| {
                format!("{}→{}{}", u.input, u.output, cached.map(|c| format!(" ({c} cached)")).unwrap_or_default())
            });
            let cost = o.calculated_total_cost.map_or("—".into(), fmt_cost);
            println!(
                "\n  • {}  ({}, {usage} tok, {cost}, {}){lvl}",
                o.name,
                o.model.as_deref().unwrap_or("—"),
                fmt_sec(Some(o.latency))
            );
            if let Some(status) = &o.status_message {
                println!("    status: {status}");
            }
            if !content.is_empty() {
                println!("    content: {}", truncate(&content, if raw { 100_000 } else { 400 }));
            }
            for call in o.output["tool_calls"].as_array().into_iter().flatten() {
                let name = call["function"]["name"].as_str().unwrap_or("?");
                let args = call["function"]["arguments"].as_str().unwrap_or("");
                println!("    → {name}({})", truncate(args, if raw { 100_000 } else { 200 }));
            }
        } else {
            // The observation kind, so typed spans (tool/agent/chain) are
            // visible at a glance, mirroring the UI badge.
            let kind = match o.kind {
                ObservationType::Span => String::new(),
                other => format!(
                    " {{{}}}",
                    serde_json::to_value(other)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_lowercase))
                        .unwrap_or_default()
                ),
            };
            println!("\n  · {}{kind}  ({}){lvl}", o.name, fmt_sec(Some(o.latency)));
            if let Some(status) = &o.status_message {
                println!("    status: {status}");
            }
            let in_text = to_text(&o.input, if raw { 100_000 } else { 200 });
            let out_text = to_text(&o.output, if raw { 100_000 } else { 300 });
            if in_text != "—" {
                println!("    in:  {}", in_text.replace('\n', "\n         "));
            }
            if out_text != "—" {
                println!("    out: {}", out_text.replace('\n', "\n         "));
            }
        }
    }
    println!("\nOUTPUT");
    println!("{}", to_text(&t.output, if raw { 100_000 } else { 800 }));
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    load_env();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(id) = args.iter().find(|a| !a.starts_with("--")) else {
        eprintln!("usage: langfuse-trace <sessionId|traceId> [--by-id] [--raw]");
        std::process::exit(1);
    };
    let by_id = args.iter().any(|a| a == "--by-id");
    let raw = args.iter().any(|a| a == "--raw");
    let langfuse = Langfuse::from_env()?;
    let mut traces: Vec<Value> = if by_id {
        vec![langfuse.get(&format!("/traces/{}", encode(id))).await?]
    } else {
        let list = langfuse.get(&format!("/traces?sessionId={}", encode(id))).await?;
        let data = list["data"].as_array().cloned().unwrap_or_default();
        if data.is_empty() {
            eprintln!("no traces found for session {id}");
            std::process::exit(1);
        }
        data
    };
    // Oldest first — easier to follow how a session unfolded.
    if !by_id {
        traces.sort_by_key(|t| millis(t["timestamp"].as_str().unwrap_or("")));
    }
    for raw_trace in traces {
        let latency = raw_trace["latency"].as_f64();
        let (trace, observations) = langfuse.resolve_trace(raw_trace).await?;
        render_trace(&trace, &observations, latency, raw);
    }
    Ok(())
}
