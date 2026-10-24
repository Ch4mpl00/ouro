// Secrets: 1. the host's resolver; 2. a run's resolved secrets as the
// `$secrets` argument; 3. redaction, which keeps their values out of
// everything a run persists or reports.
//
// Values exist only in memory, resolved again whenever a run (re)starts.
// Persisted records hold a marker in place of every occurrence of a secret
// string, and get the value back when they are loaded, so replay sees the
// same data. Observer events, errors and the outcome a run returns are
// redacted for good. The function a call reaches gets the real value: that
// is what the secret is for. Events a run takes are stored by the host as
// delivered and are neither redacted nor restored.
use crate::durable::{Annotation, CallState, Record, StepOutcome, TaskResult};
use crate::run::{Status, WorkflowError};
use serde_json::{Map, Value};

// ── 1. resolver ────────────────────────────────────────────────────────────

/// The host's secret store.
pub trait Secrets: Send + Sync {
    /// The value of the secret `name`, or `None` when it does not exist or
    /// this host does not grant it. An error is a failure of the store.
    fn resolve(&self, name: &str) -> anyhow::Result<Option<Value>>;
}

// ── 2. a run's secrets ─────────────────────────────────────────────────────

/// Delimits a marker in redacted text; doubled, it is the character itself.
const MARK: char = '\u{1}';

/// A run's resolved secrets and the redaction they imply. Empty (no
/// declared secrets) it changes nothing.
#[derive(Clone, Debug, Default)]
pub struct Redaction {
    /// Declared name → its value, `None` when the host has none.
    values: Vec<(String, Option<Value>)>,
    /// Every secret string with its marker path (`name.key[0]`), longest first.
    texts: Vec<(String, String)>,
}

impl Redaction {
    /// Resolves every declared secret.
    pub fn resolve(declared: &[String], store: &dyn Secrets) -> anyhow::Result<Self> {
        let mut values = Vec::new();
        for name in declared {
            values.push((name.clone(), store.resolve(name)?));
        }
        let mut texts = Vec::new();
        for (name, value) in &values {
            if let Some(v) = value {
                leaves(v, name, &mut texts);
            }
        }
        texts.sort_by(|a: &(String, String), b| b.0.len().cmp(&a.0.len()).then_with(|| a.1.cmp(&b.1)));
        texts.dedup_by(|a, b| a.0 == b.0);
        Ok(Self { values, texts })
    }

    pub fn active(&self) -> bool {
        !self.texts.is_empty()
    }

    /// `$secrets` for an expression reading `names` (all declared ones when
    /// `None`). Reading one the host does not have is the DSL's
    /// authorization error.
    pub fn argument(&self, names: Option<&[String]>) -> Result<Value, WorkflowError> {
        let mut map = Map::new();
        for (name, value) in &self.values {
            let wanted = names.is_none_or(|n| n.contains(name));
            match value {
                Some(v) => {
                    map.insert(name.clone(), v.clone());
                }
                None if wanted => {
                    return Err(WorkflowError::standard(
                        "authorization",
                        403,
                        "Authorization Error",
                        format!("secret `{name}` is not available to this workflow"),
                    ));
                }
                None => {}
            }
        }
        Ok(Value::Object(map))
    }
}

fn leaves(v: &Value, path: &str, out: &mut Vec<(String, String)>) {
    match v {
        Value::String(s) if !s.is_empty() => out.push((s.clone(), path.to_owned())),
        Value::Array(items) => items.iter().enumerate().for_each(|(i, v)| leaves(v, &format!("{path}[{i}]"), out)),
        Value::Object(map) => map.iter().for_each(|(k, v)| leaves(v, &format!("{path}.{k}"), out)),
        _ => {}
    }
}

// ── 3. redaction ───────────────────────────────────────────────────────────

impl Redaction {
    /// `text` with every secret replaced by its marker.
    pub fn text(&self, text: &str) -> String {
        if !self.active() {
            return text.to_owned();
        }
        let mut out = text.replace(MARK, &format!("{MARK}{MARK}"));
        for (secret, path) in &self.texts {
            out = out.replace(&secret.replace(MARK, &format!("{MARK}{MARK}")), &format!("{MARK}{path}{MARK}"));
        }
        out
    }

    pub fn value(&self, v: &Value) -> Value {
        if !self.active() {
            return v.clone();
        }
        map_strings(v, &mut |s| Ok::<_, String>(self.text(s))).unwrap_or_else(|_| v.clone())
    }

    pub fn error(&self, e: &WorkflowError) -> WorkflowError {
        WorkflowError {
            type_: self.text(&e.type_),
            status: e.status,
            title: e.title.as_deref().map(|t| self.text(t)),
            detail: e.detail.as_deref().map(|t| self.text(t)),
            instance: e.instance.as_deref().map(|t| self.text(t)),
        }
    }

    pub fn status(&self, s: &Status) -> Status {
        match s {
            Status::Completed(v) => Status::Completed(self.value(v)),
            Status::Faulted(e) => Status::Faulted(self.error(e)),
            Status::Interrupted(e) => Status::Interrupted(self.error(e)),
            other => other.clone(),
        }
    }

    /// A record as it may be persisted.
    pub fn record(&self, r: &Record) -> Record {
        if !self.active() {
            return r.clone();
        }
        convert(r, &mut |s| Ok::<_, String>(self.text(s))).unwrap_or_else(|_| r.clone())
    }

    /// A loaded record with its secrets back. Fails when a marker names a
    /// secret this run no longer has.
    pub fn restore(&self, r: Record) -> Result<Record, String> {
        if !self.active() {
            return Ok(r);
        }
        convert(&r, &mut |s| self.restore_text(s))
    }

    pub fn restore_value(&self, v: &Value) -> Result<Value, String> {
        if !self.active() {
            return Ok(v.clone());
        }
        map_strings(v, &mut |s| self.restore_text(s))
    }

    fn restore_text(&self, text: &str) -> Result<String, String> {
        if !text.contains(MARK) {
            return Ok(text.to_owned());
        }
        let mut out = String::new();
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            if c != MARK {
                out.push(c);
                continue;
            }
            let mut path = String::new();
            for c in chars.by_ref() {
                if c == MARK {
                    break;
                }
                path.push(c);
            }
            if path.is_empty() {
                out.push(MARK);
                continue;
            }
            match self.texts.iter().find(|(_, p)| *p == path) {
                Some((secret, _)) => out.push_str(secret),
                None => return Err(format!("a saved value refers to secret `{path}`, which is not available")),
            }
        }
        Ok(out)
    }
}

fn map_strings<E>(v: &Value, f: &mut dyn FnMut(&str) -> Result<String, E>) -> Result<Value, E> {
    Ok(match v {
        Value::String(s) => Value::String(f(s)?),
        Value::Array(items) => Value::Array(items.iter().map(|v| map_strings(v, f)).collect::<Result<_, _>>()?),
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                out.insert(f(k)?, map_strings(v, f)?);
            }
            Value::Object(out)
        }
        other => other.clone(),
    })
}

/// A record's payloads (values, errors) mapped; its structure, identities
/// and the events the host stored are left as they are.
fn convert(r: &Record, f: &mut dyn FnMut(&str) -> Result<String, String>) -> Result<Record, String> {
    Ok(match r {
        Record::Execution { version, run_id, definition, input, runtime, started_ms, max_tasks, max_attempts } => {
            Record::Execution {
                version: *version,
                run_id: run_id.clone(),
                definition: map_strings(definition, f)?,
                input: map_strings(input, f)?,
                runtime: map_strings(runtime, f)?,
                started_ms: *started_ms,
                max_tasks: *max_tasks,
                max_attempts: *max_attempts,
            }
        }
        Record::Task { started_ms, result } => Record::Task {
            started_ms: *started_ms,
            result: match result {
                None => None,
                Some(TaskResult { context, outcome }) => Some(TaskResult {
                    context: map_strings(context, f)?,
                    outcome: match outcome {
                        StepOutcome::Completed { output, flow } => {
                            StepOutcome::Completed { output: map_strings(output, f)?, flow: flow.clone() }
                        }
                        StepOutcome::Faulted { error: e, fatal } => {
                            StepOutcome::Faulted { error: map_error(e, f)?, fatal: *fatal }
                        }
                    },
                }),
            },
        },
        Record::Call { logical, function, arguments, delivery, state, annotations } => Record::Call {
            logical: logical.clone(),
            function: function.clone(),
            arguments: map_strings(arguments, f)?,
            delivery: *delivery,
            state: match state {
                CallState::Pending => CallState::Pending,
                CallState::NotExecuted(e) => CallState::NotExecuted(map_error(e, f)?),
                CallState::Done(Ok(v)) => CallState::Done(Ok(map_strings(v, f)?)),
                CallState::Done(Err(e)) => CallState::Done(Err(map_error(e, f)?)),
            },
            // A text annotation is a payload too: a secret in it is hidden.
            annotations: annotations
                .iter()
                .map(|(k, v)| {
                    let v = match v {
                        Annotation::Text(text) => Annotation::Text(f(text)?),
                        number => number.clone(),
                    };
                    Ok((k.clone(), v))
                })
                .collect::<Result<_, String>>()?,
        },
        Record::Winner { output } => Record::Winner { output: map_strings(output, f)? },
        Record::Finished { status } => Record::Finished {
            status: match status {
                Status::Completed(v) => Status::Completed(map_strings(v, f)?),
                Status::Faulted(e) => Status::Faulted(map_error(e, f)?),
                other => other.clone(),
            },
        },
        Record::Event { .. }
        | Record::Expired { .. }
        | Record::Timer { .. }
        | Record::Retry { .. }
        | Record::Budget { .. }
        | Record::Exceeded { .. } => r.clone(),
    })
}

fn map_error(e: &WorkflowError, f: &mut dyn FnMut(&str) -> Result<String, String>) -> Result<WorkflowError, String> {
    Ok(WorkflowError {
        type_: f(&e.type_)?,
        status: e.status,
        title: e.title.as_deref().map(&mut *f).transpose()?,
        detail: e.detail.as_deref().map(&mut *f).transpose()?,
        instance: e.instance.as_deref().map(&mut *f).transpose()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    struct Store(HashMap<&'static str, Value>);
    impl Secrets for Store {
        fn resolve(&self, name: &str) -> anyhow::Result<Option<Value>> {
            Ok(self.0.get(name).cloned())
        }
    }

    fn redaction() -> Redaction {
        let store =
            Store(HashMap::from([("api", json!({"token": "s3cr3t-token", "user": "bob"})), ("pin", json!("s3cr3t"))]));
        Redaction::resolve(&["api".into(), "pin".into(), "absent".into()], &store).unwrap()
    }

    #[test]
    fn redaction_round_trips_and_hides_every_occurrence() {
        let r = redaction();
        let value = json!({"auth": "Bearer s3cr3t-token", "s3cr3t": ["x\u{1}y", "s3cr3t", 7]});
        let hidden = r.value(&value);
        let text = hidden.to_string();
        assert!(!text.contains("s3cr3t"), "{text}");
        assert_eq!(r.restore_value(&hidden).unwrap(), value);
        let record = Record::Call {
            logical: "/do/0/a".into(),
            function: "f".into(),
            arguments: value.clone(),
            delivery: crate::durable::Delivery::AtLeastOnce,
            state: CallState::Done(Err(WorkflowError::runtime("bob said s3cr3t-token"))),
            annotations: [("note".to_owned(), "s3cr3t-token".into()), ("tokens".to_owned(), 7.into())].into(),
        };
        let stored = r.record(&record);
        assert!(!serde_json::to_string(&stored).unwrap().contains("s3cr3t"));
        assert_eq!(r.restore(stored).unwrap(), record);
        assert!(Redaction::default().restore_value(&json!("\u{1}api.token\u{1}")).is_ok());
        assert!(r.restore_value(&json!("\u{1}gone\u{1}")).is_err());
    }

    #[test]
    fn the_argument_holds_resolved_secrets_and_missing_ones_are_403() {
        let r = redaction();
        assert_eq!(r.argument(Some(&["pin".into()])).unwrap()["pin"], "s3cr3t");
        let e = r.argument(Some(&["absent".into()])).unwrap_err();
        assert!(e.is("authorization") && e.status == 403);
        assert!(r.argument(None).is_err());
    }
}
