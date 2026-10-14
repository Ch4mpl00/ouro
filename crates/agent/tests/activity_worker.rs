// Eligibility, projection progress, drafts, retry/backoff and in-flight guard
// from test/activity-journal.test.ts, against the shared SQLite schema.
use agent::{
    activity::*,
    db::{AgentDb, MemoryStore, StoredTrace, TraceStore},
    mcp::Mcp,
    providers::ToolDef,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::{Map, Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
#[derive(Default)]
struct Notes {
    calls: AtomicUsize,
    failures: usize,
    packets: Mutex<Vec<Value>>,
}
#[async_trait]
impl NoteGenerator for Notes {
    async fn generate(&self, packet: &Value, _: &CancellationToken) -> anyhow::Result<String> {
        self.packets.lock().unwrap().push(packet.clone());
        if self.calls.fetch_add(1, Ordering::SeqCst) < self.failures {
            anyhow::bail!("generation offline");
        }
        Ok("Что попросили.\nЧто сделано.".into())
    }
}
#[derive(Default)]
struct Journal {
    calls: Mutex<Vec<Map<String, Value>>>,
    failures: usize,
}
#[async_trait]
impl Mcp for Journal {
    fn tools(&self) -> Vec<ToolDef> {
        vec![]
    }
    async fn call_tool(&self, name: &str, args: Map<String, Value>, _: &CancellationToken) -> anyhow::Result<String> {
        assert_eq!(name, "journal_append");
        let mut calls = self.calls.lock().unwrap();
        calls.push(args);
        if calls.len() <= self.failures {
            Ok("[tool error] append offline".into())
        } else {
            Ok(json!({"ok":true,"entry":{"id":10}}).to_string())
        }
    }
}
struct Fixture {
    _dir: tempfile::TempDir,
    db: Arc<AgentDb>,
}
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(AgentDb::open(dir.path().join("agent.db")).unwrap());
    db.set("activity_journal.cutoff", "2026-10-01 00:00:00").unwrap();
    Fixture { _dir: dir, db }
}
fn write(db: &AgentDb, id: &str, role: &str, recorded: &str) {
    db.write_trace(&StoredTrace {
        id: id.into(),
        name: "signal:telegram".into(),
        source: Some("telegram".into()),
        skill: None,
        session_id: Some("telegram:42".into()),
        tags: vec!["telegram".into()],
        input: Some(json!("попросили помочь")),
        output: Some(json!("готово")),
        metadata: Some(json!({"activity":role,"skills":["telegram"]}).as_object().unwrap().clone()),
        observations: vec![],
        started_at: "2026-09-29T00:00:00Z".into(),
    })
    .unwrap();
    db.conn().execute("UPDATE traces SET created_at=?1 WHERE id=?2", rusqlite::params![recorded, id]).unwrap();
}
fn now() -> DateTime<Utc> {
    "2026-10-01T12:00:00Z".parse().unwrap()
}
fn due(id: &str) -> DueTrace {
    DueTrace { trace_id: id.into(), completed_at: "2026-10-01 00:00:01".into() }
}
#[test]
fn cutoff_is_persistent_and_only_new_completed_production_runs_are_due() {
    let f = fixture();
    write(&f.db, "old", "run", "2026-09-30 00:00:00");
    write(&f.db, "new", "run", "2026-10-01 00:00:00");
    write(&f.db, "bench", "benchmark", "2026-10-01 00:00:01");
    write(&f.db, "long", "run", "2026-10-02 00:00:00");
    let due = f.db.due_activity(10, now()).unwrap();
    assert_eq!(due.iter().map(|d| d.trace_id.as_str()).collect::<Vec<_>>(), ["new", "long"]);
    assert_eq!(f.db.activity_cutoff(now()).unwrap(), "2026-10-01 00:00:00");
    f.db.activity_done("new", Some(1)).unwrap();
    assert_eq!(f.db.due_activity(10, now()).unwrap()[0].trace_id, "long");
}
#[tokio::test]
async fn empty_poll_calls_neither_note_model_nor_mcp() {
    let f = fixture();
    let notes = Arc::new(Notes::default());
    let journal = Arc::new(Journal::default());
    let worker = create_activity_worker(f.db, notes.clone(), journal.clone(), ActivityOptions::default()).unwrap();
    assert_eq!(worker.tick(&CancellationToken::new(), now()).await.unwrap(), 0);
    assert_eq!(notes.calls.load(Ordering::SeqCst), 0);
    assert!(journal.calls.lock().unwrap().is_empty());
}
#[tokio::test]
async fn appends_one_note_with_code_owned_provenance_then_skips() {
    let f = fixture();
    write(&f.db, "run", "run", "2026-10-01 00:00:01");
    let notes = Arc::new(Notes::default());
    let journal = Arc::new(Journal::default());
    let worker =
        create_activity_worker(f.db.clone(), notes.clone(), journal.clone(), ActivityOptions::default()).unwrap();
    assert_eq!(worker.process_trace(&due("run"), &CancellationToken::new(), now()).await.unwrap(), "done");
    assert_eq!(worker.process_trace(&due("run"), &CancellationToken::new(), now()).await.unwrap(), "skipped");
    assert_eq!(notes.calls.load(Ordering::SeqCst), 1);
    let calls = journal.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["stream"], "ouro/activity");
    assert_eq!(calls[0]["capacity"], 200);
    assert_eq!(calls[0]["metadata"]["trace_id"], "run");
    assert_eq!(calls[0]["metadata"]["projector_version"], 1);
    assert!(calls[0]["body"].as_str().unwrap().starts_with("2026-09-29 00:00 UTC · telegram\n"));
    let row = f.db.activity_progress("run").unwrap().unwrap();
    assert_eq!(row.status, "done");
    assert_eq!(row.entry_id, Some(10));
    assert!(row.draft.is_none());
}
#[tokio::test]
async fn append_retry_reuses_persisted_draft_across_worker_restart_without_more_generation() {
    let f = fixture();
    write(&f.db, "run", "run", "2026-10-01 00:00:01");
    let notes = Arc::new(Notes::default());
    let journal = Arc::new(Journal { failures: 8, ..Default::default() });
    let worker =
        create_activity_worker(f.db.clone(), notes.clone(), journal.clone(), ActivityOptions::default()).unwrap();
    assert_eq!(worker.process_trace(&due("run"), &CancellationToken::new(), now()).await.unwrap(), "retry");
    let draft = f.db.activity_progress("run").unwrap().unwrap().draft.unwrap();
    drop(worker);
    let worker =
        create_activity_worker(f.db.clone(), notes.clone(), journal.clone(), ActivityOptions::default()).unwrap();
    for _ in 0..7 {
        assert_eq!(worker.process_trace(&due("run"), &CancellationToken::new(), now()).await.unwrap(), "retry");
    }
    assert_eq!(f.db.activity_progress("run").unwrap().unwrap().status, "pending");
    assert_eq!(worker.process_trace(&due("run"), &CancellationToken::new(), now()).await.unwrap(), "done");
    assert_eq!(notes.calls.load(Ordering::SeqCst), 1);
    assert!(journal.calls.lock().unwrap().iter().all(|args| args["body"] == draft.body));
}
#[tokio::test]
async fn failing_generation_backs_off_without_blocking_later_runs_and_eventually_gives_up() {
    let f = fixture();
    write(&f.db, "bad", "run", "2026-10-01 00:00:01");
    write(&f.db, "next", "run", "2026-10-01 00:00:02");
    let notes = Arc::new(Notes { failures: 1, ..Default::default() });
    let journal = Arc::new(Journal::default());
    let worker = create_activity_worker(
        f.db.clone(),
        notes.clone(),
        journal.clone(),
        ActivityOptions { max_generation_attempts: 2, ..Default::default() },
    )
    .unwrap();
    assert_eq!(worker.tick(&CancellationToken::new(), now()).await.unwrap(), 2);
    let bad = f.db.activity_progress("bad").unwrap().unwrap();
    assert_eq!(bad.next_attempt_at.as_deref(), Some("2026-10-01T12:00:30.000Z"));
    assert!(bad.draft.is_none());
    assert_eq!(f.db.activity_progress("next").unwrap().unwrap().status, "done");
    assert!(f.db.due_activity(10, now()).unwrap().is_empty());
    assert_eq!(f.db.due_activity(10, now() + chrono::TimeDelta::seconds(31)).unwrap()[0].trace_id, "bad");
    let failing = Arc::new(Notes { failures: 100, ..Default::default() });
    let worker = create_activity_worker(
        f.db.clone(),
        failing,
        journal,
        ActivityOptions { max_generation_attempts: 2, ..Default::default() },
    )
    .unwrap();
    assert_eq!(worker.process_trace(&due("bad"), &CancellationToken::new(), now()).await.unwrap(), "failed");
    assert_eq!(f.db.activity_progress("bad").unwrap().unwrap().status, "failed");
    assert_eq!(notes.calls.load(Ordering::SeqCst), 2);
}
struct WaitingNotes {
    started: tokio::sync::Notify,
}
#[async_trait]
impl NoteGenerator for WaitingNotes {
    async fn generate(&self, _: &Value, cancel: &CancellationToken) -> anyhow::Result<String> {
        self.started.notify_one();
        agent::errors::or_cancel(cancel, std::future::pending()).await
    }
}
#[tokio::test]
async fn in_flight_guard_prevents_duplicate_processing_and_is_released_on_cancellation() {
    let f = fixture();
    write(&f.db, "run", "run", "2026-10-01 00:00:01");
    let notes = Arc::new(WaitingNotes { started: tokio::sync::Notify::new() });
    let worker =
        create_activity_worker(f.db.clone(), notes.clone(), Arc::new(Journal::default()), ActivityOptions::default())
            .unwrap();
    let cancel = CancellationToken::new();
    let signal = cancel.clone();
    let w = worker.clone();
    let task = tokio::spawn(async move { w.process_trace(&due("run"), &signal, now()).await });
    notes.started.notified().await;
    assert_eq!(worker.process_trace(&due("run"), &CancellationToken::new(), now()).await.unwrap(), "skipped");
    cancel.cancel();
    assert!(task.await.unwrap().is_err());
    assert!(f.db.activity_progress("run").unwrap().is_none());
    let w = worker.clone();
    let cancel = CancellationToken::new();
    let signal = cancel.clone();
    let task = tokio::spawn(async move { w.process_trace(&due("run"), &signal, now()).await });
    notes.started.notified().await;
    cancel.cancel();
    assert!(task.await.unwrap().is_err());
}
#[tokio::test]
async fn background_worker_stops_during_idle_wait() {
    let f = fixture();
    let worker = create_activity_worker(
        f.db,
        Arc::new(Notes::default()),
        Arc::new(Journal::default()),
        ActivityOptions::default(),
    )
    .unwrap();
    let cancel = CancellationToken::new();
    let signal = cancel.clone();
    let task = tokio::spawn(async move { worker.run(signal).await });
    tokio::task::yield_now().await;
    cancel.cancel();
    tokio::time::timeout(std::time::Duration::from_secs(2), task).await.unwrap().unwrap().unwrap();
}
