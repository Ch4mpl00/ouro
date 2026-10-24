//! Operator access using only a store and a clock, in a console or separate
//! process. The embedding host supplies authorization, request IDs and actors.
pub use ouro_durable::control::*;
use ouro_durable::{Clock, RunStore, SavedRun};
use std::sync::Arc;

pub struct Control {
    store: Arc<dyn RunStore>,
    clock: Arc<dyn Clock>,
}
impl Control {
    pub fn new(store: Arc<dyn RunStore>, clock: Arc<dyn Clock>) -> Self {
        Self { store, clock }
    }
    pub async fn execute(&self, request: &Request, command: &Command) -> anyhow::Result<Reply> {
        self.store.control(request, command, self.clock.now_ms()).await
    }
    pub async fn cancel(&self, request: &Request, run: &str) -> anyhow::Result<Reply> {
        self.execute(request, &Command::Cancel { run: run.into() }).await
    }
    pub async fn cancel_tree(&self, request: &Request, run: &str) -> anyhow::Result<Reply> {
        self.cancel(request, run).await
    }
    pub async fn pause(&self, request: &Request, definition: &str, note: &str) -> anyhow::Result<Reply> {
        self.execute(request, &Command::Pause { definition: definition.into(), note: note.into() }).await
    }
    pub async fn resume(&self, request: &Request, definition: &str) -> anyhow::Result<Reply> {
        self.execute(request, &Command::Resume { definition: definition.into() }).await
    }
    pub async fn emergency_stop(&self, request: &Request, enabled: bool) -> anyhow::Result<Reply> {
        self.execute(request, &Command::Stop { enabled }).await
    }
    pub async fn stopped(&self) -> anyhow::Result<bool> {
        self.store.stopped().await
    }
    pub async fn respond(
        &self,
        request: &Request,
        run: &str,
        wait: &str,
        event: serde_json::Value,
    ) -> anyhow::Result<Reply> {
        self.execute(request, &Command::Respond { run: run.into(), wait: wait.into(), event }).await
    }
    pub async fn inspect(&self, run: &str) -> anyhow::Result<Option<Inspection>> {
        self.store.inspect_control(run).await
    }
    pub async fn list(&self, filter: &Runs, cursor: Option<&Cursor>, limit: usize) -> anyhow::Result<Page<SavedRun>> {
        self.store.query_runs(filter, false, cursor, limit).await
    }
    pub async fn history(
        &self,
        filter: &Runs,
        cursor: Option<&Cursor>,
        limit: usize,
    ) -> anyhow::Result<Page<SavedRun>> {
        anyhow::ensure!(
            filter.since_ms.is_some() && filter.before_ms.is_some(),
            "history requires a bounded time window"
        );
        self.store.query_runs(filter, true, cursor, limit).await
    }
    pub async fn waits(&self, filter: &Waits, cursor: Option<&Cursor>, limit: usize) -> anyhow::Result<Page<Wait>> {
        self.store.pending_waits(filter, cursor, limit).await
    }
    pub async fn counts(
        &self,
        filter: &Runs,
        history: bool,
        cursor: Option<&Cursor>,
        limit: usize,
    ) -> anyhow::Result<Counts> {
        anyhow::ensure!(filter.definition.is_some(), "counts require a definition");
        let page =
            if history { self.history(filter, cursor, limit).await? } else { self.list(filter, cursor, limit).await? };
        let mut states = std::collections::BTreeMap::new();
        for run in page.items {
            let state = match run.state {
                ouro_durable::RunState::Ready => "ready",
                ouro_durable::RunState::Running => "running",
                ouro_durable::RunState::Waiting => "waiting",
                ouro_durable::RunState::Completed => "completed",
                ouro_durable::RunState::Failed => "failed",
                ouro_durable::RunState::Cancelled => "cancelled",
            };
            *states.entry(state.into()).or_insert(0) += 1;
        }
        Ok(Counts { states, next: page.next })
    }
    pub async fn entry(&self, id: &str) -> anyhow::Result<Option<Entry>> {
        self.store.control_entry(id).await
    }
    pub async fn journal(
        &self,
        run: Option<&str>,
        cursor: Option<&Cursor>,
        limit: usize,
    ) -> anyhow::Result<Page<Entry>> {
        self.store.control_history(run, cursor, limit).await
    }
}
