// Both sides answer in one shape, holding only what the contract fixes: ok or
// the error code, version, size, body, history, and the version a conflict
// reports. Messages, patch ids, timestamps and near-match suggestions are
// left out — they are allowed to differ. A port of packages/mcp/model/answers.ts.

use serde_json::{Map, Value, json};

use crate::calls::Call;
use crate::model::{Cmd, Reply, World, step};
use crate::support::Outcome;

pub type Answer = Value;

// A write that lost a compare-and-swap race to a concurrent one: a
// version_conflict without a current version. It changes nothing.
pub fn raced() -> Answer {
    json!({ "ok": false, "error": "raced" })
}

fn failure(error: &str, extra: &[(&str, Value)]) -> Answer {
    let mut out = Map::new();
    out.insert("ok".into(), false.into());
    out.insert("error".into(), error.into());
    for (k, v) in extra {
        // Absent stays absent, as `undefined` does in the TS shape.
        if !v.is_null() {
            out.insert((*k).into(), v.clone());
        }
    }
    Value::Object(out)
}

pub fn render(reply: &Reply) -> Answer {
    match reply {
        Reply::Written { version, size_bytes } => json!({ "ok": true, "version": version, "sizeBytes": size_bytes }),
        Reply::Read { body, version } => json!({ "ok": true, "body": body, "version": version }),
        Reply::History { entries } => json!({
            "ok": true,
            "history": entries
                .iter()
                .map(|(kind, before, after)| json!({ "kind": kind.as_str(), "versionBefore": before, "versionAfter": after }))
                .collect::<Vec<_>>(),
        }),
        Reply::DocNotFound => failure("doc_not_found", &[]),
        Reply::VersionRequired { current } => failure("version_required", &[("currentVersion", json!(current))]),
        Reply::VersionConflict { current } => failure("version_conflict", &[("currentVersion", json!(current))]),
        Reply::EditFailed => failure("edit_failed", &[]),
        Reply::PatchNotFound => failure("patch_not_found", &[]),
        Reply::RevertConflict { rollback_to } => {
            failure("revert_conflict", &[("rollbackToVersion", json!(rollback_to))])
        }
        Reply::Rejected => json!({ "rejected": true }),
    }
}

// The model's answer to a call, and the world after it.
pub fn model_answer(world: &World, call: &Call) -> (World, Answer) {
    let (next, reply) = step(world, &call.cmd);
    (next, render(&reply))
}

// The server's answer in the model's shape.
pub fn normalise(call: &Call, outcome: Outcome) -> Answer {
    let value = match outcome {
        Outcome::Rejected { .. } => return json!({ "rejected": true }),
        // Not a memory answer at all (a database error): never valid.
        Outcome::ToolError(message) => return json!({ "failed": message }),
        Outcome::Ok(value) => value,
    };
    if value.get("ok") != Some(&Value::Bool(true)) {
        let error = value.get("error").and_then(Value::as_str).unwrap_or("?").to_owned();
        let current = value.get("currentVersion").cloned().unwrap_or(Value::Null);
        if error == "version_conflict" && current.is_null() {
            return raced();
        }
        if error == "version_conflict" || error == "version_required" {
            return failure(&error, &[("currentVersion", current)]);
        }
        if error == "revert_conflict" {
            return failure(
                &error,
                &[("rollbackToVersion", value.get("rollbackToVersion").cloned().unwrap_or(Value::Null))],
            );
        }
        return failure(&error, &[]);
    }
    match &call.cmd {
        Cmd::Read { .. } => json!({ "ok": true, "body": value["doc"]["body"], "version": value["doc"]["version"] }),
        Cmd::History { .. } => json!({
            "ok": true,
            "history": value["history"]
                .as_array()
                .map(|h| h.iter().map(|p| json!({ "kind": p["kind"], "versionBefore": p["versionBefore"], "versionAfter": p["versionAfter"] })).collect::<Vec<_>>())
                .unwrap_or_default(),
        }),
        _ => json!({ "ok": true, "version": value["version"], "sizeBytes": value["sizeBytes"] }),
    }
}

// Key order does not matter (serde_json maps compare as maps); everything
// else does.
pub fn same_answer(a: &Answer, b: &Answer) -> bool {
    a == b
}
