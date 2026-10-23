//! Host capabilities beyond functions: 1. the seams a host may provide
//! (process runner, secret resolver, resource resolver, event sink); 2. the
//! catalog validation sees, so a definition needing a capability the host
//! lacks is rejected when it is saved, never when it runs.
//!
//! None has a built-in implementation: protocol clients, processes,
//! credentials and remote resources belong to the embedding application.
use crate::{FunctionContext, FunctionError, Registry};
use async_trait::async_trait;
use owf::{model::Catalog, run::Effect};
use serde_json::Value;
use std::sync::Arc;

// ── 1. seams ───────────────────────────────────────────────────────────────

/// Runs `run: container | script | shell` processes.
#[async_trait]
pub trait ProcessRunner: Send + Sync {
    /// The effect of running a `kind` process, or `None` when this runner
    /// does not run that kind. A write is delivered at most once, like a
    /// write function.
    fn effect(&self, kind: &str) -> Option<Effect>;
    /// `definition` is the rendered `run.<kind>` object with `await` added.
    /// Returns a process result `{ code, stdout, stderr }`; `return` picks
    /// from it. A process that is not awaited may return as soon as it starts.
    async fn run(&self, kind: &str, definition: Value, context: FunctionContext) -> Result<Value, FunctionError>;
}

/// Resolves `use.secrets` (see `owf::secrets`): values stay in memory and
/// are redacted from records, spans and errors.
pub use owf::secrets::Secrets as SecretResolver;

/// Resolves external resources: the JSON Schemas `schema.resource` names.
/// Called when a definition is saved and when a run is admitted; the run
/// keeps the documents it was admitted with.
pub trait ResourceResolver: Send + Sync {
    fn resolve(&self, uri: &str) -> anyhow::Result<Value>;
}

/// Receives every event a run emits, after the runtime has delivered it to
/// the runs listening for it. Called again with the same envelope when
/// recovery cannot tell whether a previous attempt got through:
/// deduplicate by `source` + `id`.
#[async_trait]
pub trait EventSink: Send + Sync {
    async fn emit(&self, event: &Value) -> Result<(), FunctionError>;
}

/// The capabilities a runtime was built with (`Runtime::with_*`).
#[derive(Clone, Default)]
pub struct Capabilities {
    pub processes: Option<Arc<dyn ProcessRunner>>,
    pub secrets: Option<Arc<dyn SecretResolver>>,
    pub resources: Option<Arc<dyn ResourceResolver>>,
    pub sink: Option<Arc<dyn EventSink>>,
}

// ── 2. validation ──────────────────────────────────────────────────────────

/// The registry's functions plus the runtime's capabilities. Events, emits
/// and child workflows are always available: the runtime implements them.
pub(crate) struct HostCatalog<'a> {
    pub registry: &'a Registry,
    pub capabilities: &'a Capabilities,
}

impl Catalog for HostCatalog<'_> {
    fn has(&self, name: &str) -> bool {
        self.registry.has(name)
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
    fn processes(&self, kind: &str) -> bool {
        self.capabilities.processes.as_ref().is_some_and(|p| p.effect(kind).is_some())
    }
    fn secrets(&self) -> bool {
        self.capabilities.secrets.is_some()
    }
    fn resources(&self) -> bool {
        self.capabilities.resources.is_some()
    }
}
