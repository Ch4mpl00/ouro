//! User functions: 1. functions (schemas, declared delivery and explicit
//! injected async handlers) and the registry; 2. the registry a runtime has
//! installed, by generation; 3. the start check: every function a definition
//! can call is registered, cached per definition version and generation.
//!
//! The registry does not say whether a function reads or writes: only the
//! host can judge that, and it decides from the call journal (what a run
//! called, how far each call got) and from its own dry-run mock rule.
use crate::capabilities::{Capabilities, HostCatalog};
use async_trait::async_trait;
use opentelemetry::{Context as SpanContext, KeyValue, metrics::Meter};
pub use ows::durable::Delivery;
use ows::durable::{Annotation, Annotations};
use ows::{
    model::{self, Catalog, Process, Workflow},
    run::WorkflowError,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    future::Future,
    hash::{Hash, Hasher},
    sync::{Arc, Mutex, RwLock},
};
use tokio_util::sync::CancellationToken;

// ── 1. functions and the registry ──────────────────────────────────────────

/// A handler's error. Its type says how far the call got
/// (`ows::durable::CallFailure::answer`): `FunctionError::not_executed`
/// (refused before doing anything: saved, caught like any error, safe to
/// send again in a new attempt), `unknown_outcome` (it may have had its
/// effect: an at-most-once call is never sent again and only a `catch`
/// naming `CALL_UNKNOWN` handles it) and `interrupted` (stopped, no answer:
/// the run stays recoverable). Any other error is a definite failure.
pub type FunctionError = WorkflowError;

#[derive(Clone)]
pub struct FunctionContext {
    /// Fires when the run is cancelled (in this process at once, from
    /// another one at its next lease renewal), when the worker stops, and
    /// when a renewal finds the lease lost or an emergency stop set. The
    /// runtime then drops the handler's future
    /// without waiting for it, so a handler that starts work outside its
    /// future (a remote job, a spawned task) watches the token to stop it.
    /// Nothing the handler answers afterwards is recorded: the call keeps its
    /// intent, unknown if at most once (never sent again), pending otherwise
    /// (sent again on recovery unless the run was cancelled).
    pub cancel: CancellationToken,
    pub run_id: String,
    pub task_path: String,
    pub attempt: u32,
    pub idempotency_key: String,
    /// The call's span, current while the handler runs: add attributes to
    /// it (`gen_ai.request.model`) and use it as the parent of client/SDK
    /// spans (an HTTP request to a model, a database query).
    pub span: SpanContext,
    /// The task's transformed input, against which a protocol function
    /// resolves URI templates (`https://host/pet/{petId}`).
    pub input: Value,
    /// The function called (`run:<kind>` for a process).
    pub function: String,
    /// The run's definition and version.
    pub definition: String,
    pub version: u64,
    /// The call's effective delivery.
    pub delivery: Delivery,
    /// The runtime's meter (`Runtime::with_meter`), for a function's own
    /// metrics; label them with `attributes`, never with the run id.
    pub meter: Meter,
    /// The run's host data (`FunctionContext::host`).
    pub host: Option<Arc<Value>>,
    /// Starts owned children the host vouches for (`FunctionContext::start_child`):
    /// present for a registered function's handler, in live runs and dry runs;
    /// `None` for a process runner and for a dry run's `Mock::response`.
    pub children: Option<Arc<dyn ChildStarter>>,
}
impl FunctionContext {
    /// The call's bounded metric attributes: `ouro.definition`,
    /// `ouro.function` and `ouro.delivery`. Run ids and task paths belong
    /// on spans only: they would make a series per run.
    pub fn attributes(&self) -> Vec<KeyValue> {
        crate::telemetry::call_attributes(&self.definition, &self.function, self.delivery)
    }
    /// The host data the run was admitted with (`Runtime::start_with`, or
    /// the host function that started it as a child): what the host vouches
    /// for, which no workflow can set. `None`: the run has none, as every run
    /// a workflow started itself (`run: workflow`, the built-in `start_child`).
    pub fn host(&self) -> Option<&Value> {
        self.host.as_deref()
    }
    /// Starts an owned child of the run this call serves, which the host
    /// vouches for: with its definition (the active version, or `version`),
    /// input and host data. It is committed with the parent's subscriptions
    /// to its end, through the run's lease, under the same ownership,
    /// cascade, budget, nesting depth and trip/pause rules as the built-in
    /// `start_child`. Its id is derived from the run, this call's idempotency
    /// key and `key`, so a re-dispatch of the call joins the same child. The
    /// first commit wins: a join returns the child as it was admitted, whatever
    /// input, version or host data the re-dispatch computed (only another
    /// definition is refused, `ChildRefusal::Mismatch`). The
    /// workflow joins it with `listen` (`ChildRef::done`) or reads its outcome
    /// with the built-in `child_outcome` (`ChildRef::id`, `ChildRef::key`).
    ///
    /// Register a function that starts children at least once (the default):
    /// an at-most-once call whose outcome is unknown is never sent again, so
    /// it could not join the child it may have started.
    ///
    /// A refusal (`ChildError::Refused`) means nothing happened: answer it
    /// as not executed. `ChildError` converts into exactly that, or into an
    /// interruption for a lost lease or a storage failure, so `?` does.
    pub async fn start_child(&self, child: ChildStart) -> Result<ChildRef, ChildError> {
        match &self.children {
            Some(children) => children.start_child(child).await,
            None => Err(ChildError::Refused(ChildRefusal::Unavailable)),
        }
    }
}

/// An owned child a host function starts (`FunctionContext::start_child`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChildStart {
    /// Distinguishes the children one call starts; part of the child's id.
    pub key: String,
    /// The child's definition.
    pub name: String,
    /// A stored version to run instead of the active one.
    pub version: Option<u64>,
    pub input: Value,
    /// The child's host data (`null`: none), at most `durable::MAX_HOST_BYTES`.
    pub host: Value,
}

/// A started (or joined) host child: its run id, the link key the built-in
/// `child_outcome` takes with the id, and the event types of its end and of
/// its waiting, as the built-in `start_child` returns them.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChildRef {
    pub id: String,
    pub key: String,
    pub done: String,
    pub waiting: String,
}

/// The capability behind `FunctionContext::start_child`, which the runtime
/// supplies for each call.
#[async_trait]
pub trait ChildStarter: Send + Sync {
    async fn start_child(&self, child: ChildStart) -> Result<ChildRef, ChildError>;
}

/// Why `FunctionContext::start_child` started no child.
#[derive(Debug)]
pub enum ChildError {
    /// Refused before anything happened: no child was admitted.
    Refused(ChildRefusal),
    /// Stopped without an answer: the run's lease is lost, it is held or
    /// stopped, or storage failed. A commit may have happened; a re-dispatch
    /// of the call joins the child if it did.
    Interrupted(anyhow::Error),
}

/// What refused a host child (`ChildError::Refused`).
#[derive(Debug)]
pub enum ChildRefusal {
    /// The call has no child capability (a process runner, a mocked call).
    Unavailable,
    /// Its host data is larger than `durable::MAX_HOST_BYTES`.
    HostData(HostDataTooLarge),
    /// It would nest deeper than the host's `Limits::max_depth`.
    Depth(WorkflowError),
    /// Its definition is tripped by its breaker or paused by an operator.
    Tripped(ouro_durable::DefinitionTripped),
    /// The id is taken by a child of another definition.
    Mismatch(ouro_durable::AdmissionMismatch),
    /// The definition cannot be admitted: no active or no such version, a
    /// function it calls is not registered (`MissingFunction`), or it does not
    /// validate against the host (`InvalidDefinition`).
    Inadmissible(anyhow::Error),
}

impl std::fmt::Display for ChildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChildError::Refused(ChildRefusal::Unavailable) => {
                f.write_str("this call cannot start child runs (a process run or a mocked call)")
            }
            ChildError::Refused(ChildRefusal::HostData(e)) => e.fmt(f),
            ChildError::Refused(ChildRefusal::Depth(e)) => e.fmt(f),
            ChildError::Refused(ChildRefusal::Tripped(e)) => e.fmt(f),
            ChildError::Refused(ChildRefusal::Mismatch(e)) => e.fmt(f),
            ChildError::Refused(ChildRefusal::Inadmissible(e)) | ChildError::Interrupted(e) => write!(f, "{e:#}"),
        }
    }
}
impl std::error::Error for ChildError {}

/// A refusal is a proof that the call did nothing (`not_executed`); an
/// interruption is no answer (`interrupted`).
impl From<ChildError> for FunctionError {
    fn from(error: ChildError) -> Self {
        match &error {
            ChildError::Refused(_) => FunctionError::not_executed(error.to_string()),
            ChildError::Interrupted(_) => FunctionError::interrupted(error.to_string()),
        }
    }
}

/// Host data larger than `durable::MAX_HOST_BYTES` (`Runtime::start_with`,
/// `FunctionContext::start_child`): refused before anything is stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostDataTooLarge {
    pub bytes: usize,
}
impl std::fmt::Display for HostDataTooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "host data of {} bytes exceeds {} bytes", self.bytes, ouro_durable::MAX_HOST_BYTES)
    }
}
impl std::error::Error for HostDataTooLarge {}

/// What a function returns: its result and the annotations it carries
/// beside it (`ows::durable::Annotation`: numbers and short strings), saved
/// with the call's record, returned on recovery and replay, copied to the
/// call's span attributes and, for numbers, summed into the
/// `ouro.call.annotations` metric. At most `ows::durable::MAX_ANNOTATIONS`
/// of them and `MAX_ANNOTATION_BYTES` as JSON: beyond that, the call is
/// answered with a validation error. A handler may return a bare `Value`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Output {
    pub value: Value,
    pub annotations: Annotations,
}
impl Output {
    pub fn new(value: Value) -> Self {
        Self { value, annotations: Annotations::new() }
    }
    /// Adds an annotation; a later one of the same name replaces it.
    pub fn annotate(mut self, name: impl Into<String>, value: impl Into<Annotation>) -> Self {
        self.annotations.insert(name.into(), value.into());
        self
    }
}
impl From<Value> for Output {
    fn from(value: Value) -> Self {
        Self::new(value)
    }
}

#[async_trait]
pub trait Handler: Send + Sync {
    async fn call(&self, arguments: Value, context: FunctionContext) -> Result<Output, FunctionError>;
}
#[async_trait]
impl<F, Fut, O> Handler for F
where
    F: Fn(Value, FunctionContext) -> Fut + Send + Sync,
    Fut: Future<Output = Result<O, FunctionError>> + Send,
    O: Into<Output>,
{
    async fn call(&self, a: Value, c: FunctionContext) -> Result<Output, FunctionError> {
        self(a, c).await.map(Into::into)
    }
}
pub struct Function {
    pub name: String,
    pub input_schema: Value,
    pub output_schema: Value,
    /// At least once unless declared otherwise: a call whose outcome is
    /// unknown after a crash may be dispatched again under the same
    /// idempotency key. A function that must not be repeated (sending a
    /// message, charging a card) declares `AtMostOnce`; a call in YAML may
    /// tighten it (`metadata.ouro.delivery: at_most_once`), never loosen it.
    pub delivery: Delivery,
    pub handler: Arc<dyn Handler>,
}
impl Function {
    pub fn new(name: impl Into<String>, input: Value, output: Value, handler: impl Handler + 'static) -> Self {
        Self {
            name: name.into(),
            input_schema: input,
            output_schema: output,
            delivery: Delivery::AtLeastOnce,
            handler: Arc::new(handler),
        }
    }
    /// Declares the function's delivery (`Function::new` is at least once).
    pub fn with_delivery(mut self, delivery: Delivery) -> Self {
        self.delivery = delivery;
        self
    }
}
pub(crate) struct Registered {
    pub function: Function,
    pub input: jsonschema::Validator,
    pub output: jsonschema::Validator,
}
#[derive(Default)]
pub struct Registry {
    entries: BTreeMap<String, Registered>,
}
impl Registry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn register(&mut self, f: Function) -> anyhow::Result<()> {
        anyhow::ensure!(!f.name.is_empty() && !builtin(&f.name), "empty or reserved function name");
        anyhow::ensure!(!self.entries.contains_key(&f.name), "function {} already registered", f.name);
        let input = jsonschema::validator_for(&f.input_schema).map_err(|e| anyhow::anyhow!(e.to_string()))?;
        let output = jsonschema::validator_for(&f.output_schema).map_err(|e| anyhow::anyhow!(e.to_string()))?;
        self.entries.insert(f.name.clone(), Registered { function: f, input, output });
        Ok(())
    }
    pub(crate) fn get(&self, name: &str) -> Option<&Registered> {
        self.entries.get(name)
    }
    /// Parses and validates a definition against these functions and a
    /// runtime without optional capabilities (`Runtime::validate` knows its own).
    pub fn validate(&self, yaml: &str) -> anyhow::Result<Workflow> {
        self.validate_with(yaml, &Capabilities::default())
    }
    pub(crate) fn validate_with(&self, yaml: &str, capabilities: &Capabilities) -> anyhow::Result<Workflow> {
        let wf = model::parse_yaml(yaml)?;
        model::validate(&wf, &HostCatalog { registry: self, capabilities })?;
        for site in model::call_sites(&wf) {
            let Some(f) = self.get(&site.function) else {
                continue;
            };
            let args = site.with.unwrap_or_else(|| json!({}));
            if !has_expression(&args) {
                f.input
                    .validate(&args)
                    .map_err(|e| anyhow::anyhow!("{}: {} input: {e}", site.reference, site.function))?;
            } else if let Some(args) = args.as_object() {
                if let Some(required) = f.function.input_schema["required"].as_array() {
                    for name in required.iter().filter_map(Value::as_str) {
                        anyhow::ensure!(
                            args.contains_key(name),
                            "{}: {} needs argument {name}",
                            site.reference,
                            site.function
                        );
                    }
                }
                if f.function.input_schema["additionalProperties"] == false
                    && let Some(properties) = f.function.input_schema["properties"].as_object()
                {
                    for name in args.keys() {
                        anyhow::ensure!(
                            properties.contains_key(name),
                            "{}: {} takes no argument {name}",
                            site.reference,
                            site.function
                        );
                    }
                }
            }
        }
        Ok(wf)
    }
    /// Persist schemas and declared delivery for the functions this
    /// definition binds. Changes to those bindings fail recovery explicitly,
    /// before executing a handler.
    pub(crate) fn bindings(&self, wf: &Workflow) -> Value {
        let mut bindings = BTreeMap::new();
        for site in model::call_sites(wf) {
            if let Some(f) = self.get(&site.function) {
                bindings.insert(
                    site.function,
                    json!({
                        "input": f.function.input_schema,
                        "output": f.function.output_schema,
                        "delivery": f.function.delivery,
                    }),
                );
            }
        }
        json!(bindings)
    }
}
impl Catalog for Registry {
    fn has(&self, name: &str) -> bool {
        self.entries.contains_key(name) || builtin(name)
    }
    fn events(&self) -> bool {
        true
    }
    fn emits(&self) -> bool {
        true
    }
    fn children(&self) -> bool {
        true
    }
}
/// The reserved functions: `start_child` starts an owned child that the
/// parent joins later with `listen` (its `done` event type), and
/// `child_outcome` reads an owned child's stored outcome. `run: workflow`
/// is the DSL's way to run a child; these remain for a parent that starts
/// a child, keeps working and joins it afterwards.
pub(crate) fn builtin(name: &str) -> bool {
    matches!(name, "start_child" | "child_outcome")
}
fn has_expression(value: &Value) -> bool {
    match value {
        Value::String(s) => s.contains("${"),
        Value::Array(a) => a.iter().any(has_expression),
        Value::Object(o) => o.values().any(has_expression),
        _ => false,
    }
}

// ── 2. the installed registry ──────────────────────────────────────────────

/// The registry a runtime calls through. Replacing it (a host whose function
/// set changes while it runs) starts a new generation; a start check is valid
/// for the generation it was made against only.
pub(crate) struct Installed {
    current: RwLock<(Arc<Registry>, u64)>,
    pub checks: Arc<Checks>,
}

impl Installed {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self { current: RwLock::new((registry, 0)), checks: Arc::default() }
    }
    /// The registry and its generation, together.
    pub fn get(&self) -> (Arc<Registry>, u64) {
        self.current.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
    pub fn replace(&self, registry: Arc<Registry>) {
        let mut current = self.current.write().unwrap_or_else(|e| e.into_inner());
        *current = (registry, current.1 + 1);
        // Checks of older generations can never be read again. One made
        // concurrently against the old registry is stored under its own
        // generation, so it cannot stand for the new one either.
        self.checks.0.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

// ── 3. the start check ─────────────────────────────────────────────────────

/// A function a run would call that the registry does not hold, found before
/// the run's first step (at admission) or its next one (when it resumes).
/// `path` names the definitions from the checked one down to the one that
/// calls it, through the `run: workflow` children each names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissingFunction {
    pub function: String,
    /// `(definition name, version)`, outermost first.
    pub path: Vec<(String, u64)>,
}

impl MissingFunction {
    /// The configuration error a run that cannot resume faults with.
    pub fn error(&self) -> WorkflowError {
        WorkflowError::standard("configuration", 400, "Configuration Error", self.to_string())
    }
}

impl std::fmt::Display for MissingFunction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "function `{}` is not registered", self.function)?;
        let mut path = self.path.iter().map(|(name, version)| format!("{name} version {version}"));
        if let Some(first) = path.next() {
            write!(f, "; called by {}", path.fold(first, |a, b| format!("{a} > {b}")))?;
        }
        Ok(())
    }
}

impl std::error::Error for MissingFunction {}

/// What one definition version can call, as one registry generation sees it.
pub(crate) struct Needs {
    /// The first function it calls (definition order) that is not
    /// registered: a host function, or a process runner (`run:<kind>`).
    pub missing: Option<String>,
    /// The child workflows its `run: workflow` tasks name: namespace, name
    /// and version, resolved against the catalog when a check walks them.
    pub children: Vec<(String, String, String)>,
}

impl Needs {
    fn of(yaml: &str, registry: &Registry, capabilities: &Capabilities) -> Self {
        // A definition that does not parse calls nothing: validation, which
        // follows the check, reports why.
        let Ok(wf) = model::parse_yaml(yaml) else { return Self { missing: None, children: Vec::new() } };
        let local = |name: &str| wf.uses.functions.iter().any(|(n, _)| n == name);
        let mut missing = model::call_sites(&wf)
            .into_iter()
            .map(|site| site.function)
            .find(|name| !local(name) && !registry.has(name));
        let mut children = Vec::new();
        for process in model::processes(&wf) {
            match process {
                Process::Workflow { namespace, name, version, .. } => children.push((namespace, name, version)),
                Process::External { kind, .. } => {
                    if missing.is_none() && !capabilities.processes.as_ref().is_some_and(|p| p.runs(&kind)) {
                        missing = Some(Process::function(&kind));
                    }
                }
            }
        }
        Self { missing, children }
    }
}

/// Start checks by definition version and registry generation. A definition
/// version is immutable, so its direct needs are computed once per
/// generation; the children it names are resolved on every check, because
/// activation may change which version `latest` is.
#[derive(Default)]
pub(crate) struct Checks(Mutex<HashMap<CheckKey, Arc<Needs>>>);

/// Definition name, version, digest of its text, registry generation.
type CheckKey = (String, u64, u64, u64);

impl Checks {
    /// `name`/`version` identify the definition in its catalog; the digest of
    /// its text keeps definitions of two catalogs apart (a dry run's own and
    /// the host's children).
    pub fn needs(
        &self,
        (name, version, yaml): (&str, u64, &str),
        (registry, generation): (&Registry, u64),
        capabilities: &Capabilities,
    ) -> Arc<Needs> {
        let mut digest = std::collections::hash_map::DefaultHasher::new();
        yaml.hash(&mut digest);
        let key = (name.to_owned(), version, digest.finish(), generation);
        if let Some(needs) = self.0.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
            return needs.clone();
        }
        // Computed outside the lock; a concurrent check may compute it too.
        let needs = Arc::new(Needs::of(yaml, registry, capabilities));
        self.0.lock().unwrap_or_else(|e| e.into_inner()).entry(key).or_insert(needs).clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WF: &str = "document: { dsl: '1.0.4', namespace: t, name: w, version: '0.1.0' }\ndo:\n  - a: { call: a }\n  - b: { call: b }\n  - kid: { run: { workflow: { namespace: t, name: kid, version: latest } } }\n";

    fn registry(names: &[&str]) -> Arc<Registry> {
        let mut r = Registry::new();
        for name in names {
            r.register(Function::new(*name, json!({}), json!({}), |_: Value, _: FunctionContext| async {
                Ok(Value::Null)
            }))
            .unwrap();
        }
        Arc::new(r)
    }

    #[test]
    fn a_check_is_cached_per_definition_version_and_registry_generation() {
        let installed = Installed::new(registry(&["a"]));
        let none = Capabilities::default();
        let check = |installed: &Installed| {
            let (r, generation) = installed.get();
            installed.checks.needs(("w", 1, WF), (&r, generation), &none)
        };
        let first = check(&installed);
        assert_eq!(first.missing.as_deref(), Some("b"));
        assert_eq!(first.children, [("t".to_owned(), "kid".to_owned(), "latest".to_owned())]);
        // The same version against the same generation: the cached check.
        assert!(Arc::ptr_eq(&first, &check(&installed)));
        // A new registry is a new generation: the check is made again.
        installed.replace(registry(&["a", "b"]));
        let second = check(&installed);
        assert!(!Arc::ptr_eq(&first, &second));
        assert_eq!(second.missing, None);
        assert!(Arc::ptr_eq(&second, &check(&installed)));
        // Another text under the same name and version is another entry.
        let other = WF.replace("call: b", "call: c");
        let (r, generation) = installed.get();
        assert_eq!(installed.checks.needs(("w", 1, &other), (&r, generation), &none).missing.as_deref(), Some("c"));
    }

    #[test]
    fn a_missing_function_names_itself_and_who_calls_it() {
        let m = MissingFunction { function: "c".into(), path: vec![("parent".into(), 2), ("kid".into(), 1)] };
        assert_eq!(m.to_string(), "function `c` is not registered; called by parent version 2 > kid version 1");
        assert!(m.error().is("configuration"));
    }
}
