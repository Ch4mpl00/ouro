//! A single process restart, using only public APIs and local files. The
//! ledger stands in for an external service whose effect outlives a worker.
use ouro_runtime::{
    Config, Delivery, Function, FunctionContext, Registry, Runtime,
    durable::{self, Clock},
    owf::run::{Status, WorkflowError},
};
use serde_json::{Value, json};
use std::{
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    process::{Child, Command},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

const YAML: &str = r#"document: { dsl: '1.0.4', namespace: example, name: restart, version: '1.0.0' }
do:
  - fetch: { call: read, with: { value: 42 } }
  - send: { call: write, with: { value: '${ .value }' } }
  - pause: { call: read, with: { value: '${ .value }', pause: true } }
"#;
struct RecoveryClock;
impl Clock for RecoveryClock {
    fn now_ms(&self) -> i64 {
        // Advance past the dead worker's lease instead of making this example
        // sleep for 30 seconds. Production workers use a shared wall clock.
        durable::SystemClock.now_ms() + durable::LEASE_MS + 1
    }
}
fn io_error(e: std::io::Error) -> WorkflowError {
    WorkflowError::runtime(e.to_string())
}
fn registry(dir: &Path, recovering: bool) -> anyhow::Result<Registry> {
    let mut registry = Registry::new();
    let ready = dir.join("ready");
    let input = json!({"type":"object","required":["value"],"properties":{"value":{"type":"integer"},"pause":{"type":"boolean"}},"additionalProperties":false});
    let output = json!({"type":"object","required":["value"],"properties":{"value":{"type":"integer"}}});
    registry.register(Function::new("read", input.clone(), output.clone(), move |a: Value, c: FunctionContext| {
        let ready = ready.clone();
        async move {
            if a["pause"] == true && !recovering {
                // Synchronization for this example only: send has committed
                // its result before this following task can start.
                std::fs::write(ready, b"write recorded").map_err(io_error)?;
                c.cancel.cancelled().await;
            }
            Ok(json!({"value":a["value"]}))
        }
    }))?;
    let ledger = dir.join("ledger");
    // A ledger entry must not be repeated: the function declares at-most-once
    // delivery (calls are at least once unless their function says otherwise).
    let write = Function::new("write", input, output, move |a: Value, _: FunctionContext| {
        let ledger = ledger.clone();
        async move {
            let mut f = OpenOptions::new().create(true).append(true).open(ledger).map_err(io_error)?;
            writeln!(f, "{}", a["value"]).map_err(io_error)?;
            f.sync_all().map_err(io_error)?;
            Ok(a)
        }
    });
    registry.register(write.with_delivery(Delivery::AtMostOnce))?;
    Ok(registry)
}
fn runtime(dir: &Path, recovering: bool) -> anyhow::Result<Runtime> {
    Runtime::new(
        Arc::new(durable::sqlite::open(dir.join("runs.sqlite"))?),
        Arc::new(registry(dir, recovering)?),
        Config::default(),
        if recovering { Arc::new(RecoveryClock) } else { Arc::new(durable::SystemClock) },
        ouro_runtime::opentelemetry::global::tracer("restart-example"),
    )
}
struct Worker(Child);
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.get(1).is_some_and(|a| a == "worker") {
        let dir = PathBuf::from(args.get(2).ok_or_else(|| anyhow::anyhow!("missing example directory"))?);
        let runtime = runtime(&dir, false)?;
        runtime.drive("example", &CancellationToken::new()).await?;
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    {
        let runtime = runtime(dir.path(), false)?;
        let d = runtime.save("restart", YAML)?;
        runtime.activate(&d.name, d.version)?;
        runtime.start("example", "restart", json!({}))?;
    }
    let mut worker = Worker(Command::new(std::env::current_exe()?).arg("worker").arg(dir.path()).spawn()?);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !dir.path().join("ready").exists() {
        anyhow::ensure!(worker.0.try_wait()?.is_none(), "worker stopped before the write was recorded");
        anyhow::ensure!(Instant::now() < deadline, "worker did not reach the pause");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    worker.0.kill()?;
    worker.0.wait()?;
    let runtime = runtime(dir.path(), true)?;
    runtime.recover_once(&CancellationToken::new()).await?;
    let run = runtime.get("example")?.unwrap();
    assert_eq!(run.outcome, Some(Status::Completed(json!({"value":42}))));
    let sends = std::fs::read_to_string(dir.path().join("ledger"))?.lines().count();
    assert_eq!(sends, 1);
    println!("Killed and resumed the worker: completed with value 42; write sent {sends} time.");
    Ok(())
}
