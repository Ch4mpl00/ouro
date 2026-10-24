// The interpreter: 1. the host's seams (functions, events, children,
// secrets, resources, observer, clock) and the outcome; 2. errors (Problem
// Details with the standard types); 3. the run (data flow, flow directives,
// extensions, limits, the jump budget); 4. task kinds; 5. events: listen and
// emit; 6. child workflows and processes; 7. try, retries and durations.
//
// Each step is reported to the observer under its task reference
// (`/do/1/fetch/try/0/get`), so a trace names exactly which part of the
// definition ran. Calls, processes, emitted events, child runs, secrets and
// external schemas reach the host through its seams; the DSL's semantics
// (flow, data, errors, retries, timers, event matching and consumption,
// extensions) are evaluated here.
use crate::durable::{self, CallEntry, CallFailure, CallState, Delivery, Journal, Record, StepOutcome, TaskResult};
use crate::events::{self, Offer};
use crate::expr::{self, Args};
use crate::model::{
    self, Backoff, Catalog, Catch, DurationSpec, Exceeded, Extension, Flow, Foreach, Invalid, Items, Kind, Listen,
    Problem, Process, Raise, Retry, Return, RunTask, Schema, Strategy, Task, TaskList, Until, Workflow,
};
use crate::secrets::Redaction;
use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, Utc};
use futures::future::{BoxFuture, try_join_all};
use futures::stream::{self, FuturesUnordered, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

// ── 1. host seams and outcome ──────────────────────────────────────────────

/// Where a call comes from, for the host's tracing and cancellation.
pub struct CallInfo<'a> {
    pub run_id: &'a str,
    pub reference: &'a str,
    /// Stable logical invocation identity across DSL retries.
    pub logical: &'a str,
    /// The enclosing retry attempt, starting at one.
    pub attempt: u32,
    pub cancel: &'a CancellationToken,
    pub delivery: Delivery,
    /// The task's transformed input, for a function that resolves templates
    /// against it (an HTTP endpoint's `{petId}`).
    pub input: &'a Value,
    /// Hides the run's secrets in what the host reports about the call
    /// (a span's error); the call itself gets the real values.
    pub secrets: &'a Redaction,
}

/// The user functions `call` reaches, and the host's other seams. Each
/// optional seam an absent host lacks makes validation reject the
/// constructs that need it.
#[async_trait]
pub trait Functions: Send + Sync {
    /// Durable event delivery for `listen`.
    fn events(&self) -> Option<&dyn Events> {
        None
    }
    /// Child runs for `run: workflow`.
    fn children(&self) -> Option<&dyn Children> {
        None
    }
    /// Secret values for `use.secrets`.
    fn secrets(&self) -> Option<&dyn crate::secrets::Secrets> {
        None
    }
    /// External schemas (`schema.resource`).
    fn resources(&self) -> Option<&dyn Resources> {
        None
    }
    /// Whether `emit` publishes through this host.
    fn emits(&self) -> bool {
        false
    }
    /// Publishes an emitted event (its full CloudEvents envelope). Called
    /// again with the same envelope (`id`) when recovery cannot tell whether
    /// the first attempt got through, so receivers deduplicate by
    /// `source` + `id`.
    async fn emit(&self, _event: &Value, _info: &CallInfo<'_>) -> Result<(), CallFailure> {
        Err(CallFailure::NotExecuted(WorkflowError::runtime("emitting events is not supported by this host")))
    }
    /// Whether this host runs `run: <kind>` (`container`, `script`, `shell`).
    fn process(&self, _kind: &str) -> bool {
        false
    }
    /// Runs a process: `definition` is the rendered `run.<kind>` with
    /// `await` added. The result is a process result `{ code, stdout,
    /// stderr }`; a process that is not awaited may return anything.
    /// Recorded and delivered like a function call (`run:<kind>`).
    async fn run_process(&self, kind: &str, _definition: Value, _info: &CallInfo<'_>) -> Result<Value, CallFailure> {
        Err(CallFailure::NotExecuted(WorkflowError::runtime(format!("no process runner for `{kind}`"))))
    }

    /// Whether the host has a function `name`. The engine does not know
    /// whether a function reads or writes: that is the host's to judge.
    fn has(&self, name: &str) -> bool;
    /// Durable entry point. Adapters must honor delivery at every internal
    /// retry boundary; only proven non-execution permits an at-most-once retry.
    async fn invoke(&self, name: &str, args: Value, info: &CallInfo<'_>) -> Result<Value, CallFailure> {
        self.call(name, args, info).await.map_err(CallFailure::Unknown)
    }
    /// Ephemeral invocation, including a host-level stop that YAML cannot
    /// catch. Ordinary adapters keep their existing catchable error contract.
    /// A workbench, for example, interrupts when required test data is absent.
    async fn execute(&self, name: &str, args: Value, info: &CallInfo<'_>) -> Result<Value, CallFailure> {
        self.call(name, args, info).await.map_err(CallFailure::Response)
    }
    async fn call(&self, name: &str, args: Value, info: &CallInfo<'_>) -> Result<Value, WorkflowError>;
    /// Whether this invocation is at most once whatever its task says: the
    /// function's declared delivery. A task's `metadata.delivery` can only
    /// tighten it.
    fn at_most_once(&self, _name: &str, _args: &Value) -> bool {
        false
    }
    /// What an at-most-once invocation whose outcome is unknown returns
    /// instead of faulting the run (the default, `None`). The value is the
    /// host's explicit marker, saved as the call's result; the call is still
    /// never sent again.
    fn unknown_outcome(&self, _name: &str, _args: &Value) -> Option<Value> {
        None
    }
    /// Whether the function stops on the run's cancellation by itself and
    /// finishes its own cleanup (its trace, its children): the run then waits
    /// for it instead of dropping it, and stops at the next task.
    fn cooperative(&self, _name: &str) -> bool {
        false
    }
}

/// One step of a run, keyed by task reference.
#[derive(Debug)]
pub enum Event<'a> {
    /// `parent` is the task this one runs inside (a reusable function's
    /// caller), `None` at the top level.
    TaskStarted {
        reference: &'a str,
        parent: Option<&'a str>,
        kind: &'static str,
        input: &'a Value,
    },
    /// `if` was false; the task's input passes through.
    TaskSkipped {
        reference: &'a str,
        parent: Option<&'a str>,
    },
    TaskCompleted {
        reference: &'a str,
        output: &'a Value,
    },
    TaskSuspended {
        reference: &'a str,
        wake_at_ms: i64,
    },
    /// A `switch` took `case` (its default case included). A switch whose
    /// cases all miss with no default takes none and reports none.
    SwitchCase {
        reference: &'a str,
        case: &'a str,
    },
    TaskFaulted {
        reference: &'a str,
        error: &'a WorkflowError,
    },
    /// `try` caught `error` and runs its body again after `delay`.
    TaskRetrying {
        reference: &'a str,
        attempt: u32,
        delay: Duration,
        error: &'a WorkflowError,
    },
}

pub trait Observer: Send + Sync {
    fn event(&self, event: &Event<'_>);
}

/// An observer that ignores every event.
pub struct Silent;

impl Observer for Silent {
    fn event(&self, _: &Event<'_>) {}
}

// A timeout or a fork winner can drop a pending child without returning a
// fault through `task`; its observer span must still close before its parent.
struct TaskSpan<'a> {
    observer: &'a dyn Observer,
    reference: &'a str,
    active: bool,
}

impl Drop for TaskSpan<'_> {
    fn drop(&mut self) {
        if self.active {
            let error = WorkflowError {
                instance: Some(self.reference.into()),
                ..WorkflowError::standard(
                    "runtime",
                    499,
                    "Task Interrupted",
                    "the task was cancelled or its enclosing task stopped",
                )
            };
            self.observer.event(&Event::TaskFaulted { reference: self.reference, error: &error });
        }
    }
}

// Enclosing deadlines constrain an operator answer too. The guard removes
// only this task's entry when a timeout or fork drops its future.
struct DeadlineGuard<'a> {
    deadlines: &'a Mutex<BTreeMap<String, i64>>,
    reference: &'a str,
}
impl Drop for DeadlineGuard<'_> {
    fn drop(&mut self) {
        self.deadlines.lock().unwrap().remove(self.reference);
    }
}

/// Durable event delivery. `take` atomically consumes the first matching event
/// AND saves `Record::Event` at `key` in this run's journal. It must fence the
/// executor just like Journal::save. Repeating a take returns the saved event.
/// Registration is idempotent, including registration before a question is sent.
/// A type `*` in `take` and `subscribe` stands for every published event.
pub trait Events: Send + Sync {
    /// Persist the currently unfilled listen position for external control.
    fn waiting(&self, _run_id: &str, _key: &str, _wait: &events::PendingWait) -> anyhow::Result<()> {
        Ok(())
    }

    /// Opens (idempotently) a subscription under `key` that keeps receiving
    /// published events of `types` until `unsubscribe`, across takes, so a
    /// listen consuming several events misses none in between.
    fn subscribe(&self, run_id: &str, _key: &str, types: &[String]) -> anyhow::Result<()> {
        self.register(run_id, types)
    }
    fn unsubscribe(&self, _run_id: &str, _key: &str) -> anyhow::Result<()> {
        Ok(())
    }
    /// Subscribe before taking, so a delivery between take and wait is not lost.
    /// Hosts without in-memory notifications still support zero-hold listens.
    fn notification(&self, _run_id: &str) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
    fn register(&self, run_id: &str, types: &[String]) -> anyhow::Result<()>;
    /// Close a timed-out wait. Delivered events remain available to a future
    /// matching listen, but cannot keep an unrelated wait awake.
    fn unregister(&self, run_id: &str, key: &str, types: &[String]) -> anyhow::Result<()>;
    fn take(&self, run_id: &str, key: &str, types: &[String]) -> anyhow::Result<Option<Value>>;
}

/// Child runs for `run: workflow`.
pub trait Children: Send + Sync {
    /// Starts, or with the same `key` joins, a run of the target workflow and
    /// returns its id and the event type that carries its outcome, with
    /// `data` `{ status: completed, output } | { status: faulted, error } |
    /// { status: cancelled }`, delivered to this run through `Events`. An
    /// owned child commits atomically with the parent's ownership and its
    /// subscription to that event; `owned: false` starts an independent run
    /// that nothing awaits. `NotExecuted`/`Response` are catchable (no such
    /// workflow); `Interrupted`/`Unknown` stop the run recoverably.
    fn start(
        &self,
        run_id: &str,
        key: &str,
        target: &ChildTarget<'_>,
        input: &Value,
        owned: bool,
    ) -> Result<Child, CallFailure>;
}

/// `run.workflow`'s reference: the version is a semantic version or `latest`.
pub struct ChildTarget<'a> {
    pub namespace: &'a str,
    pub name: &'a str,
    pub version: &'a str,
}

pub struct Child {
    pub id: String,
    pub done: String,
}

/// External resources: JSON Schemas named by `schema.resource`.
pub trait Resources: Send + Sync {
    fn schema(&self, uri: &str) -> anyhow::Result<Value>;
}

pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// Host-side ceilings no definition can lift.
#[derive(Clone, Debug)]
pub struct Limits {
    /// Task executions per run; every loop pass and `then` jump counts.
    pub max_tasks: usize,
    /// Jumps to a named task per run tree (`model::Budget`): a child run
    /// spends its root's budget, through a journal that shares it
    /// (`Journal::spend`). A definition may only tighten it.
    pub max_jumps: u64,
    /// How deep child runs nest below their root; the `Children` host
    /// refuses a deeper child with `budget.exhausted`.
    pub max_depth: u32,
    /// Attempts per `try`, the first included, whatever the policy says.
    pub max_attempts: u32,
    /// The longest a `wait` or a retry delay sleeps (a dry run's `ZERO`);
    /// `None`: as long as the definition says.
    pub max_sleep: Option<Duration>,
    /// The longest a `listen` with a `timeout` waits for its event; past it
    /// the timeout fires at once instead of the run suspending until it (a
    /// dry run's `ZERO`: an event not already delivered has expired).
    /// `None`: the listen holds and suspends as the definition says.
    pub max_listen: Option<Duration>,
}

impl Default for Limits {
    fn default() -> Self {
        Self { max_tasks: 10_000, max_jumps: 100, max_depth: 10, max_attempts: 10, max_sleep: None, max_listen: None }
    }
}

pub struct Host {
    pub functions: Arc<dyn Functions>,
    pub observer: Arc<dyn Observer>,
    pub clock: Arc<dyn Clock>,
    pub limits: Limits,
    /// `$runtime`: `{ name, version, metadata }`.
    pub runtime: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Status {
    Completed(Value),
    Faulted(WorkflowError),
    Cancelled,
    /// The worker can be released until this UTC timestamp.
    Waiting {
        wake_at_ms: i64,
    },
    /// Persistence failed, ownership was lost or the host interrupted a call
    /// (`CallFailure::Interrupted`). Leave the run recoverable.
    Interrupted(WorkflowError),
}

#[derive(Debug)]
pub struct Outcome {
    pub status: Status,
    /// The run's calls (and emitted events), in record-key order: what was
    /// called, its delivery and how far it got. The host decides from it
    /// whether the run changed the world, e.g. whether a failed run can be
    /// handed over or rerun as if it had never happened. Empty when the
    /// run's records could not be loaded (an interrupted outcome).
    pub calls: Vec<CallEntry>,
    pub tasks_run: usize,
    /// The run ran out of jump budget and that ended it: its `on_exceeded`
    /// ended it (`end`, or a handler's output or fault), or an uncaught
    /// `budget.exhausted` faulted it.
    pub limit_exceeded: bool,
}

/// `model::validate` against the host's functions.
pub fn validate(wf: &Workflow, functions: &dyn Functions) -> Result<(), Invalid> {
    struct Known<'a>(&'a dyn Functions);
    impl Catalog for Known<'_> {
        fn has(&self, name: &str) -> bool {
            self.0.has(name)
        }
        fn events(&self) -> bool {
            self.0.events().is_some()
        }
        fn emits(&self) -> bool {
            self.0.emits()
        }
        fn children(&self) -> bool {
            self.0.children().is_some() && self.0.events().is_some()
        }
        fn processes(&self, kind: &str) -> bool {
            self.0.process(kind)
        }
        fn secrets(&self) -> bool {
            self.0.secrets().is_some()
        }
        fn resources(&self) -> bool {
            self.0.resources().is_some()
        }
    }
    model::validate(wf, &Known(functions))
}

/// What validation cannot know without the host: a definition's jump
/// budget may only tighten the host's ceiling.
pub fn validate_limits(wf: &Workflow, limits: &Limits) -> Result<(), Invalid> {
    match wf.budget.max_jumps {
        Some(n) if n > limits.max_jumps => Err(Invalid(vec![Problem {
            at: "/document/metadata/limits/max_jumps".into(),
            message: format!("{n} is above this host's ceiling of {} jumps", limits.max_jumps),
        }])),
        _ => Ok(()),
    }
}

/// The tasks an `on_exceeded` handler may run, itself included.
pub const HANDLER_TASKS: usize = 10;

/// The reference segment under a jumping task where its handler runs.
const HANDLER: &str = "/limits/on_exceeded";

// ── 2. errors ──────────────────────────────────────────────────────────────

const STANDARD_ERRORS: &str = "https://open-workflow-specification.org/spec/1.0.0/errors/";

/// A workflow error as RFC 7807 Problem Details; `instance` is the
/// reference of the task that raised it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowError {
    #[serde(rename = "type")]
    pub type_: String,
    pub status: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
}

impl WorkflowError {
    /// A standard error: `kind` is the last segment of its type URI.
    pub fn standard(kind: &str, status: u16, title: &str, detail: impl Into<String>) -> Self {
        Self {
            type_: format!("{STANDARD_ERRORS}{kind}"),
            status,
            title: Some(title.into()),
            detail: Some(detail.into()),
            instance: None,
        }
    }
    pub fn validation(detail: impl Into<String>) -> Self {
        Self::standard("validation", 400, "Validation Error", detail)
    }
    pub fn expression(detail: impl Into<String>) -> Self {
        Self::standard("expression", 400, "Expression Error", detail)
    }
    pub fn timeout(detail: impl Into<String>) -> Self {
        Self::standard("timeout", 408, "Timeout", detail)
    }
    pub fn communication(status: u16, detail: impl Into<String>) -> Self {
        Self::standard("communication", status, "Communication Error", detail)
    }
    pub fn runtime(detail: impl Into<String>) -> Self {
        Self::standard("runtime", 500, "Runtime Error", detail)
    }
    /// A host budget ran out: the run tree's jumps, an `on_exceeded`
    /// handler's allowance, or the nesting depth of child runs. CHOICE: in
    /// the standard namespace like `outcome_unknown`, status 508 (a loop).
    pub fn budget(detail: impl Into<String>) -> Self {
        Self::standard("budget.exhausted", 508, "Budget Exhausted", detail)
    }
    /// Whether this is the standard error `kind` (`"timeout"`, …).
    pub fn is(&self, kind: &str) -> bool {
        self.type_.strip_prefix(STANDARD_ERRORS) == Some(kind)
    }
    /// Whether a `catch.errors.with.type` names this error's type. CHOICE: a
    /// standard error also answers to its kind under the namespaces the DSL
    /// has used: the deprecated `https://serverlessworkflow.io/spec/1.0.0/errors/`
    /// and `https://open-workflow-specification.org/dsl/errors/types/`
    /// (the conformance kit's), so portable definitions catch it either way.
    pub fn has_type(&self, filter: &str) -> bool {
        filter == self.type_
            || self.type_.strip_prefix(STANDARD_ERRORS).is_some_and(|kind| {
                [
                    "https://serverlessworkflow.io/spec/1.0.0/errors/",
                    "https://open-workflow-specification.org/dsl/errors/types/",
                ]
                .iter()
                .any(|ns| filter.strip_prefix(ns) == Some(kind))
            })
    }
    fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

impl std::fmt::Display for WorkflowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.title.as_deref().unwrap_or(&self.type_))?;
        if let Some(d) = &self.detail {
            write!(f, ": {d}")?;
        }
        if let Some(i) = &self.instance {
            write!(f, " (at {i})")?;
        }
        Ok(())
    }
}

impl std::error::Error for WorkflowError {}

/// Why a step stopped. Only `Error` can be caught by `try`.
#[derive(Clone)]
enum Fault {
    Error(WorkflowError),
    /// A host limit was hit: the run stops whatever the definition says.
    Fatal(WorkflowError),
    Cancelled,
    Waiting(i64),
    /// Persistence failed, ownership was lost or the host interrupted a
    /// call: the run stops where it is and stays recoverable.
    Interrupted(WorkflowError),
    /// The jump budget ran out and `on_exceeded` ends the run with this
    /// output. Nothing catches it; no enclosing task checkpoints it, so
    /// recovery reaches the refused jump (and the handler's saved result)
    /// again.
    Exceeded(Value),
}

impl From<WorkflowError> for Fault {
    fn from(e: WorkflowError) -> Self {
        Fault::Error(e)
    }
}

fn expression(e: String) -> Fault {
    Fault::Error(WorkflowError::expression(e))
}

fn conform(schema: &Value, value: &Value, what: &str) -> Result<(), Fault> {
    let validator =
        jsonschema::validator_for(schema).map_err(|e| WorkflowError::validation(format!("{what} schema: {e}")))?;
    let errors: Vec<String> =
        validator.iter_errors(value).take(3).map(|e| format!("{}: {e}", e.instance_path())).collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(WorkflowError::validation(format!("{what} does not match its schema: {}", errors.join("; "))).into())
    }
}

// ── 3. the run ─────────────────────────────────────────────────────────────

/// Variables a scope adds on top of the runtime arguments (`for`'s item
/// and index, `catch.as`); a later entry shadows an earlier one.
type Scope = Vec<(String, Value)>;

/// How a task list finished: on its own, by `then: exit` (which only an
/// extension's `before` list tells apart: it skips the extended task), or by
/// `then: end`, which ends the whole workflow.
enum Next {
    Done(Value),
    Exit(Value),
    End(Value),
}

fn ended(next: Next) -> (Value, Option<Flow>) {
    match next {
        Next::Done(v) | Next::Exit(v) => (v, None),
        Next::End(v) => (v, Some(Flow::End)),
    }
}

/// Where a run waits forever for a delivery (the end of 9999, in ms).
const NO_DEADLINE: i64 = 253_402_300_799_000;

/// A reference segment that marks a task of an extension's `before` or
/// `after` list; those tasks are not extended themselves.
const EXTENSION_TASKS: &str = "/use/extensions/";

/// What a call record names: a function, or a process run.
#[derive(Clone, Copy)]
enum Target<'a> {
    Function(&'a str),
    Process(&'a str),
}

fn datetime(t: DateTime<Utc>) -> Value {
    json!({
        "iso8601": t.to_rfc3339_opts(SecondsFormat::Millis, true),
        "epoch": { "seconds": t.timestamp(), "milliseconds": t.timestamp_millis() },
    })
}

/// Runs `wf` on `input` to completion, a fault, or `cancel`.
pub async fn run(host: &Host, wf: &Workflow, run_id: &str, input: Value, cancel: &CancellationToken) -> Outcome {
    execute(host, wf, run_id, input, cancel, None, None).await
}

/// Replays saved checkpoints, then executes until completion or a durable
/// wait. The same run must always use the same definition and original input.
/// A first execution starts the workflow's clock now (`resume_from` fixes it
/// at admission instead).
pub async fn resume(
    host: &Host,
    wf: &Workflow,
    run_id: &str,
    input: Value,
    cancel: &CancellationToken,
    journal: &dyn Journal,
) -> Outcome {
    execute(host, wf, run_id, input, cancel, Some(journal), None).await
}

/// `resume` for a run admitted at `admitted_ms`: its workflow `timeout` and
/// `$workflow.startedAt` count from admission, so a run that waited for a
/// worker longer than its timeout times out before any task runs.
pub async fn resume_from(
    host: &Host,
    wf: &Workflow,
    run_id: &str,
    input: Value,
    admitted_ms: i64,
    cancel: &CancellationToken,
    journal: &dyn Journal,
) -> Outcome {
    execute(host, wf, run_id, input, cancel, Some(journal), Some(admitted_ms)).await
}

async fn execute(
    host: &Host,
    wf: &Workflow,
    run_id: &str,
    input: Value,
    cancel: &CancellationToken,
    journal: Option<&dyn Journal>,
    admitted_ms: Option<i64>,
) -> Outcome {
    let interrupted = |e: String| Outcome {
        status: Status::Interrupted(WorkflowError::runtime(e)),
        calls: Vec::new(),
        tasks_run: 0,
        limit_exceeded: false,
    };
    // Secrets resolve again on every start, before anything is loaded: the
    // saved records hold markers, which loading turns back into values.
    let secrets = match (wf.uses.secrets.is_empty(), host.functions.secrets()) {
        (true, _) => Redaction::default(),
        (false, Some(store)) => match Redaction::resolve(&wf.uses.secrets, store) {
            Ok(r) => r,
            Err(e) => return interrupted(format!("resolving secrets: {e}")),
        },
        (false, None) => return interrupted("this host has no secret resolver".into()),
    };
    let loaded = journal.map(Journal::load).transpose().map_err(|e| e.to_string()).and_then(|records| {
        records
            .unwrap_or_default()
            .into_iter()
            .map(|(k, r)| Ok((k, secrets.restore(r)?)))
            .collect::<Result<BTreeMap<_, _>, String>>()
    });
    let records = match loaded {
        Ok(records) => records,
        Err(e) => return interrupted(secrets.text(&e)),
    };
    let tasks = records.values().filter(|r| matches!(r, Record::Task { .. })).count();
    let calls = durable::call_journal(&records);
    let stopped = |status: Status| Outcome { status, calls: calls.clone(), tasks_run: tasks, limit_exceeded: false };
    let (started_ms, runtime) = match records.get("execution") {
        Some(Record::Execution { version, max_tasks, max_attempts, .. })
            if *version != durable::VERSION
                || *max_tasks != host.limits.max_tasks
                || *max_attempts != host.limits.max_attempts =>
        {
            return stopped(Status::Interrupted(WorkflowError::runtime(
                "incompatible durable version or execution limits",
            )));
        }
        Some(Record::Execution {
            version,
            run_id: original_id,
            definition,
            input: original,
            runtime,
            started_ms,
            max_tasks,
            max_attempts,
        }) if *version == durable::VERSION
            && original_id == run_id
            && definition == &wf.definition
            && original == &input
            && *max_tasks == host.limits.max_tasks
            && *max_attempts == host.limits.max_attempts =>
        {
            (*started_ms, runtime.clone())
        }
        Some(_) => return stopped(Status::Faulted(WorkflowError::runtime("a run's definition or input changed"))),
        None if records.is_empty() => {
            (admitted_ms.unwrap_or_else(|| host.clock.now().timestamp_millis()), host.runtime.clone())
        }
        None => return stopped(Status::Interrupted(WorkflowError::runtime("missing execution record"))),
    };
    if records.iter().any(|(key, record)| !match record {
        Record::Execution { .. } => key == "execution",
        Record::Finished { status } => {
            key == "finished" && matches!(status, Status::Completed(_) | Status::Faulted(_) | Status::Cancelled)
        }
        Record::Task { .. } => key.ends_with("#task"),
        Record::Call { .. } => key.ends_with("#call"),
        Record::Winner { .. } => key.ends_with("#winner"),
        Record::Event { .. } => key.contains("#event[") || key.ends_with("#child"),
        Record::Expired { .. } => key.starts_with("mailbox#expired["),
        Record::Timer { .. } => key.ends_with("#wait") || key.contains("#retry[") || key.contains("#attempt["),
        Record::Retry { .. } => key.contains("#retry-decision["),
        Record::Budget { .. } => key.ends_with("#jump"),
        Record::Exceeded { .. } => key == durable::EXCEEDED,
    }) {
        return stopped(Status::Interrupted(WorkflowError::runtime("invalid durable record key or terminal state")));
    }
    if records.values().any(|record| match record {
        Record::Execution { started_ms, .. } | Record::Task { started_ms, .. } => {
            DateTime::from_timestamp_millis(*started_ms).is_none()
        }
        _ => false,
    }) {
        return stopped(Status::Interrupted(WorkflowError::runtime("invalid saved timestamp")));
    }
    let jumps = records.values().filter(|r| matches!(r, Record::Budget { granted: true, .. })).count();
    let run = Run {
        host,
        wf,
        run_id,
        cancel,
        tasks_run: AtomicUsize::new(tasks),
        jumps: AtomicU64::new(jumps as u64),
        max_jumps: wf.budget.max_jumps(host.limits.max_jumps),
        spending: Mutex::new(()),
        handlers: Mutex::default(),
        handler_failed: AtomicBool::new(false),
        exceeded: tokio::sync::watch::channel(None).0,
        calls: Mutex::new(calls.iter().map(|c| (c.key.clone(), c.clone())).collect()),
        protected: AtomicUsize::new(0),
        workflow: json!({ "id": run_id, "definition": wf.definition, "input": input, "startedAt": datetime(DateTime::from_timestamp_millis(started_ms).expect("stored timestamp")) }),
        runtime: runtime.clone(),
        journal,
        records: Mutex::new(records),
        attempts: Mutex::default(),
        deadlines: Mutex::default(),
        secrets,
    };
    if let Some(Record::Finished { status }) = run.record("finished") {
        // A finished run that ended through its budget left a refused jump
        // (or the uncaught error) behind.
        let refused = run.records.lock().unwrap_or_else(|e| e.into_inner()).values().any(|r| {
            matches!(r, Record::Budget { granted: false, .. }) && !matches!(run.wf.budget.on_exceeded, Exceeded::Raise)
        });
        let limit_exceeded = refused || matches!(&status, Status::Faulted(e) if e.is("budget.exhausted"));
        return Outcome { status: run.secrets.status(&status), calls, tasks_run: tasks, limit_exceeded };
    }
    let result = async {
        if run.record("execution").is_none() {
            run.save(
                "execution",
                Record::Execution {
                    version: durable::VERSION,
                    run_id: run_id.into(),
                    definition: wf.definition.clone(),
                    input: input.clone(),
                    runtime,
                    started_ms,
                    max_tasks: host.limits.max_tasks,
                    max_attempts: host.limits.max_attempts,
                },
            )?;
        }
        // Cancelled before anything ran: that wins over a fault the
        // workflow's own input expression would raise.
        if cancel.is_cancelled() {
            return Err(Fault::Cancelled);
        }
        match &wf.timeout {
            None => run.workflow(input).await,
            Some(spec) => match run.duration(
                spec,
                &input,
                &run.args(&Value::Null, &Value::Null, &Value::Null, &Value::Null, &Scope::new()),
            ) {
                Err(f) => Err(f),
                Ok(limit) => match run.remaining(limit, started_ms) {
                    Err(_) if run.has_unknown("") => Err(run.unknown("workflow")),
                    Err(e) => Err(e),
                    Ok((remaining, deadline)) => match {
                        run.deadlines.lock().unwrap().insert(String::new(), deadline);
                        tokio::time::timeout(remaining, run.workflow(input)).await
                    } {
                        Ok(Err(Fault::Waiting(wake))) => Err(Fault::Waiting(wake.min(deadline))),
                        Ok(r) => r,
                        Err(_) if run.has_unknown("") => Err(run.unknown("workflow")),
                        Err(_) => Err(Fault::Fatal(WorkflowError::timeout(format!(
                            "the workflow did not finish within {limit:?}"
                        )))),
                    },
                },
            },
        }
    }
    .await;
    let mut limit_exceeded = run.handler_failed.load(Ordering::SeqCst);
    let mut status = match result {
        Ok(v) => Status::Completed(v),
        Err(Fault::Exceeded(v)) => {
            limit_exceeded = true;
            Status::Completed(v)
        }
        Err(Fault::Error(e) | Fault::Fatal(e)) => {
            limit_exceeded |= e.is("budget.exhausted");
            Status::Faulted(e)
        }
        Err(Fault::Cancelled) => Status::Cancelled,
        Err(Fault::Waiting(wake_at_ms)) => Status::Waiting { wake_at_ms },
        Err(Fault::Interrupted(e)) => Status::Interrupted(e),
    };
    if matches!(status, Status::Completed(_) | Status::Faulted(_) | Status::Cancelled)
        && let Err(Fault::Interrupted(error)) = run.save("finished", Record::Finished { status: status.clone() })
    {
        status = Status::Interrupted(error);
    }
    Outcome {
        status: run.secrets.status(&status),
        calls: run.calls.lock().unwrap_or_else(|e| e.into_inner()).values().cloned().collect(),
        tasks_run: run.tasks_run.load(Ordering::SeqCst),
        limit_exceeded: limit_exceeded && matches!(status, Status::Completed(_) | Status::Faulted(_)),
    }
}

struct Run<'a> {
    host: &'a Host,
    wf: &'a Workflow,
    run_id: &'a str,
    cancel: &'a CancellationToken,
    tasks_run: AtomicUsize,
    /// Jumps this run was granted (`$run.jumps`).
    jumps: AtomicU64,
    /// The run's jump budget: the host's ceiling, tightened by the definition.
    max_jumps: u64,
    /// Serializes this run's own decisions when its journal keeps no shared
    /// budget, so concurrent branches cannot both take the last unit.
    spending: Mutex<()>,
    /// The `on_exceeded` handlers running now, by reference prefix, and the
    /// tasks each has entered.
    handlers: Mutex<Vec<(String, usize)>>,
    /// A handler faulted, which ended the run through its budget.
    handler_failed: AtomicBool,
    /// How the run's first refused jump ended this execution, for the
    /// branches refused at the same moment, which follow it.
    exceeded: tokio::sync::watch::Sender<Option<Fault>>,
    /// The call journal by record key, kept for runs without a journal too.
    calls: Mutex<BTreeMap<String, CallEntry>>,
    /// At-most-once calls started so far, for runs without a journal: they
    /// keep no results, so an attempt that started one is never retried.
    protected: AtomicUsize,
    /// `$workflow`.
    workflow: Value,
    runtime: Value,
    journal: Option<&'a dyn Journal>,
    records: Mutex<BTreeMap<String, Record>>,
    /// Attempt prefixes of the `try`s running now: `<task>/try[k]` → `<task>/try`.
    attempts: Mutex<Vec<(String, String, u32)>>,
    /// The declared secrets; what is persisted or reported is redacted.
    secrets: Redaction,
    deadlines: Mutex<BTreeMap<String, i64>>,
}

impl Run<'_> {
    fn record(&self, key: &str) -> Option<Record> {
        self.records.lock().unwrap_or_else(|e| e.into_inner()).get(key).cloned()
    }

    fn save(&self, key: &str, record: Record) -> Result<(), Fault> {
        if self.journal.is_none() {
            self.note(key, &record);
        }
        if let Some(journal) = self.journal {
            if let Some(old) = self.record(key) {
                durable::check_transition(&old, &record)
                    .map_err(|e| Fault::Interrupted(WorkflowError::runtime(e.to_string())))?;
            }
            journal.save(key, &self.secrets.record(&record)).map_err(|e| {
                Fault::Interrupted(WorkflowError::runtime(format!(
                    "checkpoint {key}: {}",
                    self.secrets.text(&e.to_string())
                )))
            })?;
            self.note(key, &record);
            self.records.lock().unwrap_or_else(|e| e.into_inner()).insert(key.to_owned(), record);
        }
        Ok(())
    }

    /// Keeps the call journal current; only after a durable save when the
    /// run has a journal.
    fn note(&self, key: &str, record: &Record) {
        if let Some(entry) = CallEntry::of(key, record) {
            self.calls.lock().unwrap_or_else(|e| e.into_inner()).insert(key.to_owned(), entry);
        }
    }

    fn remaining(&self, duration: Duration, started_ms: i64) -> Result<(Duration, i64), Fault> {
        let millis = i64::try_from(duration.as_millis()).unwrap_or(i64::MAX);
        let deadline = started_ms.saturating_add(millis);
        let left = if self.journal.is_some() {
            deadline.saturating_sub(self.host.clock.now().timestamp_millis())
        } else {
            millis
        };
        if left <= 0 {
            return Err(WorkflowError::timeout("the saved deadline has passed").into());
        }
        Ok((Duration::from_millis(left as u64), deadline))
    }

    /// An at-most-once call in the attempt whose outcome forbids sending it
    /// again: unknown, or a tool error (which is no proof it did nothing).
    /// One that succeeded does not block: the next attempt reuses its result.
    fn blocks_retry(&self, reference: &str) -> bool {
        self.records.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|(key, r)| {
            key.starts_with(&format!("{reference}/"))
                && matches!(
                    r,
                    Record::Call {
                        delivery: Delivery::AtMostOnce,
                        state: CallState::Pending | CallState::Done(Err(_)),
                        ..
                    }
                )
        })
    }

    /// `reference` as the first attempt of every enclosing `try` names it.
    /// Only the prefixes `attempt` registered are replaced, longest (the
    /// innermost `try`) first, so a task merely named `try` never folds.
    fn logical(&self, reference: &str) -> String {
        let attempts = self.attempts.lock().unwrap_or_else(|e| e.into_inner());
        let mut logical = reference.to_owned();
        while let Some((attempt, first, _)) = attempts
            .iter()
            .filter(|(attempt, first, _)| {
                attempt != first
                    && logical
                        .strip_prefix(attempt.as_str())
                        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
            })
            .max_by_key(|(attempt, _, _)| attempt.len())
        {
            logical = format!("{first}{}", &logical[attempt.len()..]);
        }
        logical
    }

    /// What an earlier attempt of this protected call left: the result to
    /// reuse, a fault, or None when it has to be sent (again).
    fn earlier_attempt(
        &self,
        logical: &str,
        key: &str,
        name: &str,
        args: &Value,
        reference: &str,
    ) -> Option<Result<Value, Fault>> {
        let records = self.records.lock().unwrap_or_else(|e| e.into_inner());
        let earlier: Vec<_> = records
            .iter()
            .filter(|(k, _)| k.as_str() != key)
            .filter_map(|(_, r)| match r {
                Record::Call { logical: l, function, arguments, delivery, state, .. } if l == logical => {
                    Some((function, arguments, delivery, state))
                }
                _ => None,
            })
            .collect();
        if let Some((function, arguments, _, CallState::Done(Ok(value)))) =
            earlier.iter().copied().find(|(.., state)| matches!(state, CallState::Done(Ok(_))))
        {
            if function != name || arguments != args {
                return Some(Err(Fault::Fatal(WorkflowError::standard(
                    "conflict",
                    409,
                    "Idempotency conflict",
                    format!(
                        "{reference}: an earlier attempt already made this call with other arguments; nothing was sent"
                    ),
                ))));
            }
            return Some(Ok(value.clone()));
        }
        // Retrying an attempt like this is refused (`blocks_retry`); kept here
        // so that no other path can send an at-most-once call twice.
        for (_, _, delivery, state) in &earlier {
            match (delivery, state) {
                (Delivery::AtMostOnce, CallState::Pending) => {
                    return Some(Err(self.unknown(reference)));
                }
                (Delivery::AtMostOnce, CallState::Done(Err(error))) => {
                    return Some(Err(Fault::Error(error.clone())));
                }
                _ => {}
            }
        }
        None
    }

    fn has_unknown(&self, reference: &str) -> bool {
        self.records.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|(key, r)| {
            (key == &format!("{reference}#call") || key.starts_with(&format!("{reference}/")))
                && matches!(r, Record::Call { delivery: Delivery::AtMostOnce, state: CallState::Pending, .. })
        })
    }

    fn unknown(&self, reference: &str) -> Fault {
        Fault::Fatal(WorkflowError::standard(
            "outcome_unknown",
            409,
            "Outcome unknown",
            format!("{reference}: at-most-once call may have executed; automatic retry is forbidden"),
        ))
    }

    fn observe(&self, event: Event<'_>) {
        if !self.secrets.active() {
            return self.host.observer.event(&event);
        }
        let observer = &self.host.observer;
        match event {
            Event::TaskStarted { reference, parent, kind, input } => {
                let input = self.secrets.value(input);
                observer.event(&Event::TaskStarted { reference, parent, kind, input: &input })
            }
            Event::TaskCompleted { reference, output } => {
                let output = self.secrets.value(output);
                observer.event(&Event::TaskCompleted { reference, output: &output })
            }
            Event::TaskFaulted { reference, error } => {
                let error = self.secrets.error(error);
                observer.event(&Event::TaskFaulted { reference, error: &error })
            }
            Event::TaskRetrying { reference, attempt, delay, error } => {
                let error = self.secrets.error(error);
                observer.event(&Event::TaskRetrying { reference, attempt, delay, error: &error })
            }
            other => observer.event(&other),
        }
    }

    /// Arguments for an `input.from`, the only expression that sees
    /// `$secrets`; only the secrets it names are required to exist.
    fn input_args(&self, mut args: Args, t: &Value) -> Result<Args, Fault> {
        let mut codes = Vec::new();
        match t {
            Value::String(code) => codes.push(code.clone()),
            template => expr::template_exprs(template, &mut codes),
        }
        let value = if codes.iter().any(|c| c.contains("$secrets")) {
            let names: Option<Vec<String>> =
                codes.iter().map(|c| model::secret_names(c)).collect::<Option<Vec<_>>>().map(|n| n.concat());
            self.secrets.argument(names.as_deref())?
        } else {
            Value::Null
        };
        args.set(model::SECRETS, value);
        Ok(args)
    }

    /// Validates against an inline or a host-resolved schema.
    fn conform(&self, schema: &Schema, value: &Value, what: &str) -> Result<(), Fault> {
        match schema {
            Schema::Document(d) => conform(d, value, what),
            Schema::Resource(uri) => {
                let resources = self
                    .host
                    .functions
                    .resources()
                    .ok_or_else(|| WorkflowError::validation(format!("no resource resolver for `{uri}`")))?;
                let document = resources
                    .schema(uri)
                    .map_err(|e| Fault::Interrupted(WorkflowError::runtime(format!("schema `{uri}`: {e}"))))?;
                conform(&document, value, what)
            }
        }
    }

    /// The runtime arguments, then the scope's variables. Arguments an
    /// expression has no access to are bound to null, so validation and
    /// evaluation see the same names.
    fn args(&self, ctx: &Value, input: &Value, output: &Value, task: &Value, scope: &Scope) -> Args {
        let mut args = Args::new()
            .with("context", ctx.clone())
            .with("input", input.clone())
            .with("output", output.clone())
            .with("task", task.clone())
            .with("workflow", self.workflow.clone())
            .with("runtime", self.runtime.clone())
            .with(model::RUN, self.run_argument(json!({})));
        for (name, value) in scope {
            args.set(name, value.clone());
        }
        args
    }

    /// `input.from` / `output.as` / `export.as`: an expression or a template.
    fn transform(&self, t: &Value, on: &Value, args: &Args) -> Result<Value, Fault> {
        match t {
            Value::String(code) => expr::eval(code, on, args),
            template => expr::render(template, on, args),
        }
        .map_err(expression)
    }

    async fn workflow(&self, raw: Value) -> Result<Value, Fault> {
        let null = Value::Null;
        let none = Scope::new();
        if let Some(s) = self.wf.input.as_ref().and_then(|i| i.schema.as_ref()) {
            self.conform(s, &raw, "the workflow input")?;
        }
        let input = match self.wf.input.as_ref().and_then(|i| i.from.as_ref()) {
            Some(t) => self.transform(t, &raw, &self.input_args(self.args(&null, &null, &null, &null, &none), t)?)?,
            None => raw,
        };
        let mut ctx = input.clone();
        let (out, _) = ended(self.list(&self.wf.tasks, "/do".into(), None, input, &mut ctx, &none).await?);
        let out = match self.wf.output.as_ref().and_then(|o| o.as_.as_ref()) {
            Some(t) => self.transform(t, &out, &self.args(&ctx, &null, &null, &null, &none))?,
            None => out,
        };
        if let Some(s) = self.wf.output.as_ref().and_then(|o| o.schema.as_ref()) {
            self.conform(s, &out, "the workflow output")?;
        }
        Ok(out)
    }

    /// `$run` with the given visit counts.
    fn run_argument(&self, visits: Value) -> Value {
        json!({ "jumps": self.jumps.load(Ordering::SeqCst), "maxJumps": self.max_jumps, "visits": visits })
    }

    /// Takes the jump the task at `reference` chose, spending one unit of
    /// the run tree's budget under `<reference>#jump` (or restoring the
    /// decision saved there). `Some(first)`: the budget is spent, and `first`
    /// is the key of the run's first refusal, the one that acts on it.
    fn jump(&self, reference: &str) -> Result<Option<String>, Fault> {
        if self.handler(reference, false).is_some() {
            return Err(Fault::Fatal(WorkflowError {
                instance: Some(reference.into()),
                ..WorkflowError::budget("an on_exceeded handler cannot jump")
            }));
        }
        let key = format!("{reference}#jump");
        let invalid = || Fault::Interrupted(WorkflowError::runtime("invalid budget record"));
        let refused = |r: &Record| match r {
            Record::Budget { granted: true, .. } => Ok(None),
            Record::Budget { granted: false, first: Some(first), .. } => Ok(Some(first.clone())),
            _ => Err(invalid()),
        };
        if let Some(saved) = self.record(&key) {
            return refused(&saved);
        }
        let _one = self.spending.lock().unwrap_or_else(|e| e.into_inner());
        let shared = match self.journal {
            Some(journal) => journal.spend(&key, self.max_jumps).map_err(|e| {
                Fault::Interrupted(WorkflowError::runtime(format!(
                    "spending {key}: {}",
                    self.secrets.text(&e.to_string())
                )))
            })?,
            None => None,
        };
        let decided = match shared {
            Some(record) => {
                let decided = refused(&record)?;
                self.records.lock().unwrap_or_else(|e| e.into_inner()).insert(key, record);
                decided
            }
            None => {
                let spent = self.jumps.load(Ordering::SeqCst);
                let taken = spent < self.max_jumps;
                // Every refusal of the run names its first: an earlier one
                // already does, else this one is it, in its one record.
                let first = (!taken).then(|| {
                    self.records
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .values()
                        .find_map(|r| match r {
                            Record::Budget { granted: false, first, .. } => first.clone(),
                            _ => None,
                        })
                        .unwrap_or_else(|| key.clone())
                });
                let record = Record::Budget { granted: taken, spent: spent + u64::from(taken), first: first.clone() };
                self.save(&key, record)?;
                first
            }
        };
        if decided.is_none() {
            self.jumps.fetch_add(1, Ordering::SeqCst);
        }
        Ok(decided)
    }

    /// The handler `reference` runs in, if any; `enter` counts a task.
    fn handler(&self, reference: &str, enter: bool) -> Option<usize> {
        let mut handlers = self.handlers.lock().unwrap_or_else(|e| e.into_inner());
        let (_, entered) = handlers.iter_mut().find(|(prefix, _)| {
            reference.strip_prefix(prefix.as_str()).is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
        })?;
        if enter {
            *entered += 1;
        }
        Some(*entered)
    }

    /// The jump the task at `reference` chose was refused: `on_exceeded`
    /// decides. The task faults with `budget.exhausted` (catchable), or it
    /// completes with `output` and the run ends with that output or with
    /// what its handler returns. Only the run's first refusal (`first`) acts:
    /// a branch refused at the same moment ends as that one does, so the
    /// handler runs once and recovery and replay end the run the same way.
    async fn exceeded(
        &self,
        reference: &str,
        first: &str,
        output: &Value,
        ctx: &mut Value,
        scope: &Scope,
    ) -> Result<(), Fault> {
        let detail = format!("more than {} jumps in this run tree; a `then` loop did not end", self.max_jumps);
        if matches!(self.wf.budget.on_exceeded, Exceeded::Raise) {
            return Err(WorkflowError::budget(detail).into());
        }
        if first != format!("{reference}#jump") {
            let mut ended = self.exceeded.subscribe();
            let fault = tokio::select! {
                _ = self.cancel.cancelled() => Fault::Cancelled,
                fault = ended.wait_for(Option::is_some) => match fault {
                    Ok(fault) => fault.clone().unwrap_or(Fault::Cancelled),
                    Err(_) => Fault::Cancelled,
                },
            };
            return Err(fault);
        }
        let fault = self.act_on_exceeded(reference, output, ctx, scope).await;
        self.exceeded.send_replace(Some(fault.clone()));
        Err(fault)
    }

    /// `on_exceeded` for the run's first refusal: how it ends the run.
    async fn act_on_exceeded(&self, reference: &str, output: &Value, ctx: &mut Value, scope: &Scope) -> Fault {
        match &self.wf.budget.on_exceeded {
            // Answered by `exceeded` before any branch acts.
            Exceeded::Raise => Fault::Error(WorkflowError::budget("the jump budget is spent")),
            Exceeded::End => {
                self.observe(Event::TaskCompleted { reference, output });
                Fault::Exceeded(output.clone())
            }
            Exceeded::Task(handler) => {
                let at = format!("{reference}{HANDLER}");
                self.handlers.lock().unwrap_or_else(|e| e.into_inner()).push((at.clone(), 0));
                let result = self.task(handler, "on_exceeded", &at, Some(reference), output.clone(), ctx, scope).await;
                self.handlers.lock().unwrap_or_else(|e| e.into_inner()).retain(|(prefix, _)| *prefix != at);
                match result {
                    Ok((out, _)) => {
                        self.observe(Event::TaskCompleted { reference, output });
                        Fault::Exceeded(out)
                    }
                    // The handler is the run's last word: its error ends the
                    // run, whatever encloses the jump.
                    Err(Fault::Error(e) | Fault::Fatal(e)) => {
                        self.handler_failed.store(true, Ordering::SeqCst);
                        Fault::Fatal(e)
                    }
                    Err(other) => other,
                }
            }
        }
    }

    /// Runs `tasks` in order, following each task's flow directive.
    fn list<'s>(
        &'s self,
        tasks: &'s TaskList,
        at: String,
        parent: Option<&'s str>,
        input: Value,
        ctx: &'s mut Value,
        scope: &'s Scope,
    ) -> BoxFuture<'s, Result<Next, Fault>> {
        Box::pin(async move {
            let mut data = input;
            let mut i = 0;
            let mut visits = vec![0usize; tasks.len()];
            // `$run.visits`: the enclosing lists' counts, this list's on top.
            let outer = scope
                .iter()
                .rev()
                .find(|(n, _)| n == model::RUN)
                .and_then(|(_, run)| run.get("visits").and_then(Value::as_object).cloned())
                .unwrap_or_default();
            while let Some((name, task)) = tasks.get(i) {
                let suffix = if visits[i] == 0 { String::new() } else { format!("[{}]", visits[i]) };
                let reference = format!("{at}/{i}/{name}{suffix}");
                visits[i] += 1;
                let mut counts = outer.clone();
                for ((n, _), v) in tasks.iter().zip(&visits).filter(|(_, v)| **v > 0) {
                    counts.insert(n.clone(), json!(v));
                }
                let mut inner: Scope = scope.iter().filter(|(n, _)| n != model::RUN).cloned().collect();
                inner.insert(0, (model::RUN.into(), self.run_argument(Value::Object(counts))));
                let (out, flow) = self.task(task, name, &reference, parent, data, ctx, &inner).await?;
                data = out;
                i = match flow {
                    Flow::Continue => i + 1,
                    Flow::Exit => return Ok(Next::Exit(data)),
                    Flow::End => return Ok(Next::End(data)),
                    // The task spent the jump's unit before it completed.
                    Flow::Goto(target) => tasks.iter().position(|(n, _)| *n == target).ok_or_else(|| {
                        Fault::Fatal(WorkflowError::runtime(format!("`then: {target}` names no task in {at}")))
                    })?,
                };
            }
            Ok(Next::Done(data))
        })
    }

    /// One task: the limit and cancellation checks, then the data flow,
    /// with a fault located at this task unless an inner one already is.
    #[allow(clippy::too_many_arguments)]
    fn task<'s>(
        &'s self,
        task: &'s Task,
        name: &'s str,
        reference: &'s str,
        parent: Option<&'s str>,
        raw: Value,
        ctx: &'s mut Value,
        scope: &'s Scope,
    ) -> BoxFuture<'s, Result<(Value, Flow), Fault>> {
        Box::pin(async move {
            if self.cancel.is_cancelled() {
                return Err(Fault::Cancelled);
            }
            if self.handler(reference, true).is_some_and(|n| n > HANDLER_TASKS) {
                let e = WorkflowError::budget(format!("an on_exceeded handler may run at most {HANDLER_TASKS} tasks"));
                let e = WorkflowError { instance: Some(reference.into()), ..e };
                self.observe(Event::TaskFaulted { reference, error: &e });
                return Err(Fault::Fatal(e));
            }
            let key = format!("{reference}#task");
            let prior = self.record(&key);
            if let Some(Record::Task { result: Some(result), .. }) = &prior {
                *ctx = result.context.clone();
                return match &result.outcome {
                    StepOutcome::Completed { output, flow } => Ok((output.clone(), flow.clone())),
                    StepOutcome::Faulted { error, fatal: true } => Err(Fault::Fatal(error.clone())),
                    StepOutcome::Faulted { error, fatal: false } => Err(Fault::Error(error.clone())),
                };
            }
            let max = self.host.limits.max_tasks;
            if prior.is_none() && self.tasks_run.fetch_add(1, Ordering::SeqCst) >= max {
                let e =
                    WorkflowError::runtime(format!("more than {max} tasks ran; a loop or a `then` cycle does not end"));
                let e = WorkflowError { instance: Some(reference.into()), ..e };
                self.observe(Event::TaskFaulted { reference, error: &e });
                return Err(Fault::Fatal(e));
            }
            let started_ms = match prior {
                Some(Record::Task { started_ms, .. }) => started_ms,
                _ => self.host.clock.now().timestamp_millis(),
            };
            self.save(&key, Record::Task { started_ms, result: None })?;
            let mut span = TaskSpan { observer: self.host.observer.as_ref(), reference, active: false };
            let result = self.steps(task, name, reference, parent, raw, ctx, scope, started_ms, &mut span).await;
            if let Err(Fault::Waiting(wake_at_ms)) = &result {
                self.observe(Event::TaskSuspended { reference, wake_at_ms: *wake_at_ms });
            }
            if !matches!(result, Err(Fault::Cancelled | Fault::Interrupted(_))) {
                span.active = false;
            }
            let result = match result {
                Err(Fault::Error(mut e)) => {
                    e.instance.get_or_insert_with(|| reference.into());
                    self.observe(Event::TaskFaulted { reference, error: &e });
                    Err(Fault::Error(e))
                }
                Err(Fault::Fatal(mut e)) => {
                    e.instance.get_or_insert_with(|| reference.into());
                    self.observe(Event::TaskFaulted { reference, error: &e });
                    Err(Fault::Fatal(e))
                }
                other => other,
            };
            let outcome = match &result {
                Ok((output, flow)) => Some(StepOutcome::Completed { output: output.clone(), flow: flow.clone() }),
                Err(Fault::Error(error)) => Some(StepOutcome::Faulted { error: error.clone(), fatal: false }),
                Err(Fault::Fatal(error)) => Some(StepOutcome::Faulted { error: error.clone(), fatal: true }),
                _ => None,
            };
            if let Some(outcome) = outcome {
                self.save(
                    &key,
                    Record::Task { started_ms, result: Some(TaskResult { context: ctx.clone(), outcome }) },
                )?;
            }
            result
        })
    }

    /// if → input.schema → input.from → the task with its extensions (under
    /// its timeout) → output.as → output.schema → export.as → export.schema.
    #[allow(clippy::too_many_arguments)]
    async fn steps(
        &self,
        task: &Task,
        name: &str,
        reference: &str,
        parent: Option<&str>,
        raw: Value,
        ctx: &mut Value,
        scope: &Scope,
        started_ms: i64,
        span: &mut TaskSpan<'_>,
    ) -> Result<(Value, Flow), Fault> {
        let null = Value::Null;
        let mut desc = json!({
            "name": name,
            "reference": reference,
            "definition": task.definition,
            "input": raw,
            "startedAt": datetime(DateTime::from_timestamp_millis(started_ms).expect("stored timestamp")),
        });
        if let Some(cond) = &task.when {
            let v =
                expr::eval(expr::bare(cond), &raw, &self.args(ctx, &null, &null, &desc, scope)).map_err(expression)?;
            if !expr::truthy(&v) {
                self.observe(Event::TaskSkipped { reference, parent });
                return Ok((raw, Flow::Continue));
            }
        }
        if let Some(s) = task.input.as_ref().and_then(|i| i.schema.as_ref()) {
            self.conform(s, &raw, "the task input")?;
        }
        let input = match task.input.as_ref().and_then(|i| i.from.as_ref()) {
            Some(t) => self.transform(t, &raw, &self.input_args(self.args(ctx, &null, &null, &desc, scope), t)?)?,
            None => raw,
        };
        let limit = match &task.timeout {
            Some(spec) => Some(self.duration(spec, &input, &self.args(ctx, &input, &null, &desc, scope))?),
            None => None,
        };
        let _deadline = limit.map(|limit| {
            let millis = i64::try_from(limit.as_millis()).unwrap_or(i64::MAX);
            self.deadlines.lock().unwrap().insert(reference.into(), started_ms.saturating_add(millis));
            DeadlineGuard { deadlines: &self.deadlines, reference }
        });
        self.observe(Event::TaskStarted { reference, parent, kind: task.kind.name(), input: &input });
        span.active = true;
        let (extensions, described) = self.extensions(task, reference, &input, ctx, scope, &desc)?;
        let mut around = scope.clone();
        around.push(("task".into(), described));
        macro_rules! run {
            ($deadline:expr) => {
                self.layer(0, &extensions, task, reference, input.clone(), ctx, scope, &around, &desc, $deadline)
            };
        }
        let child_done = matches!(&task.kind, Kind::Run(r) if matches!(r.process, Process::Workflow { .. }))
            && self.record(&format!("{reference}#child")).is_some();
        let (out, flow) = match limit {
            None => run!(None).await?,
            // A child that already reported finishes; its deadline was met.
            Some(_) if child_done => run!(None).await?,
            Some(limit) => {
                let millis = i64::try_from(limit.as_millis()).unwrap_or(i64::MAX);
                let deadline = started_ms.saturating_add(millis);
                if matches!(task.kind, Kind::Listen(_)) {
                    // A listen enforces its deadline itself, only when it needs
                    // a fresh event: events consumed before a crash still
                    // complete it after the deadline.
                    match run!(Some(deadline)).await {
                        Err(Fault::Waiting(wake)) => return Err(Fault::Waiting(wake.min(deadline))),
                        other => other?,
                    }
                } else {
                    let (left, expired) = match self.remaining(limit, started_ms) {
                        Ok((left, _)) => (left, false),
                        Err(_) => (Duration::ZERO, true),
                    };
                    // A body that replays to its end without waiting still
                    // completes past the deadline: the inner future is polled
                    // before the timer. One that would wait has timed out.
                    match tokio::time::timeout(left, run!(None)).await {
                        Ok(Err(Fault::Waiting(wake))) if !expired => return Err(Fault::Waiting(wake.min(deadline))),
                        Ok(r) if !matches!(r, Err(Fault::Waiting(_))) => r?,
                        _ => {
                            if self.has_unknown(reference) {
                                return Err(self.unknown(reference));
                            }
                            return Err(
                                WorkflowError::timeout(format!("{reference} did not finish within {limit:?}")).into()
                            );
                        }
                    }
                }
            }
        };
        desc["output"] = out.clone();
        let out = match task.output.as_ref().and_then(|o| o.as_.as_ref()) {
            Some(t) => self.transform(t, &out, &self.args(ctx, &input, &null, &desc, scope))?,
            None => out,
        };
        if let Some(s) = task.output.as_ref().and_then(|o| o.schema.as_ref()) {
            self.conform(s, &out, "the task output")?;
        }
        if let Some(export) = &task.export {
            if let Some(t) = &export.as_ {
                *ctx = self.transform(t, &out, &self.args(ctx, &input, &out, &desc, scope))?;
            }
            if let Some(s) = &export.schema {
                self.conform(s, ctx, "the exported context")?;
            }
        }
        let flow = flow.or_else(|| task.then.clone()).unwrap_or(Flow::Continue);
        if matches!(flow, Flow::Goto(_))
            && let Some(first) = self.jump(reference)?
        {
            self.exceeded(reference, &first, &out, ctx, scope).await?;
        }
        self.observe(Event::TaskCompleted { reference, output: &out });
        Ok((out, flow))
    }

    /// The extensions that apply to this task, in declaration order (those
    /// extending its kind whose `when` holds), and what `$task` is for them.
    /// CHOICE: in `when` and in the extension's tasks, `$task` describes the
    /// extended task: its descriptor, to which its own definition properties
    /// are added (the DSL's examples write `$task.with.endpoint`, and log
    /// `$task.reference` from a `before` task); the descriptor's names win.
    /// `when` is evaluated on the transformed task input. Tasks of an
    /// extension are not extended.
    #[allow(clippy::type_complexity)]
    fn extensions(
        &self,
        task: &Task,
        reference: &str,
        input: &Value,
        ctx: &Value,
        scope: &Scope,
        desc: &Value,
    ) -> Result<(Vec<(usize, &str, &Extension)>, Value), Fault> {
        if self.wf.uses.extensions.is_empty() || reference.contains(EXTENSION_TASKS) {
            return Ok((Vec::new(), Value::Null));
        }
        let mut described = task.definition.as_object().cloned().unwrap_or_default();
        if let Value::Object(d) = desc {
            described.extend(d.clone());
        }
        let described = Value::Object(described);
        let mut out = Vec::new();
        for (i, (name, e)) in self.wf.uses.extensions.iter().enumerate() {
            if !e.extends(&task.kind) {
                continue;
            }
            if let Some(w) = &e.when {
                let hit = expr::eval(w, input, &self.args(ctx, input, &Value::Null, &described, scope))
                    .map_err(expression)?;
                if !expr::truthy(&hit) {
                    continue;
                }
            }
            out.push((i, name.as_str(), e));
        }
        Ok((out, described))
    }

    /// Extension `i` around the inner layers: its `before` list runs on the
    /// task's input and its output becomes the input inside; its `after`
    /// list runs on the output from inside and its output is the task's.
    /// `then: exit` in a `before` list skips what is inside (the extended
    /// task included) and this extension's `after`: its output is the
    /// task's. `then: end` in either ends the workflow.
    #[allow(clippy::too_many_arguments)]
    fn layer<'s>(
        &'s self,
        i: usize,
        extensions: &'s [(usize, &'s str, &'s Extension)],
        task: &'s Task,
        reference: &'s str,
        input: Value,
        ctx: &'s mut Value,
        scope: &'s Scope,
        around: &'s Scope,
        desc: &'s Value,
        deadline: Option<i64>,
    ) -> BoxFuture<'s, Result<(Value, Option<Flow>), Fault>> {
        Box::pin(async move {
            let Some((index, name, e)) = extensions.get(i) else {
                return self.kind(task, reference, &input, ctx, scope, desc, deadline).await;
            };
            let at = format!("{reference}{EXTENSION_TASKS}{index}/{name}");
            let mut data = input;
            if let Some(before) = &e.before {
                match self.list(before, format!("{at}/before"), Some(reference), data, ctx, around).await? {
                    Next::Exit(v) => return Ok((v, None)),
                    Next::End(v) => return Ok((v, Some(Flow::End))),
                    Next::Done(v) => data = v,
                }
            }
            let (out, flow) =
                self.layer(i + 1, extensions, task, reference, data, ctx, scope, around, desc, deadline).await?;
            match &e.after {
                None => Ok((out, flow)),
                Some(after) => {
                    match self.list(after, format!("{at}/after"), Some(reference), out, ctx, around).await? {
                        Next::End(v) => Ok((v, Some(Flow::End))),
                        Next::Done(v) | Next::Exit(v) => Ok((v, flow)),
                    }
                }
            }
        })
    }

    // ── 4. task kinds ──────────────────────────────────────────────────────

    /// The task's raw output and the directive it chose, if any (a switch
    /// case, `catch.then`, or `end` from inside). `deadline`: a listen's.
    #[allow(clippy::too_many_arguments)]
    async fn kind(
        &self,
        task: &Task,
        reference: &str,
        input: &Value,
        ctx: &mut Value,
        scope: &Scope,
        desc: &Value,
        deadline: Option<i64>,
    ) -> Result<(Value, Option<Flow>), Fault> {
        let args = |ctx: &Value| self.args(ctx, input, &Value::Null, desc, scope);
        match &task.kind {
            Kind::Call { function, with } => {
                let call_args = match with {
                    Some(w) => expr::render(w, input, &args(ctx)).map_err(expression)?,
                    None => Value::Object(Map::new()),
                };
                let target = Target::Function(model::function_name(function));
                Ok((self.call(target, call_args, reference, ctx, task.delivery, input).await?, None))
            }
            Kind::Do(tasks) => Ok(ended(
                self.list(tasks, format!("{reference}/do"), Some(reference), input.clone(), ctx, scope).await?,
            )),
            Kind::For { each, at, items, condition, concurrency, tasks } => {
                let items = match items {
                    Items::Inline(items) => items.clone(),
                    Items::Expr(code) => match expr::eval(code, input, &args(ctx)).map_err(expression)? {
                        Value::Array(items) => items,
                        Value::Null => Vec::new(),
                        other => {
                            return Err(WorkflowError::expression(format!(
                                "`for.in` must produce an array, got {other}"
                            ))
                            .into());
                        }
                    },
                };
                if let Some(limit) = concurrency {
                    return self.concurrent(tasks, items, each, at, *limit, reference, input, ctx, scope).await;
                }
                // Each pass gets the previous pass's output; the first gets the task's input.
                let mut data = input.clone();
                for (i, item) in items.into_iter().enumerate() {
                    let mut inner = scope.clone();
                    inner.push((each.clone(), item));
                    inner.push((at.clone(), json!(i)));
                    if let Some(c) = condition {
                        let go = expr::eval(c, &data, &self.args(ctx, input, &Value::Null, desc, &inner))
                            .map_err(expression)?;
                        if !expr::truthy(&go) {
                            break;
                        }
                    }
                    match self.list(tasks, format!("{reference}[{i}]/do"), Some(reference), data, ctx, &inner).await? {
                        Next::Done(v) | Next::Exit(v) => data = v,
                        Next::End(v) => return Ok((v, Some(Flow::End))),
                    }
                }
                Ok((data, None))
            }
            Kind::Fork { branches, compete } => {
                Ok((self.fork(branches, *compete, reference, input, ctx, scope).await?, None))
            }
            Kind::Set(value) => {
                let out = match value {
                    Value::String(code) => expr::eval(expr::bare(code), input, &args(ctx)),
                    template => expr::render(template, input, &args(ctx)),
                }
                .map_err(expression)?;
                Ok((out, None))
            }
            Kind::Switch(cases) => {
                let a = args(ctx);
                for (name, case) in cases {
                    let hit = match &case.when {
                        None => false,
                        Some(w) => expr::truthy(&expr::eval(w, input, &a).map_err(expression)?),
                    };
                    if hit {
                        self.observe(Event::SwitchCase { reference, case: name });
                        return Ok((input.clone(), Some(case.then.clone())));
                    }
                }
                let default = cases.iter().find(|(_, case)| case.when.is_none()).map(|(name, case)| {
                    self.observe(Event::SwitchCase { reference, case: name });
                    case.then.clone()
                });
                // Without a default, the task's own `then` applies.
                Ok((input.clone(), default))
            }
            Kind::Try { tasks, catch } => self.attempt(tasks, catch, reference, input, ctx, scope, desc).await,
            Kind::Raise(r) => Err(self.raise(r, reference, input, &args(ctx))?.into()),
            Kind::Listen(listen) => self.listen(task, listen, reference, input, ctx, scope, desc, deadline).await,
            Kind::Emit(with) => Ok((self.emit(with, reference, input, &args(ctx)).await?, None)),
            Kind::Run(run) => {
                Ok((self.run_process(run, reference, input, ctx, &args(ctx), task.delivery).await?, None))
            }
            Kind::Wait(spec) => {
                let d = self.duration(spec, input, &args(ctx))?;
                self.sleep(d, &format!("{reference}#wait")).await?;
                Ok((input.clone(), None))
            }
        }
    }

    async fn call(
        &self,
        target: Target<'_>,
        args: Value,
        reference: &str,
        ctx: &mut Value,
        delivery: Delivery,
        input: &Value,
    ) -> Result<Value, Fault> {
        let name = match target {
            Target::Function(name) => name.to_owned(),
            Target::Process(kind) => Process::function(kind),
        };
        let name = name.as_str();
        if let Target::Function(name) = target
            && let Some((_, function)) = self.wf.uses.functions.iter().find(|(n, _)| n == name)
        {
            if delivery != Delivery::AtLeastOnce {
                return Err(Fault::Fatal(WorkflowError::validation(
                    "delivery belongs on the external call inside a reusable function",
                )));
            }
            let none = Scope::new();
            let (out, _) = self
                .task(function, name, &format!("{reference}/use/functions/{name}"), Some(reference), args, ctx, &none)
                .await?;
            return Ok(out);
        }
        let delivery = if self.host.functions.at_most_once(name, &args) { Delivery::AtMostOnce } else { delivery };
        let key = format!("{reference}#call");
        let logical = self.logical(reference);
        let prior = self.record(&key);
        if let Some(Record::Call { logical: old_logical, function, arguments, delivery: old_delivery, state }) = &prior
        {
            if old_logical != &logical || function != name || arguments != &args || *old_delivery != delivery {
                return Err(Fault::Fatal(WorkflowError::runtime(format!("{reference}: call bindings changed"))));
            }
            match state {
                CallState::Done(result) => return result.clone().map_err(Fault::Error),
                CallState::Pending if delivery == Delivery::AtMostOnce => {
                    return self.unknown_call(&key, logical, name, args, reference);
                }
                _ => {}
            }
        } else if prior.is_some() {
            return Err(Fault::Interrupted(WorkflowError::runtime("invalid call record")));
        }
        let known = match target {
            Target::Function(_) => self.host.functions.has(name),
            Target::Process(kind) => self.host.functions.process(kind),
        };
        if !known {
            return Err(Fault::Fatal(WorkflowError::runtime(format!("unknown function `{name}`"))));
        }
        // A retry attempt of a protected (at-most-once) call: an earlier
        // attempt's success stands, so the call is not sent again. Any
        // at-least-once call runs again, whatever it does to the world.
        if prior.is_none()
            && logical != reference
            && delivery == Delivery::AtMostOnce
            && let Some(result) = self.earlier_attempt(&logical, &key, name, &args, reference)
        {
            return result;
        }
        self.save(
            &key,
            Record::Call {
                logical: logical.clone(),
                function: name.into(),
                arguments: args.clone(),
                delivery,
                state: CallState::Pending,
            },
        )?;
        if delivery == Delivery::AtMostOnce {
            self.protected.fetch_add(1, Ordering::SeqCst);
        }
        let attempt = self
            .attempts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(prefix, _, _)| {
                reference.strip_prefix(prefix.as_str()).is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
            })
            .max_by_key(|(prefix, _, _)| prefix.len())
            .map_or(1, |(_, _, attempt)| *attempt);
        let info = CallInfo {
            run_id: self.run_id,
            reference,
            logical: &logical,
            attempt,
            cancel: self.cancel,
            delivery,
            input,
            secrets: &self.secrets,
        };
        let cooperative = self.host.functions.cooperative(name);
        let functions = &self.host.functions;
        let info = &info;
        let dispatch = move |args: Value, durable: bool| async move {
            match target {
                Target::Process(kind) => functions.run_process(kind, args, info).await,
                Target::Function(_) if durable => functions.invoke(name, args, info).await,
                Target::Function(_) => functions.execute(name, args, info).await,
            }
        };
        if self.journal.is_none() {
            let result = if cooperative {
                dispatch(args, false).await
            } else {
                tokio::select! {
                    _ = self.cancel.cancelled() => return Err(Fault::Cancelled),
                    r = dispatch(args, false) => r,
                }
            };
            if self.cancel.is_cancelled() {
                return Err(Fault::Cancelled);
            }
            // No journal, so no record to save: the call journal still
            // learns how the call ended.
            let state = match &result {
                Ok(value) => Some(CallState::Done(Ok(value.clone()))),
                Err(CallFailure::Response(error)) => Some(CallState::Done(Err(error.clone()))),
                Err(CallFailure::NotExecuted(error)) => Some(CallState::NotExecuted(error.clone())),
                Err(CallFailure::Unknown(_) | CallFailure::Interrupted(_)) => None,
            };
            if let Some(state) = state {
                let record = Record::Call { logical, function: name.into(), arguments: Value::Null, delivery, state };
                self.note(&key, &record);
            }
            return result.map_err(|failure| match failure {
                CallFailure::Interrupted(error) => Fault::Interrupted(error),
                CallFailure::Response(error) | CallFailure::NotExecuted(error) | CallFailure::Unknown(error) => {
                    Fault::Error(error)
                }
            });
        }
        let result = if cooperative {
            dispatch(args.clone(), true).await
        } else {
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return Err(Fault::Cancelled),
                r = dispatch(args.clone(), true) => r,
            }
        };
        let (state, result) = match result {
            Ok(value) => (CallState::Done(Ok(value.clone())), Ok(value)),
            Err(CallFailure::Response(error)) => (CallState::Done(Err(error.clone())), Err(Fault::Error(error))),
            Err(CallFailure::NotExecuted(error)) => (CallState::NotExecuted(error.clone()), Err(Fault::Error(error))),
            Err(CallFailure::Unknown(error)) => {
                if delivery == Delivery::AtMostOnce {
                    return self.unknown_call(&key, logical, name, args, reference);
                }
                // A transient transport failure remains retryable, including after restart.
                return Err(Fault::Error(error));
            }
            // A stop, not an answer: the pending record stays, so recovery
            // calls it again. Explicit cancellation of the run still wins,
            // and a stop is no proof that an at-most-once call did nothing.
            Err(CallFailure::Interrupted(error)) => {
                if self.cancel.is_cancelled() {
                    return Err(Fault::Cancelled);
                }
                if delivery == Delivery::AtMostOnce {
                    return self.unknown_call(&key, logical, name, args, reference);
                }
                return Err(Fault::Interrupted(error));
            }
        };
        self.save(&key, Record::Call { logical, function: name.into(), arguments: args, delivery, state })?;
        result
    }

    /// A concurrent `for`: every iteration runs on the input and a copy of
    /// the context (its exports stay in it), at most `limit` at once. All of
    /// them run to their end, so none is dropped mid-call; then the first
    /// fault in item order wins, else the earliest wait, else the outputs in
    /// item order (with `end` when an iteration ended the workflow).
    #[allow(clippy::too_many_arguments)]
    async fn concurrent(
        &self,
        tasks: &TaskList,
        items: Vec<Value>,
        each: &str,
        at: &str,
        limit: usize,
        reference: &str,
        input: &Value,
        ctx: &Value,
        scope: &Scope,
    ) -> Result<(Value, Option<Flow>), Fault> {
        let runs = items.into_iter().enumerate().map(|(i, item)| {
            let mut inner = scope.clone();
            inner.push((each.to_owned(), item));
            inner.push((at.to_owned(), json!(i)));
            let mut ctx = ctx.clone();
            let input = input.clone();
            async move {
                let next =
                    self.list(tasks, format!("{reference}[{i}]/do"), Some(reference), input, &mut ctx, &inner).await;
                (i, next)
            }
        });
        let mut done: Vec<_> = stream::iter(runs).buffer_unordered(limit).collect().await;
        done.sort_by_key(|(i, _)| *i);
        let mut output = Vec::with_capacity(done.len());
        let mut wake: Option<i64> = None;
        let mut end = None;
        for (_, next) in done {
            match next {
                Ok(Next::Done(v) | Next::Exit(v)) => output.push(v),
                Ok(Next::End(v)) => {
                    output.push(v);
                    end = Some(Flow::End);
                }
                Err(Fault::Waiting(deadline)) => wake = Some(wake.map_or(deadline, |old| old.min(deadline))),
                Err(stop) => return Err(stop),
            }
        }
        match wake {
            Some(deadline) => Err(Fault::Waiting(deadline)),
            None => Ok((Value::Array(output), end)),
        }
    }

    /// An at-most-once call that may have run: the host's marker for it,
    /// saved as its result, or a fatal fault when the host has none.
    fn unknown_call(
        &self,
        key: &str,
        logical: String,
        name: &str,
        args: Value,
        reference: &str,
    ) -> Result<Value, Fault> {
        let Some(marker) = self.host.functions.unknown_outcome(name, &args) else {
            return Err(self.unknown(reference));
        };
        if !matches!(self.record(key), Some(Record::Call { .. })) {
            return Err(Fault::Interrupted(WorkflowError::runtime("missing call record")));
        }
        self.save(
            key,
            Record::Call {
                logical,
                function: name.into(),
                arguments: args,
                delivery: Delivery::AtMostOnce,
                state: CallState::Done(Ok(marker.clone())),
            },
        )?;
        Ok(marker)
    }

    /// Branches run concurrently, each on the fork's input and a copy of the
    /// context as it was at the fork: what a branch exports stays in it.
    async fn fork(
        &self,
        branches: &TaskList,
        compete: bool,
        fork: &str,
        input: &Value,
        ctx: &Value,
        scope: &Scope,
    ) -> Result<Value, Fault> {
        let winner_key = format!("{fork}#winner");
        if let Some(Record::Winner { output }) = self.record(&winner_key) {
            return Ok(output);
        }
        let runs = branches.iter().enumerate().map(|(i, (name, branch))| {
            let reference = format!("{fork}/fork/branches/{i}/{name}");
            let mut ctx = ctx.clone();
            let input = input.clone();
            async move { self.task(branch, name, &reference, Some(fork), input, &mut ctx, scope).await.map(|(v, _)| v) }
        });
        if !compete {
            if self.journal.is_none() {
                return Ok(Value::Array(try_join_all(runs).await?));
            }
            let mut pending: FuturesUnordered<_> =
                runs.enumerate().map(|(i, run)| async move { (i, run.await) }).collect();
            let mut output = vec![Value::Null; branches.len()];
            let mut wake: Option<i64> = None;
            while let Some((i, result)) = pending.next().await {
                match result {
                    Ok(value) => output[i] = value,
                    Err(Fault::Waiting(deadline)) => wake = Some(wake.map_or(deadline, |old| old.min(deadline))),
                    Err(error) => {
                        if self.has_unknown(fork) && !matches!(error, Fault::Interrupted(_)) {
                            return Err(self.unknown(fork));
                        }
                        return Err(error);
                    }
                }
            }
            return match wake {
                Some(deadline) => Err(Fault::Waiting(deadline)),
                None => Ok(Value::Array(output)),
            };
        }
        // The first branch to complete wins and the others are dropped; the
        // fork faults only when every branch has.
        let mut racing: FuturesUnordered<_> = runs.collect();
        let mut last = None;
        let mut wake: Option<i64> = None;
        while let Some(r) = racing.next().await {
            match r {
                Ok(v) => {
                    self.save(&winner_key, Record::Winner { output: v.clone() })?;
                    return Ok(v);
                }
                Err(Fault::Waiting(deadline)) => wake = Some(wake.map_or(deadline, |old| old.min(deadline))),
                Err(Fault::Error(e)) => last = Some(e),
                Err(stop) => return Err(stop),
            }
        }
        if let Some(deadline) = wake {
            return Err(Fault::Waiting(deadline));
        }
        Err(last.unwrap_or_else(|| WorkflowError::runtime("a competing fork needs a branch")).into())
    }

    fn raise(&self, r: &Raise, reference: &str, input: &Value, args: &Args) -> Result<WorkflowError, Fault> {
        let def = match r {
            Raise::Inline(d) => d,
            Raise::Named(name) => {
                self.wf.uses.errors.iter().find(|(n, _)| n == name).map(|(_, d)| d).ok_or_else(|| {
                    Fault::Fatal(WorkflowError::runtime(format!("no error `{name}` under use.errors")))
                })?
            }
        };
        let text = |t: &Option<String>| -> Result<Option<String>, Fault> {
            let Some(s) = t else { return Ok(None) };
            let Some(code) = expr::strip(s) else {
                return Ok(Some(s.clone()));
            };
            Ok(match expr::eval(code, input, args).map_err(expression)? {
                Value::Null => None,
                Value::String(s) => Some(s),
                other => Some(other.to_string()),
            })
        };
        Ok(WorkflowError {
            type_: def.type_.clone(),
            status: def.status,
            title: text(&def.title)?,
            detail: text(&def.detail)?,
            instance: Some(def.instance.clone().unwrap_or_else(|| reference.to_string())),
        })
    }

    // ── 5. events: listen and emit ─────────────────────────────────────────

    /// Consumes events until the strategy is satisfied: each one taken under
    /// its own key (`<task>#event[i]`) atomically with its record, offered to
    /// a consumer (`events::Consumer`), and, with `foreach`, processed before
    /// the next is taken. On recovery the saved events are offered again in
    /// order, so the consumer, the output and the iterations replay; only
    /// then is a fresh event taken. The subscription stays open from the
    /// first take to the end, so nothing published in between is missed.
    /// CHOICE: an event of a routed type that no filter wants is consumed
    /// and dropped (`Offer::Ignored`), as a broadcast subscriber would.
    #[allow(clippy::too_many_arguments)]
    async fn listen(
        &self,
        task: &Task,
        listen: &Listen,
        reference: &str,
        input: &Value,
        ctx: &mut Value,
        scope: &Scope,
        desc: &Value,
        deadline: Option<i64>,
    ) -> Result<(Value, Option<Flow>), Fault> {
        let events = self
            .host
            .functions
            .events()
            .ok_or_else(|| WorkflowError::validation("events are not supported by this host"))?;
        let routes = events::routes(&listen.to, &self.args(ctx, input, &Value::Null, desc, scope));
        let storage = |e: anyhow::Error| Fault::Interrupted(WorkflowError::runtime(e.to_string()));
        let condition = match &listen.to {
            Strategy::Any { until: Some(Until::Condition(c)), .. } => Some(c.as_str()),
            _ => None,
        };
        let mut consumer = events::Consumer::new(&listen.to);
        let mut consumed = Vec::new();
        let mut outputs = Vec::new();
        let mut subscribed = false;
        let mut i = 0;
        while !consumer.done() {
            let key = format!("{reference}#event[{i}]");
            i += 1;
            let event = match self.record(&key) {
                Some(Record::Event { event }) => event,
                Some(_) => return Err(Fault::Interrupted(WorkflowError::runtime("invalid event record"))),
                None => {
                    if !subscribed {
                        let typed: Vec<String> = routes.iter().filter(|r| *r != "*").cloned().collect();
                        if !typed.is_empty() {
                            events.register(self.run_id, &typed).map_err(storage)?;
                        }
                        events.subscribe(self.run_id, reference, &routes).map_err(storage)?;
                        subscribed = true;
                    }
                    let wait = events::PendingWait {
                        types: routes.clone(),
                        strategy: listen.to.clone(),
                        input: input.clone(),
                        args: self.args(ctx, input, &Value::Null, desc, scope),
                        progress: consumer.progress(),
                        position: key.clone(),
                        since_ms: self.host.clock.now().timestamp_millis(),
                        timeout_ms: self
                            .deadlines
                            .lock()
                            .unwrap()
                            .iter()
                            .filter(|(ancestor, _)| {
                                ancestor.is_empty()
                                    || reference == ancestor.as_str()
                                    || reference
                                        .strip_prefix(ancestor.as_str())
                                        .is_some_and(|rest| rest.starts_with('/'))
                            })
                            .map(|(_, at)| *at)
                            .chain(deadline)
                            .min(),
                        redacted: false,
                    };
                    let raw = serde_json::to_value(&wait).map_err(|e| storage(e.into()))?;
                    let redacted = self.secrets.value(&raw);
                    let mut wait: events::PendingWait =
                        serde_json::from_value(redacted.clone()).map_err(|e| storage(e.into()))?;
                    wait.redacted = raw != redacted;
                    events.waiting(self.run_id, reference, &wait).map_err(storage)?;
                    self.next_event(events, task, reference, &key, &routes, input, ctx, scope, desc, deadline).await?
                }
            };
            let args = self.args(ctx, input, &Value::Null, desc, scope);
            if consumer.offer(&event, input, &args).map_err(expression)? != Offer::Consumed {
                continue;
            }
            let item = events::read(&event, listen.read);
            consumed.push(item.clone());
            if let Some(f) = &listen.foreach {
                let n = outputs.len();
                match self.foreach(f, reference, n, item, ctx, scope).await? {
                    (out, None) => outputs.push(out),
                    (out, Some(flow)) => {
                        events.unsubscribe(self.run_id, reference).map_err(storage)?;
                        return Ok((out, Some(flow)));
                    }
                }
            }
            if let Some(c) = condition {
                let args = self.args(ctx, input, &Value::Null, desc, scope);
                if expr::truthy(&expr::eval(c, &Value::Array(consumed.clone()), &args).map_err(expression)?) {
                    consumer.stop();
                }
            }
        }
        events.unsubscribe(self.run_id, reference).map_err(storage)?;
        Ok((Value::Array(if listen.foreach.is_some() { outputs } else { consumed }), None))
    }

    /// One event for `key`: available now, or delivered within the task's
    /// hold. Otherwise the run suspends until a delivery wakes it, or the
    /// listen times out: its deadline has come, or the host's ceiling
    /// (`Limits::max_listen`) cut the hold short of it.
    #[allow(clippy::too_many_arguments)]
    async fn next_event(
        &self,
        events: &dyn Events,
        task: &Task,
        reference: &str,
        key: &str,
        routes: &[String],
        input: &Value,
        ctx: &Value,
        scope: &Scope,
        desc: &Value,
        deadline: Option<i64>,
    ) -> Result<Value, Fault> {
        let storage = |e: anyhow::Error| Fault::Interrupted(WorkflowError::runtime(e.to_string()));
        let expire = |detail: String| -> Fault {
            let typed: Vec<String> = routes.iter().filter(|r| *r != "*").cloned().collect();
            if let Err(e) = events.unregister(self.run_id, key, &typed) {
                return storage(e);
            }
            if let Err(e) = events.unsubscribe(self.run_id, reference) {
                return storage(e);
            }
            WorkflowError::timeout(detail).into()
        };
        if self.journal.is_none() {
            if let Some(event) = events.take(self.run_id, key, routes).map_err(storage)? {
                return Ok(event);
            }
            return Err(WorkflowError::runtime("listen requires a durable journal; cannot wait in a dry run").into());
        }
        let now = self.host.clock.now().timestamp_millis();
        let mut hold = task.hold;
        let mut expires = None;
        if let Some(at) = deadline {
            if now >= at {
                return Err(expire(format!("{reference}: no event before its deadline")));
            }
            hold = hold.min(Duration::from_millis(u64::try_from(at - now).unwrap_or(0)));
            if let Some(max) = self.host.limits.max_listen.filter(|max| *max <= hold) {
                hold = max;
                let limit = match &task.timeout {
                    Some(spec) => {
                        Some(self.duration(spec, input, &self.args(ctx, input, &Value::Null, desc, scope))?)
                    }
                    None => None,
                };
                expires = Some(limit);
            }
        }
        let until = tokio::time::Instant::now() + hold;
        loop {
            let notification = events.notification(self.run_id);
            if let Some(event) = events.take(self.run_id, key, routes).map_err(storage)? {
                self.save(key, Record::Event { event: event.clone() })?;
                return Ok(event);
            }
            if tokio::time::Instant::now() >= until {
                break;
            }
            tokio::select! {
                _ = notification => {},
                _ = tokio::time::sleep_until(until) => break,
            }
        }
        if let Some(limit) = expires {
            return Err(expire(format!(
                "no event within {hold:?}, the most this host waits (the timeout allows {limit:?})"
            )));
        }
        Err(Fault::Waiting(NO_DEADLINE))
    }

    /// `listen.foreach` for the `n`th consumed event: its tasks on the item,
    /// then `output.as` (the item's output) and `export.as` (the context).
    /// The directive is `end` when an iteration ended the workflow.
    async fn foreach(
        &self,
        f: &Foreach,
        reference: &str,
        n: usize,
        item: Value,
        ctx: &mut Value,
        scope: &Scope,
    ) -> Result<(Value, Option<Flow>), Fault> {
        let mut inner = scope.clone();
        inner.push((f.item.clone(), item.clone()));
        inner.push((f.at.clone(), json!(n)));
        let (out, flow) = ended(
            self.list(&f.tasks, format!("{reference}/foreach/{n}/do"), Some(reference), item.clone(), ctx, &inner)
                .await?,
        );
        let null = Value::Null;
        let out = match f.output.as_ref().and_then(|o| o.as_.as_ref()) {
            Some(t) => self.transform(t, &out, &self.args(ctx, &item, &null, &null, &inner))?,
            None => out,
        };
        if let Some(s) = f.output.as_ref().and_then(|o| o.schema.as_ref()) {
            self.conform(s, &out, "the iteration output")?;
        }
        if let Some(export) = &f.export {
            if let Some(t) = &export.as_ {
                *ctx = self.transform(t, &out, &self.args(ctx, &item, &out, &null, &inner))?;
            }
            if let Some(s) = &export.schema {
                self.conform(s, ctx, "the exported context")?;
            }
        }
        Ok((out, flow))
    }

    /// Publishes the event through the host, recorded like a write call
    /// delivered at least once: the envelope (its generated `id` and `time`
    /// included) is saved before it is handed over, and recovery hands over
    /// that same envelope again, which receivers deduplicate by `id`. A
    /// retry attempt reuses an earlier attempt's emitted event. The output
    /// is the envelope.
    async fn emit(
        &self,
        with: &Map<String, Value>,
        reference: &str,
        input: &Value,
        args: &Args,
    ) -> Result<Value, Fault> {
        const EMIT: &str = "emit";
        let key = format!("{reference}#call");
        let logical = self.logical(reference);
        let prior = self.record(&key);
        let envelope = match &prior {
            Some(Record::Call { function, state: CallState::Done(result), .. }) if function == EMIT => {
                return result.clone().map_err(Fault::Error);
            }
            Some(Record::Call { function, arguments, .. }) if function == EMIT => arguments.clone(),
            Some(_) => return Err(Fault::Interrupted(WorkflowError::runtime("invalid emit record"))),
            None => {
                if logical != reference
                    && let Some(earlier) =
                        self.records.lock().unwrap_or_else(|e| e.into_inner()).values().find_map(|r| match r {
                            Record::Call { logical: l, function, state: CallState::Done(Ok(v)), .. }
                                if *l == logical && function == EMIT =>
                            {
                                Some(v.clone())
                            }
                            _ => None,
                        })
                {
                    return Ok(earlier);
                }
                let rendered = expr::render(&Value::Object(with.clone()), input, args).map_err(expression)?;
                let document = &self.wf.document;
                let defaults = events::Defaults {
                    id: uuid::Uuid::new_v4().to_string(),
                    source: format!("/{}/{}/{}", document.namespace, document.name, document.version),
                    time: self.host.clock.now().to_rfc3339_opts(SecondsFormat::Millis, true),
                };
                let Value::Object(rendered) = rendered else {
                    return Err(expression("the emitted event's attributes must be an object".into()));
                };
                events::envelope(rendered, defaults).map_err(expression)?
            }
        };
        let pending = |state| Record::Call {
            logical: logical.clone(),
            function: EMIT.into(),
            arguments: envelope.clone(),
            delivery: Delivery::AtLeastOnce,
            state,
        };
        self.save(&key, pending(CallState::Pending))?;
        let info = CallInfo {
            run_id: self.run_id,
            reference,
            logical: &logical,
            attempt: 1,
            cancel: self.cancel,
            delivery: Delivery::AtLeastOnce,
            input,
            secrets: &self.secrets,
        };
        let result = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => return Err(Fault::Cancelled),
            r = self.host.functions.emit(&envelope, &info) => r,
        };
        match result {
            Ok(()) => {
                self.save(&key, pending(CallState::Done(Ok(envelope.clone()))))?;
                Ok(envelope)
            }
            Err(CallFailure::Response(e)) => {
                self.save(&key, pending(CallState::Done(Err(e.clone()))))?;
                Err(Fault::Error(e))
            }
            Err(CallFailure::NotExecuted(e)) => {
                self.save(&key, pending(CallState::NotExecuted(e.clone())))?;
                Err(Fault::Error(e))
            }
            // Delivered at least once: recovery hands the same envelope over.
            Err(CallFailure::Unknown(e)) => Err(Fault::Error(e)),
            Err(CallFailure::Interrupted(e)) if self.cancel.is_cancelled() => {
                let _ = e;
                Err(Fault::Cancelled)
            }
            Err(CallFailure::Interrupted(e)) => Err(Fault::Interrupted(e)),
        }
    }

    // ── 6. child workflows and processes ───────────────────────────────────

    /// `run`: a child workflow through `Children`, or a process through the
    /// host's runner, recorded like a function call. Without
    /// `await` the task outputs its input.
    async fn run_process(
        &self,
        run: &RunTask,
        reference: &str,
        input: &Value,
        ctx: &mut Value,
        args: &Args,
        delivery: Delivery,
    ) -> Result<Value, Fault> {
        let (namespace, name, version, template) = match &run.process {
            Process::External { kind, definition } => {
                let mut definition = expr::render(definition, input, args).map_err(expression)?;
                if let Value::Object(d) = &mut definition {
                    d.insert("await".into(), Value::Bool(run.await_));
                }
                let result = self.call(Target::Process(kind), definition, reference, ctx, delivery, input).await?;
                if !run.await_ {
                    return Ok(input.clone());
                }
                return Ok(match run.return_ {
                    Return::Stdout => result.get("stdout").cloned().unwrap_or(Value::Null),
                    Return::Stderr => result.get("stderr").cloned().unwrap_or(Value::Null),
                    Return::Code => result.get("code").cloned().unwrap_or(Value::Null),
                    Return::All => result,
                    Return::None => Value::Null,
                });
            }
            Process::Workflow { namespace, name, version, input } => (namespace, name, version, input),
        };
        let child_input = match template {
            Some(t) => expr::render(t, input, args).map_err(expression)?,
            None => Value::Object(Map::new()),
        };
        let target = ChildTarget { namespace, name, version };
        let children = self
            .host
            .functions
            .children()
            .ok_or_else(|| WorkflowError::validation("child workflows are not supported by this host"))?;
        let failed = |f: CallFailure| match f {
            CallFailure::NotExecuted(e) | CallFailure::Response(e) => Fault::Error(e),
            CallFailure::Unknown(e) | CallFailure::Interrupted(e) => Fault::Interrupted(e),
        };
        if !run.await_ {
            children.start(self.run_id, reference, &target, &child_input, false).map_err(failed)?;
            return Ok(input.clone());
        }
        let key = format!("{reference}#child");
        let event = match self.record(&key) {
            Some(Record::Event { event }) => event,
            Some(_) => return Err(Fault::Interrupted(WorkflowError::runtime("invalid child record"))),
            None => {
                if self.journal.is_none() {
                    return Err(WorkflowError::runtime("a child workflow needs a durable journal").into());
                }
                let events = self
                    .host
                    .functions
                    .events()
                    .ok_or_else(|| WorkflowError::validation("child workflows need events on this host"))?;
                // The raw reference, not the logical one: a DSL retry starts a
                // new child, while recovery joins the one it started.
                let child = children.start(self.run_id, reference, &target, &child_input, true).map_err(failed)?;
                let storage = |e: anyhow::Error| Fault::Interrupted(WorkflowError::runtime(e.to_string()));
                let types = [child.done];
                events.register(self.run_id, &types).map_err(storage)?;
                match events.take(self.run_id, &key, &types).map_err(storage)? {
                    Some(event) => {
                        self.save(&key, Record::Event { event: event.clone() })?;
                        event
                    }
                    None => return Err(Fault::Waiting(NO_DEADLINE)),
                }
            }
        };
        let data = event.get("data").cloned().unwrap_or(Value::Null);
        let output = match data["status"].as_str() {
            Some("completed") => data.get("output").cloned().unwrap_or(Value::Null),
            Some("faulted") => {
                // The child's error, located at this task: its own instance
                // points into the child's definition.
                let mut error: WorkflowError = serde_json::from_value(data["error"].clone())
                    .unwrap_or_else(|_| WorkflowError::runtime("the child workflow faulted"));
                error.instance = Some(reference.into());
                return Err(error.into());
            }
            _ => {
                return Err(WorkflowError {
                    instance: Some(reference.into()),
                    ..WorkflowError::standard("runtime", 499, "Child Cancelled", "the child workflow was cancelled")
                }
                .into());
            }
        };
        // CHOICE: a workflow has no streams; it completes with code 0 and
        // its output stands for stdout.
        Ok(match run.return_ {
            Return::Stdout => output,
            Return::Stderr => json!(""),
            Return::Code => json!(0),
            Return::All => json!({ "code": 0, "stdout": output, "stderr": "" }),
            Return::None => Value::Null,
        })
    }

    // ── 7. try, retries and durations ──────────────────────────────────────

    /// A retry restores the original context and gets its own invocation path.
    /// In a durable run a protected call keeps the first attempt's identity
    /// (`logical`), so one that succeeded is not repeated; ephemeral runs
    /// keep no results and so never retry an attempt that started one.
    #[allow(clippy::too_many_arguments)]
    async fn attempt(
        &self,
        tasks: &TaskList,
        catch: &Catch,
        reference: &str,
        input: &Value,
        ctx: &mut Value,
        scope: &Scope,
        desc: &Value,
    ) -> Result<(Value, Option<Flow>), Fault> {
        let began = desc["startedAt"]["epoch"]["milliseconds"]
            .as_i64()
            .ok_or_else(|| Fault::Fatal(WorkflowError::runtime("missing retry start time")))?;
        let snapshot = ctx.clone();
        let mut attempt: u32 = 1;
        loop {
            let protected = self.protected.load(Ordering::SeqCst);
            let attempt_ref = if self.journal.is_none() || attempt == 1 {
                format!("{reference}/try")
            } else {
                let attempt_ref = format!("{reference}/try[{attempt}]");
                attempt_ref
            };
            self.attempts.lock().unwrap_or_else(|e| e.into_inner()).push((
                attempt_ref.clone(),
                format!("{reference}/try"),
                attempt,
            ));
            let body = self.list(tasks, attempt_ref.clone(), Some(reference), input.clone(), ctx, scope);
            let result = match catch.retry.as_ref().and_then(|r| r.attempt_duration.as_ref()) {
                None => body.await,
                Some(spec) => {
                    let limit = self.duration(spec, input, &self.args(&snapshot, input, &Value::Null, desc, scope))?;
                    self.within(limit, &format!("{reference}#attempt[{attempt}]"), &attempt_ref, body).await
                }
            };
            let error = match result {
                Ok(next) => return Ok(ended(next)),
                Err(Fault::Error(e)) => e,
                Err(stop) => return Err(stop),
            };
            let mut caught = scope.clone();
            caught.push((catch.as_.clone(), error.to_value()));
            let args = self.args(ctx, input, &Value::Null, desc, &caught);
            if !self.catches(catch, &error, input, &args)? {
                return Err(error.into());
            }
            let decision_key = format!("{reference}#retry-decision[{attempt}]");
            let delay = match self.record(&decision_key) {
                Some(Record::Retry { delay_ms }) => delay_ms.map(Duration::from_millis),
                Some(_) => {
                    return Err(Fault::Interrupted(WorkflowError::runtime("invalid retry record")));
                }
                None => {
                    let safe = if self.journal.is_some() {
                        !self.blocks_retry(&attempt_ref)
                    } else {
                        self.protected.load(Ordering::SeqCst) == protected
                    };
                    let delay = match &catch.retry {
                        Some(retry) if safe => self.retry_delay(retry, attempt, began, input, &args)?,
                        _ => None,
                    };
                    self.save(
                        &decision_key,
                        Record::Retry { delay_ms: delay.map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)) },
                    )?;
                    delay
                }
            };
            if let Some(delay) = delay {
                self.observe(Event::TaskRetrying { reference, attempt, delay, error: &error });
                self.sleep(delay, &format!("{reference}#retry[{attempt}]")).await?;
                *ctx = snapshot.clone();
                attempt += 1;
                continue;
            }
            return match &catch.tasks {
                Some(handler) => {
                    let (out, flow) = ended(
                        self.list(
                            handler,
                            format!("{reference}/catch/do"),
                            Some(reference),
                            input.clone(),
                            ctx,
                            &caught,
                        )
                        .await?,
                    );
                    Ok((out, flow.or_else(|| catch.then.clone())))
                }
                None => Ok((input.clone(), catch.then.clone())),
            };
        }
    }

    /// One attempt bounded by `limit.attempt.duration`. Its deadline is saved
    /// at `key` when the attempt starts, so recovery keeps it; an attempt
    /// past it fails with a timeout error the same `catch` sees (an attempt
    /// that replays without waiting still completes: the body is polled
    /// before the timer).
    async fn within<T>(
        &self,
        limit: Duration,
        key: &str,
        attempt: &str,
        body: impl std::future::Future<Output = Result<T, Fault>>,
    ) -> Result<T, Fault> {
        let deadline_ms = match self.record(key) {
            Some(Record::Timer { deadline_ms }) => deadline_ms,
            Some(_) => return Err(Fault::Interrupted(WorkflowError::runtime("invalid attempt record"))),
            None => {
                let deadline_ms = self
                    .host
                    .clock
                    .now()
                    .timestamp_millis()
                    .saturating_add(i64::try_from(limit.as_millis()).unwrap_or(i64::MAX));
                self.save(key, Record::Timer { deadline_ms })?;
                deadline_ms
            }
        };
        let left = if self.journal.is_some() {
            deadline_ms.saturating_sub(self.host.clock.now().timestamp_millis()).max(0)
        } else {
            i64::try_from(limit.as_millis()).unwrap_or(i64::MAX)
        };
        let expired = self.journal.is_some() && left == 0;
        match tokio::time::timeout(Duration::from_millis(left as u64), body).await {
            Ok(Err(Fault::Waiting(wake))) if !expired => Err(Fault::Waiting(wake.min(deadline_ms))),
            Ok(r) if !matches!(r, Err(Fault::Waiting(_))) => r,
            _ if self.has_unknown(attempt) => Err(self.unknown(attempt)),
            _ => Err(WorkflowError::timeout(format!("{attempt} did not finish within {limit:?}")).into()),
        }
    }

    /// `catch.errors.with` (each listed property equals the error's; `type`
    /// also by `WorkflowError::has_type`), then `when` / `exceptWhen`.
    fn catches(&self, catch: &Catch, error: &WorkflowError, input: &Value, args: &Args) -> Result<bool, Fault> {
        if let Some(filter) = &catch.errors {
            let e = error.to_value();
            if !filter.iter().all(|(k, v)| match (k.as_str(), v.as_str()) {
                ("type", Some(t)) => error.has_type(t),
                _ => e.get(k) == Some(v),
            }) {
                return Ok(false);
            }
        }
        guards(catch.when.as_deref(), catch.except_when.as_deref(), input, args)
    }

    /// The delay before attempt `attempt + 1`, or `None` when the policy
    /// (or the host's ceiling) says to stop. Jitter adds the middle of its
    /// range, which keeps runs reproducible.
    fn retry_delay(
        &self,
        retry: &Retry,
        attempt: u32,
        began: i64,
        input: &Value,
        args: &Args,
    ) -> Result<Option<Duration>, Fault> {
        if !guards(retry.when.as_deref(), retry.except_when.as_deref(), input, args)? {
            return Ok(None);
        }
        let ceiling = self.host.limits.max_attempts;
        if attempt >= retry.attempts.unwrap_or(ceiling).min(ceiling) {
            return Ok(None);
        }
        let base = match &retry.delay {
            Some(d) => self.duration(d, input, args)?,
            None => Duration::ZERO,
        };
        let factor = match retry.backoff {
            Backoff::Constant => 1,
            Backoff::Linear => attempt,
            Backoff::Exponential => 2u32.saturating_pow(attempt - 1),
        };
        let mut delay = base.saturating_mul(factor);
        if let Some((from, to)) = &retry.jitter {
            delay += (self.duration(from, input, args)? + self.duration(to, input, args)?) / 2;
        }
        if let Some(limit) = &retry.duration
            && Duration::from_millis(self.host.clock.now().timestamp_millis().saturating_sub(began).max(0) as u64)
                + delay
                > self.duration(limit, input, args)?
        {
            return Ok(None);
        }
        Ok(Some(delay))
    }

    fn duration(&self, spec: &DurationSpec, input: &Value, args: &Args) -> Result<Duration, Fault> {
        match spec {
            DurationSpec::Fixed(d) => Ok(*d),
            DurationSpec::Expr(code) => match expr::eval(code, input, args).map_err(expression)? {
                Value::String(s) => model::parse_iso_duration(&s)
                    .ok_or_else(|| expression(format!("`{s}` is not an ISO 8601 duration (e.g. PT30S)"))),
                other => Err(expression(format!("a duration expression must produce an ISO 8601 string, got {other}"))),
            },
        }
    }

    async fn sleep(&self, d: Duration, key: &str) -> Result<(), Fault> {
        let d = self.host.limits.max_sleep.map_or(d, |max| d.min(max));
        if self.journal.is_some() {
            let deadline_ms = match self.record(key) {
                Some(Record::Timer { deadline_ms }) => deadline_ms,
                Some(_) => {
                    return Err(Fault::Interrupted(WorkflowError::runtime("invalid timer record")));
                }
                None => {
                    let deadline_ms = self
                        .host
                        .clock
                        .now()
                        .timestamp_millis()
                        .saturating_add(i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
                    self.save(key, Record::Timer { deadline_ms })?;
                    deadline_ms
                }
            };
            if deadline_ms > self.host.clock.now().timestamp_millis() {
                return Err(Fault::Waiting(deadline_ms));
            }
            return Ok(());
        }
        tokio::select! {
            _ = self.cancel.cancelled() => Err(Fault::Cancelled),
            _ = tokio::time::sleep(d) => Ok(()),
        }
    }
}

/// `when` must hold and `exceptWhen` must not.
fn guards(when: Option<&str>, except_when: Option<&str>, input: &Value, args: &Args) -> Result<bool, Fault> {
    if let Some(w) = when
        && !expr::truthy(&expr::eval(w, input, args).map_err(expression)?)
    {
        return Ok(false);
    }
    if let Some(x) = except_when
        && expr::truthy(&expr::eval(x, input, args).map_err(expression)?)
    {
        return Ok(false);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    type Behaviour = Box<dyn Fn(&Value, usize) -> Result<Value, WorkflowError> + Send + Sync>;

    /// Functions by name: a declared delivery and a behaviour given the
    /// arguments and how many times the function was called before.
    #[derive(Default)]
    struct Fake {
        functions: HashMap<&'static str, (Delivery, Behaviour)>,
        calls: Mutex<Vec<(String, Value, String)>>,
    }

    impl Fake {
        fn with(
            mut self,
            name: &'static str,
            delivery: Delivery,
            f: impl Fn(&Value, usize) -> Result<Value, WorkflowError> + Send + Sync + 'static,
        ) -> Self {
            self.functions.insert(name, (delivery, Box::new(f)));
            self
        }
    }

    #[async_trait]
    impl Functions for Fake {
        fn has(&self, name: &str) -> bool {
            self.functions.contains_key(name)
        }
        fn at_most_once(&self, name: &str, _: &Value) -> bool {
            self.functions.get(name).is_some_and(|(d, _)| *d == Delivery::AtMostOnce)
        }
        async fn call(&self, name: &str, args: Value, info: &CallInfo<'_>) -> Result<Value, WorkflowError> {
            let before = {
                let mut calls = self.calls.lock().unwrap();
                let n = calls.iter().filter(|(c, _, _)| c == name).count();
                calls.push((name.to_string(), args.clone(), info.reference.to_string()));
                n
            };
            (self.functions[name].1)(&args, before)
        }
    }

    #[derive(Default)]
    struct Log(Mutex<Vec<String>>);

    impl Observer for Log {
        fn event(&self, event: &Event<'_>) {
            let line = match event {
                Event::TaskStarted { reference, .. } => format!("start {reference}"),
                Event::TaskSkipped { reference, .. } => format!("skip {reference}"),
                Event::TaskCompleted { reference, .. } => format!("done {reference}"),
                Event::TaskFaulted { reference, .. } => format!("fault {reference}"),
                Event::TaskSuspended { reference, .. } => format!("wait {reference}"),
                Event::SwitchCase { reference, case } => format!("case {reference} {case}"),
                Event::TaskRetrying { reference, attempt, delay, .. } => {
                    format!("retry {reference} #{attempt} after {}s", delay.as_secs())
                }
            };
            self.0.lock().unwrap().push(line);
        }
    }

    impl Log {
        fn lines(&self, prefix: &str) -> Vec<String> {
            self.0.lock().unwrap().iter().filter_map(|l| l.strip_prefix(prefix).map(str::to_string)).collect()
        }
    }

    struct Fixed;
    impl Clock for Fixed {
        fn now(&self) -> DateTime<Utc> {
            DateTime::from_timestamp(1_700_000_000, 0).unwrap()
        }
    }

    struct Setup {
        host: Host,
        fake: Arc<Fake>,
        log: Arc<Log>,
    }

    fn setup(fake: Fake) -> Setup {
        let fake = Arc::new(fake);
        let log = Arc::new(Log::default());
        let host = Host {
            functions: fake.clone(),
            observer: log.clone(),
            clock: Arc::new(Fixed),
            limits: Limits::default(),
            runtime: json!({ "name": "owf-test" }),
        };
        Setup { host, fake, log }
    }

    const HEAD: &str = "document: { dsl: '1.0.4', namespace: test, name: w, version: '0.1.0' }\n";

    async fn exec(s: &Setup, definition: &str, input: Value) -> Outcome {
        let text = if definition.trim_start().starts_with("document:") {
            definition.to_string()
        } else {
            format!("{HEAD}{definition}")
        };
        let wf = model::parse_yaml(&text).unwrap_or_else(|e| panic!("{e}"));
        validate(&wf, &*s.host.functions).unwrap_or_else(|e| panic!("{e}"));
        run(&s.host, &wf, "run-1", input, &CancellationToken::new()).await
    }

    fn completed(o: &Outcome) -> &Value {
        match &o.status {
            Status::Completed(v) => v,
            other => panic!("expected completion, got {other:?}"),
        }
    }

    fn faulted(o: &Outcome) -> &WorkflowError {
        match &o.status {
            Status::Faulted(e) => e,
            other => panic!("expected a fault, got {other:?}"),
        }
    }

    fn yaml(text: &str) -> Value {
        serde_yaml_ng::from_str(text).unwrap()
    }

    // The CTK scenarios (ctk/features/*.feature), the HTTP calls replaced by
    // a fake `http` function.

    #[tokio::test]
    async fn ctk_flow_implicit_and_explicit_sequence() {
        let s = setup(Fake::default());
        let o = exec(&s, "do:\n  - setRed: { set: { colors: '${ .colors + [ \"red\" ] }' } }\n  - setGreen: { set: { colors: '${ .colors + [ \"green\" ] }' } }\n  - setBlue: { set: { colors: '${ .colors + [ \"blue\" ] }' } }\n", json!({})).await;
        assert_eq!(completed(&o), &yaml("colors: [ red, green, blue ]"));

        let s = setup(Fake::default());
        let o = exec(
            &s,
            r#"
do:
  - setRed: { set: { colors: '${ .colors + [ "red" ] }' }, then: setGreen }
  - setBlue: { set: { colors: '${ .colors + [ "blue" ] }' }, then: end }
  - setGreen: { set: { colors: '${ .colors + [ "green" ] }' }, then: setBlue }
"#,
            json!({}),
        )
        .await;
        assert_eq!(completed(&o), &yaml("colors: [ red, green, blue ]"));
        assert_eq!(s.log.lines("start "), ["/do/0/setRed", "/do/2/setGreen", "/do/1/setBlue"]);
    }

    #[tokio::test]
    async fn ctk_set_and_do() {
        let s = setup(Fake::default());
        let o = exec(&s, "do:\n  - setShape: { set: { shape: circle, size: '${ .configuration.size }', fill: '${ .configuration.fill }' } }\n", yaml("configuration: { size: { width: 6, height: 6 }, fill: { red: 69, green: 69, blue: 69 } }")).await;
        assert_eq!(
            completed(&o),
            &yaml("shape: circle\nsize: { width: 6, height: 6 }\nfill: { red: 69, green: 69, blue: 69 }")
        );

        let o = exec(&s, "do:\n  - compositeExample:\n      do:\n        - setRed: { set: { colors: '${ .colors + [\"red\"] }' } }\n        - setGreen: { set: { colors: '${ .colors + [\"green\"] }' } }\n", json!({})).await;
        assert_eq!(completed(&o), &yaml("colors: [ red, green ]"));
    }

    #[tokio::test]
    async fn ctk_for_accumulates_over_iterations() {
        let s = setup(Fake::default());
        let o = exec(
            &s,
            r#"
do:
  - loopColors:
      for: { each: color, in: '.colors' }
      do:
        - markProcessed:
            set:
              processed: '${ { colors: (.processed.colors + [ $color ]), indexes: (.processed.indexes + [ $index ])} }'
"#,
            yaml("colors: [ red, green, blue ]"),
        )
        .await;
        assert_eq!(completed(&o), &yaml("processed: { colors: [ red, green, blue ], indexes: [ 0, 1, 2 ] }"));
        assert_eq!(s.log.lines("start ").len(), 4);
    }

    const SWITCH: &str = r#"
do:
  - switchColor:
      switch:
        - red: { when: '.color == "red"', then: setRed }
        - green: { when: '.color == "green"', then: setGreen }
        - other: { then: setCustomColor }
  - setRed: { set: { colors: '${ .colors + [ "red" ] }' }, then: end }
  - setGreen: { set: { colors: '${ .colors + [ "green" ] }' }, then: end }
  - setCustomColor: { set: { colors: '${ .colors + [ $input.color ] }' } }
"#;

    #[tokio::test]
    async fn ctk_switch_matching_default_and_no_match() {
        let s = setup(Fake::default());
        assert_eq!(completed(&exec(&s, SWITCH, json!({"color": "red"})).await), &json!({"colors": ["red"]}));
        assert_eq!(completed(&exec(&s, SWITCH, json!({"color": "yellow"})).await), &json!({"colors": ["yellow"]}));

        let s = setup(Fake::default());
        let implicit = "do:\n  - switchColor:\n      switch:\n        - red: { when: '.color == \"red\"', then: setRed }\n      then: end\n  - setRed: { set: { colors: [ red ] } }\n";
        assert_eq!(completed(&exec(&s, implicit, json!({"color": "yellow"})).await), &json!({"color": "yellow"}));
        assert_eq!(s.log.lines("start "), ["/do/0/switchColor"]);
    }

    fn petstore() -> Fake {
        Fake::default().with("http", Delivery::AtLeastOnce, |args, _| {
            let uri = args["endpoint"]["uri"].as_str().unwrap_or_default();
            Ok(json!({ "id": if uri.ends_with("/2") { 2 } else { 1 } }))
        })
    }

    #[tokio::test]
    async fn ctk_data_flow_filters_input_and_output() {
        let s = setup(Fake::default());
        let o = exec(
            &s,
            "do:\n  - setPlayerId: { input: { from: .user.claims.subject }, set: { playerId: '${ . }' } }\n",
            yaml("user: { claims: { subject: 6AsnRgGEB0q2O7ux9JXFAw } }"),
        )
        .await;
        assert_eq!(completed(&o), &json!({"playerId": "6AsnRgGEB0q2O7ux9JXFAw"}));

        let s = setup(petstore());
        let o = exec(
            &s,
            r#"
do:
  - getPetById1:
      call: http
      with: { method: get, endpoint: { uri: 'https://petstore.swagger.io/v2/pet/{petId}' } }
      output: { as: .id }
  - getPetById2:
      call: http
      with: { method: get, endpoint: { uri: 'https://petstore.swagger.io/v2/pet/2' } }
      output: { as: '{ ids: [ $input, .id ] }' }
"#,
            json!({"petId": 1}),
        )
        .await;
        assert_eq!(completed(&o), &json!({"ids": [1, 2]}));
        assert_eq!(s.fake.calls.lock().unwrap()[1].2, "/do/1/getPetById2");
    }

    const TRY_PET: &str = r#"
do:
  - tryGetPet:
      try:
        - getPet:
            call: http
            with: { method: get, endpoint: { uri: 'https://petstore.swagger.io/v2/pet/getPetByName/{petName}' } }
      catch:
        errors: { with: { type: 'https://open-workflow-specification.org/spec/1.0.0/errors/communication', status: 404 } }
        as: err
        do:
          - setError: { set: { error: '${ $err }' } }
"#;

    #[tokio::test]
    async fn ctk_try_catches_a_matching_error_and_faults_on_another() {
        let s = setup(
            Fake::default()
                .with("http", Delivery::AtLeastOnce, |_, _| Err(WorkflowError::communication(404, "no such pet"))),
        );
        let o = exec(&s, TRY_PET, json!({"petName": "Milou"})).await;
        let error = &completed(&o)["error"];
        assert_eq!(error["instance"], "/do/0/tryGetPet/try/0/getPet");
        assert_eq!(error["status"], 404);
        assert!(error["type"].is_string() && error["title"].is_string());

        let s = setup(
            Fake::default().with("http", Delivery::AtLeastOnce, |_, _| Err(WorkflowError::communication(503, "down"))),
        );
        let o = exec(&s, TRY_PET, json!({"petName": "Milou"})).await;
        let e = faulted(&o);
        assert_eq!((e.status, e.instance.as_deref()), (503, Some("/do/0/tryGetPet/try/0/getPet")));
        assert_eq!(s.log.lines("fault "), ["/do/0/tryGetPet/try/0/getPet", "/do/0/tryGetPet"]);
    }

    #[tokio::test]
    async fn ctk_fork_compete_has_one_winner_and_join_keeps_order() {
        let s = setup(Fake::default());
        let branches = "branches:\n          - setRed: { set: { colors: '${ .colors + [\"red\"] }' } }\n          - setGreen: { set: { colors: '${ .colors + [\"green\"] }' } }\n          - setBlue: { set: { colors: '${ .colors + [\"blue\"] }' } }\n";
        let o = exec(
            &s,
            &format!("do:\n  - branchWithCompete:\n      fork:\n        compete: true\n        {branches}"),
            json!({}),
        )
        .await;
        assert_eq!(completed(&o)["colors"].as_array().map(Vec::len), Some(1));
        let o = exec(&s, &format!("do:\n  - join:\n      fork:\n        {branches}"), json!({})).await;
        assert_eq!(completed(&o), &json!([{"colors": ["red"]}, {"colors": ["green"]}, {"colors": ["blue"]}]));
    }

    #[tokio::test]
    async fn ctk_raise_inline_error_at_its_reference() {
        let s = setup(Fake::default());
        let o = exec(&s, "do:\n  - raiseError: { raise: { error: { status: 400, type: 'https://open-workflow-specification.org/errors/types/compliance', title: Compliance Error } } }\n", json!({})).await;
        let e = faulted(&o);
        assert_eq!(
            serde_json::to_value(e).unwrap(),
            yaml(
                "status: 400\ntype: https://open-workflow-specification.org/errors/types/compliance\ntitle: Compliance Error\ninstance: /do/0/raiseError"
            )
        );
    }

    // Beyond the CTK.

    #[tokio::test]
    async fn context_export_if_and_workflow_transforms() {
        let s = setup(Fake::default());
        let o = exec(
            &s,
            r#"
input: { from: '{ n: .n }', schema: { document: { type: object, required: [ n ] } } }
output: { as: '{ total: ., seen: $context.seen }' }
do:
  - remember: { set: { v: '${ .n * 2 }' }, export: { as: '{ seen: .v }' } }
  - skipped: { if: '${ .v > 100 }', set: { v: 0 } }
  - sum: { set: '${ .v + $context.seen }' }
"#,
            json!({"n": 3, "noise": true}),
        )
        .await;
        assert_eq!(completed(&o), &json!({"total": 12, "seen": 6}));
        assert_eq!(s.log.lines("skip "), ["/do/1/skipped"]);

        let o = exec(
            &s,
            "input: { schema: { document: { type: object, required: [ n ] } } }\ndo: [ { a: { set: { x: 1 } } } ]\n",
            json!({}),
        )
        .await;
        assert!(faulted(&o).is("validation"));
    }

    #[tokio::test]
    async fn reusable_functions_run_as_tasks_on_the_call_arguments() {
        let s =
            setup(Fake::default().with("send", Delivery::AtLeastOnce, |args, _| Ok(json!({ "sent": args["text"] }))));
        let o = exec(&s, "use:\n  functions:\n    notify: { call: send, with: { text: '${ \"hi \" + .name }' } }\ndo:\n  - greet: { call: notify, with: { name: '${ .user }' } }\n", json!({"user": "ann"})).await;
        assert_eq!(completed(&o), &json!({"sent": "hi ann"}));
        // The journal names the host call, under the function's reference.
        let journal: Vec<_> = o.calls.iter().map(|c| (c.key.as_str(), c.function.as_str(), c.state)).collect();
        assert_eq!(journal, [("/do/0/greet/use/functions/notify#call", "send", durable::CallStatus::Completed)]);
        assert_eq!(s.fake.calls.lock().unwrap()[0].2, "/do/0/greet/use/functions/notify");
    }

    const RETRYING: &str = r#"
do:
  - fetch:
      try: [ { get: { call: flaky } } ]
      catch:
        retry: { delay: { seconds: 1 }, backoff: { exponential: {} }, limit: { attempt: { count: 3 } } }
"#;

    #[tokio::test(start_paused = true)]
    async fn retries_back_off_and_stop_at_the_attempt_limit() {
        let s = setup(Fake::default().with("flaky", Delivery::AtLeastOnce, |_, n| {
            if n < 2 { Err(WorkflowError::communication(503, "busy")) } else { Ok(json!("ok")) }
        }));
        assert_eq!(completed(&exec(&s, RETRYING, Value::Null).await), &json!("ok"));
        assert_eq!(s.log.lines("retry "), ["/do/0/fetch #1 after 1s", "/do/0/fetch #2 after 2s"]);

        // Three attempts fail: the error is handled (no catch.do), the input passes through.
        let s = setup(
            Fake::default().with("flaky", Delivery::AtLeastOnce, |_, _| Err(WorkflowError::communication(503, "busy"))),
        );
        assert_eq!(completed(&exec(&s, RETRYING, json!("in")).await), &json!("in"));
        assert_eq!(s.fake.calls.lock().unwrap().len(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn an_attempt_that_started_an_at_most_once_call_is_not_retried() {
        // Without a journal there is no saved result to stand for an earlier
        // attempt, so a protected call's attempt never runs again.
        let s = setup(
            Fake::default().with("flaky", Delivery::AtMostOnce, |_, _| Err(WorkflowError::communication(503, "busy"))),
        );
        exec(&s, RETRYING, Value::Null).await;
        assert_eq!(s.fake.calls.lock().unwrap().len(), 1);
        assert!(s.log.lines("retry ").is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn task_timeouts_are_catchable_errors() {
        let s = setup(Fake::default());
        let o = exec(&s, "do:\n  - slow: { wait: PT1M, timeout: { after: PT1S } }\n", Value::Null).await;
        let e = faulted(&o);
        assert!(e.is("timeout") && e.instance.as_deref() == Some("/do/0/slow"), "{e:?}");
        let o = exec(&s, "do:\n  - t: { try: [ { slow: { wait: PT1M, timeout: { after: PT1S } } } ], catch: { errors: { with: { status: 408 } }, do: [ { h: { set: { r: handled } } } ] } }\n", Value::Null).await;
        assert_eq!(completed(&o), &json!({"r": "handled"}));
    }

    #[tokio::test]
    async fn a_cycle_hits_the_task_limit_which_try_cannot_catch() {
        let mut s = setup(Fake::default());
        s.host.limits.max_tasks = 50;
        // Validation rejects this exit-less cycle; the ceiling still holds for
        // a host that runs it anyway (the jump budget is above the ceiling).
        let wf = model::parse_yaml(&format!("{HEAD}do:\n  - t:\n      try:\n        - a: {{ set: {{ x: 1 }}, then: b }}\n        - b: {{ set: {{ x: 2 }}, then: a }}\n      catch: {{ do: [ {{ h: {{ set: {{ r: caught }} }} }} ] }}\n")).unwrap();
        assert!(validate(&wf, &*s.host.functions).is_err());
        let o = run(&s.host, &wf, "run-1", Value::Null, &CancellationToken::new()).await;
        assert!(faulted(&o).detail.as_deref().is_some_and(|d| d.contains("more than 50 tasks")));
        assert_eq!(o.tasks_run, 51);
    }

    // A loop that only the budget stops: its exit never matches.
    const LOOP: &str = r#"
do:
  - step: { set: { n: '${ .n + 1 }' } }
  - again:
      switch:
        - done: { when: '${ .n < 0 }', then: exit }
        - more: { then: step }
"#;

    fn limited(on_exceeded: &str, max: u64) -> String {
        format!(
            "document: {{ dsl: '1.0.4', namespace: test, name: w, version: '0.1.0', metadata: {{ limits: {{ max_jumps: {max}{on_exceeded} }} }} }}\n{LOOP}"
        )
    }

    #[tokio::test]
    async fn the_budget_raises_budget_exhausted_at_the_jumping_task_which_try_catches() {
        let s = setup(Fake::default());
        let o = exec(&s, &format!("{HEAD}{LOOP}"), json!({"n": 0})).await;
        let e = faulted(&o);
        assert!(e.is("budget.exhausted") && e.status == 508, "{e:?}");
        assert_eq!(e.instance.as_deref(), Some("/do/1/again[100]"));
        assert!(o.limit_exceeded);
        // The host's ceiling is 100; a definition tightens it.
        let o = exec(&s, &limited("", 3), json!({"n": 0})).await;
        assert_eq!(faulted(&o).instance.as_deref(), Some("/do/1/again[3]"));
        let caught = "do:\n  - guard:\n      try:\n        - step: { set: { n: '${ .n + 1 }' } }\n        - again: { switch: [ { done: { when: '${ .n < 0 }', then: exit } }, { more: { then: step } } ] }\n      catch: { as: e, do: [ { h: { set: { stopped: '${ .n }', type: '${ $e.type }' } } } ] }\n";
        let o = exec(&s, &format!("{HEAD}{caught}"), json!({"n": 0})).await;
        let out = completed(&o);
        assert_eq!(out["stopped"], 0, "the catch sees the try's input");
        assert!(out["type"].as_str().is_some_and(|t| t.ends_with("/budget.exhausted")));
        assert!(!o.limit_exceeded, "a caught exhaustion did not end the run");
    }

    #[tokio::test]
    async fn on_exceeded_end_or_a_handler_ends_the_run() {
        let s = setup(
            Fake::default().with("report", Delivery::AtLeastOnce, |args, _| Ok(json!({ "reported": args["n"] }))),
        );
        let o = exec(&s, &limited(", on_exceeded: end", 2), json!({"n": 0})).await;
        assert_eq!(completed(&o), &json!({"n": 3}));
        assert!(o.limit_exceeded);
        let o = exec(&s, &limited(", on_exceeded: { call: report, with: { n: '${ .n }' } }", 2), json!({"n": 0})).await;
        assert_eq!(completed(&o), &json!({"reported": 3}));
        assert!(o.limit_exceeded);
        assert_eq!(s.fake.calls.lock().unwrap()[0].2, "/do/1/again[2]/limits/on_exceeded");
        // The handler cannot jump, nor run more than a few tasks.
        let jumping = ", on_exceeded: { do: [ { a: { set: { x: 1 } } }, { b: { switch: [ { stop: { when: '${ false }', then: exit } }, { back: { then: a } } ] } } ] }";
        let o = exec(&s, &limited(jumping, 1), json!({"n": 0})).await;
        let e = faulted(&o);
        assert!(e.is("budget.exhausted") && e.detail.as_deref().is_some_and(|d| d.contains("cannot jump")), "{e:?}");
        let long: Vec<String> = (0..HANDLER_TASKS).map(|i| format!("{{ t{i}: {{ set: {{ x: {i} }} }} }}")).collect();
        let o =
            exec(&s, &limited(&format!(", on_exceeded: {{ do: [ {} ] }}", long.join(", ")), 1), json!({"n": 0})).await;
        let e = faulted(&o);
        assert!(e.detail.as_deref().is_some_and(|d| d.contains("at most 10 tasks")), "{e:?}");
        assert!(o.limit_exceeded);
    }

    #[tokio::test]
    async fn expressions_read_jumps_and_visits() {
        let s = setup(Fake::default());
        let o = exec(
            &s,
            r#"
do:
  - compose: { set: { n: '${ .n + 1 }', seen: '${ $run.visits.compose }' } }
  - review:
      switch:
        - rewrite: { when: '${ $run.visits.compose < 3 }', then: compose }
  - report: { set: { n: '${ .n }', jumps: '${ $run.jumps }', max: '${ $run.maxJumps }', inner: '${ $run.visits }' } }
"#,
            json!({"n": 0}),
        )
        .await;
        assert_eq!(
            completed(&o),
            &json!({"n": 3, "jumps": 2, "max": 100, "inner": {"compose": 3, "review": 3, "report": 1}})
        );
    }

    #[test]
    fn a_definition_may_only_tighten_the_hosts_ceiling() {
        let wf = model::parse_yaml(&limited("", 100)).unwrap();
        assert!(validate_limits(&wf, &Limits::default()).is_ok());
        let wf = model::parse_yaml(&limited("", 101)).unwrap();
        let e = validate_limits(&wf, &Limits::default()).unwrap_err().to_string();
        assert!(e.contains("/document/metadata/limits/max_jumps") && e.contains("ceiling of 100"), "{e}");
    }

    #[tokio::test]
    async fn cancellation_stops_a_run_and_reports_its_calls() {
        let s = setup(Fake::default().with("send", Delivery::AtLeastOnce, |_, _| Ok(Value::Null)));
        let wf = model::parse_yaml(&format!(
            "{HEAD}do:\n  - a: {{ call: send }}\n  - b: {{ wait: PT1H }}\n  - c: {{ call: send }}\n"
        ))
        .unwrap();
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            stop.cancel();
        });
        let o = run(&s.host, &wf, "run-1", Value::Null, &cancel).await;
        assert!(matches!(o.status, Status::Cancelled), "{o:?}");
        let journal: Vec<_> = o.calls.iter().map(|c| (c.key.as_str(), c.function.as_str(), c.state)).collect();
        assert_eq!(journal, [("/do/0/a#call", "send", durable::CallStatus::Completed)]);
        assert_eq!(s.fake.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn runtime_arguments_describe_the_task_and_the_workflow() {
        let s = setup(Fake::default());
        let o = exec(&s, "do:\n  - probe: { set: { task: '${ $task.reference }', name: '${ $task.name }', run: '${ $workflow.id }', at: '${ $task.startedAt.epoch.seconds }', rt: '${ $runtime.name }', raw: '${ $workflow.input }' } }\n", json!(7)).await;
        assert_eq!(
            completed(&o),
            &json!({"task": "/do/0/probe", "name": "probe", "run": "run-1", "at": 1_700_000_000, "rt": "owf-test", "raw": 7})
        );
    }
}
