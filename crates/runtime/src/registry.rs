//! User functions: schemas, declared delivery and explicit injected async
//! handlers.
//!
//! The registry does not say whether a function reads or writes: only the
//! host can judge that, and it decides from the call journal (what a run
//! called, how far each call got) and from its own dry-run mock rule.
use crate::capabilities::{Capabilities, HostCatalog};
use async_trait::async_trait;
use opentelemetry::Context as SpanContext;
pub use owf::durable::Delivery;
use owf::{
    model::{self, Catalog, Workflow},
    run::WorkflowError,
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, future::Future, sync::Arc};
use tokio_util::sync::CancellationToken;
pub type FunctionError = WorkflowError;

#[derive(Clone)]
pub struct FunctionContext {
    pub cancel: CancellationToken,
    pub run_id: String,
    pub task_path: String,
    pub attempt: u32,
    pub idempotency_key: String,
    /// The current function span; use as the parent of client/SDK spans.
    pub span: SpanContext,
    /// The task's transformed input, against which a protocol function
    /// resolves URI templates (`https://host/pet/{petId}`).
    pub input: Value,
}
#[async_trait]
pub trait Handler: Send + Sync {
    async fn call(&self, arguments: Value, context: FunctionContext) -> Result<Value, FunctionError>;
}
#[async_trait]
impl<F, Fut> Handler for F
where
    F: Fn(Value, FunctionContext) -> Fut + Send + Sync,
    Fut: Future<Output = Result<Value, FunctionError>> + Send,
{
    async fn call(&self, a: Value, c: FunctionContext) -> Result<Value, FunctionError> {
        self(a, c).await
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
    /// tighten it (`metadata.delivery: at_most_once`), never loosen it.
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
