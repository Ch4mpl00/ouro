//! Operator access using only a store and a clock, in a console or separate
//! process. The embedding host supplies authorization, request IDs and actors.
//! Every method fails with a typed `ControlError` (not found, conflicting
//! request or response identity, invalid state or input, closed or
//! mismatching wait, stopped, read-only), or `Storage` with its source chain.
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
    pub async fn execute(&self, request: &Request, command: &Command) -> Result<Reply, ControlError> {
        self.store.control(request, command, self.clock.now_ms()).await.map_err(ControlError::from)
    }
    pub async fn cancel(&self, request: &Request, run: &str) -> Result<Reply, ControlError> {
        self.execute(request, &Command::Cancel { run: run.into() }).await
    }
    pub async fn cancel_tree(&self, request: &Request, run: &str) -> Result<Reply, ControlError> {
        self.cancel(request, run).await
    }
    /// Freezes the run and its live owned descendants (see `Command::Hold`).
    pub async fn hold(&self, request: &Request, run: &str, note: &str) -> Result<Reply, ControlError> {
        self.execute(request, &Command::Hold { run: run.into(), note: note.into() }).await
    }
    /// Lets a held run and its held descendants continue (`Command::Release`).
    pub async fn release(&self, request: &Request, run: &str) -> Result<Reply, ControlError> {
        self.execute(request, &Command::Release { run: run.into() }).await
    }
    pub async fn pause(&self, request: &Request, definition: &str, note: &str) -> Result<Reply, ControlError> {
        self.execute(request, &Command::Pause { definition: definition.into(), note: note.into() }).await
    }
    pub async fn resume(&self, request: &Request, definition: &str) -> Result<Reply, ControlError> {
        self.execute(request, &Command::Resume { definition: definition.into() }).await
    }
    pub async fn emergency_stop(&self, request: &Request, enabled: bool) -> Result<Reply, ControlError> {
        self.execute(request, &Command::Stop { enabled }).await
    }
    pub async fn stopped(&self) -> Result<bool, ControlError> {
        self.store.stopped().await.map_err(ControlError::from)
    }
    pub async fn respond(
        &self,
        request: &Request,
        run: &str,
        wait: &str,
        event: serde_json::Value,
    ) -> Result<Reply, ControlError> {
        self.execute(request, &Command::Respond { run: run.into(), wait: wait.into(), event }).await
    }
    pub async fn inspect(&self, run: &str) -> Result<Option<Inspection>, ControlError> {
        self.store.inspect_control(run).await.map_err(ControlError::from)
    }
    pub async fn list(
        &self,
        filter: &Runs,
        cursor: Option<&Cursor>,
        limit: usize,
    ) -> Result<Page<SavedRun>, ControlError> {
        self.store.query_runs(filter, false, cursor, limit).await.map_err(ControlError::from)
    }
    pub async fn history(
        &self,
        filter: &Runs,
        cursor: Option<&Cursor>,
        limit: usize,
    ) -> Result<Page<SavedRun>, ControlError> {
        if filter.since_ms.is_none() || filter.before_ms.is_none() {
            return Err(ControlError::InvalidInput("history requires a bounded time window".into()));
        }
        self.store.query_runs(filter, true, cursor, limit).await.map_err(ControlError::from)
    }
    /// Definition metadata in binary name order. Each bounded page observes
    /// catalog, breaker, schedules and queued slots in one transaction.
    pub async fn definitions(
        &self,
        cursor: Option<&Cursor>,
        limit: usize,
    ) -> Result<Page<DefinitionSummary>, ControlError> {
        self.store.query_definitions(cursor, limit).await.map_err(ControlError::from)
    }
    pub async fn definition(&self, name: &str) -> Result<Option<DefinitionSummary>, ControlError> {
        self.store.definition_control(name).await.map_err(ControlError::from)
    }
    pub async fn waits(
        &self,
        filter: &Waits,
        cursor: Option<&Cursor>,
        limit: usize,
    ) -> Result<Page<Wait>, ControlError> {
        self.store.pending_waits(filter, cursor, limit).await.map_err(ControlError::from)
    }
    pub async fn counts(
        &self,
        filter: &Runs,
        history: bool,
        cursor: Option<&Cursor>,
        limit: usize,
    ) -> Result<Counts, ControlError> {
        if filter.definition.is_none() {
            return Err(ControlError::InvalidInput("counts require a definition".into()));
        }
        let page =
            if history { self.history(filter, cursor, limit).await? } else { self.list(filter, cursor, limit).await? };
        let mut states = std::collections::BTreeMap::new();
        for run in page.items {
            // A held run is counted as held, whatever state it froze in.
            let state = match run.state {
                _ if run.held.is_some() => "held",
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
    /// `definition`'s annotation totals of the runs that ended in
    /// `[since_ms, before_ms)`, rounded out to whole hours
    /// (`TOTALS_BUCKET_MS`), at most `MAX_TOTALS_BUCKETS` of them: the sums
    /// and the hourly buckets that hold any. One bounded read of aggregate
    /// rows each run's terminal commit wrote; no run is scanned.
    pub async fn totals(
        &self,
        definition: &str,
        since_ms: i64,
        before_ms: i64,
    ) -> Result<DefinitionTotals, ControlError> {
        self.store.totals(definition, since_ms, before_ms).await.map_err(ControlError::from)
    }
    /// A page of worker heartbeat rows in id order, each marked stale when
    /// its worker did not beat before its expiry (by this control's clock).
    pub async fn workers(&self, cursor: Option<&Cursor>, limit: usize) -> Result<Page<WorkerStatus>, ControlError> {
        let page = self.store.workers(cursor, limit).await?;
        let now = self.clock.now_ms();
        let items =
            page.items.into_iter().map(|worker| WorkerStatus { stale: worker.expires_ms <= now, worker }).collect();
        Ok(Page { items, next: page.next })
    }
    pub async fn entry(&self, id: &str) -> Result<Option<Entry>, ControlError> {
        self.store.control_entry(id).await.map_err(ControlError::from)
    }
    pub async fn journal(
        &self,
        run: Option<&str>,
        cursor: Option<&Cursor>,
        limit: usize,
    ) -> Result<Page<Entry>, ControlError> {
        self.store.control_history(run, cursor, limit).await.map_err(ControlError::from)
    }
}
