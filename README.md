# ouro

An embeddable durable runtime for workflows defined at runtime: declarative
YAML in the [Serverless Workflow](https://github.com/serverlessworkflow/specification)
(Open Workflow Specification) DSL, executed by a Rust core, with your own
functions as the extension point. Workflows are data, functions are
registered by the host, there is no server to run, runs survive restarts in a
SQLite, Postgres or in-memory store, and everything is observable through
OpenTelemetry.

## Why

Code-first workflow engines make the workflow a program that is compiled and
deployed with the host. Here the workflow is a document: a program, a person or
a model can write, validate, dry-run, version and activate a
workflow while the host keeps running, and the host decides what its functions
may do. The runtime is a library (three crates) that you embed, not a service
you operate, and it stays small: application policy remains outside the core.

## Example

A workflow is YAML. It calls the function `greet`, which the host registers:

```yaml
document: { dsl: '1.0.4', namespace: example, name: greet, version: '1.0.0' }
do:
  - hello: { call: greet, with: { name: '${ .name }' } }
```

The host registers the function, saves and activates the workflow, starts a run
and drives it to completion. This is `examples/quickstart`, compiled and run by
the checks:

```rust
use ouro_runtime::{
    Config, Function, FunctionContext, Registry, Runtime,
    durable::{self, memory::memory},
    ows::run::Status,
};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

const YAML: &str = r#"
document: { dsl: '1.0.4', namespace: example, name: greet, version: '1.0.0' }
do:
  - hello: { call: greet, with: { name: '${ .name }' } }
"#;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut registry = Registry::new();
    registry.register(Function::new(
        "greet",
        json!({ "type": "object", "required": ["name"], "properties": { "name": { "type": "string" } } }),
        json!({ "type": "object", "required": ["text"] }),
        |args: Value, _cx: FunctionContext| async move {
            Ok(json!({ "text": format!("Hello, {}!", args["name"].as_str().unwrap_or("world")) }))
        },
    ))?;

    let runtime = Runtime::new(
        Arc::new(memory()), // or durable::sqlite::open("runs.sqlite")?
        Arc::new(registry),
        Config::default(),
        Arc::new(durable::SystemClock),
        ouro_runtime::opentelemetry::global::tracer("quickstart"),
    )?;
    let definition = runtime.save("greet", YAML).await?;
    runtime.activate(&definition.name, definition.version).await?;

    runtime.start("run-1", "greet", json!({ "name": "ouro" })).await?;
    let status = runtime.drive("run-1", &CancellationToken::new()).await?;
    assert_eq!(status, Some(Status::Completed(json!({ "text": "Hello, ouro!" }))));
    Ok(())
}
```

Swap `memory()` for `durable::sqlite::open(path)` and a run survives a killed
worker: [`examples/restart`](examples/restart) kills one mid-run and recovers
it with a single write.

## Features

- Registered functions with schemas, at-most-once delivery and annotated output: [API](docs/api.md).
- Explicit call outcomes (not executed, unknown, interrupted) instead of guessed retries: [call outcomes](docs/call-outcomes.md).
- Leases, recovery, schedules, event waits, child runs and breakers on one transactional store contract: [API](docs/api.md).
- OpenTelemetry spans and metrics, and typed events for your own observers: [observability](docs/observability.md).
- Operator control: pause, hold, cancel, respond, inspect, with an audited history: [control](docs/control.md).
- The DSL subset, extensions (jump budget, schedules, overlap policies) and where it is silent: [DSL](docs/dsl.md).
- SQLite (default, WAL, `synchronous = FULL`), Postgres and memory adapters, async throughout: [storage](docs/storage.md).
- Dry runs and replay without side effects: [API](docs/api.md).

## Layout

```text
crates/
  ows/             DSL parser, validation, interpreter, schema and reference tests
  durable/         RunStore, lease Journal, storage adapters, conformance suite
    migrations/    versioned SQLite and Postgres SQL
  runtime/         functions, versions, run driver, observers, dry runs, replay, OpenTelemetry
examples/
  quickstart/      the example above
  restart/         kill a worker, recover its run, verify one write
```

Rust 2024. SQLite is the embedded default, with WAL and `synchronous = FULL`.
Memory and Postgres implement the same transactional contract; none of the
crates depends on another checkout or on an application database.

## Documentation

- [Public API](docs/api.md): registry, runtime, worker, schedules, dry runs.
- [Call outcomes](docs/call-outcomes.md)
- [Observability and observers](docs/observability.md)
- [Operator control](docs/control.md)
- [The DSL](docs/dsl.md)
- [Durability and storage](docs/storage.md)
- [Checks, example and contract model](docs/testing.md)
- [Durable invariants](docs/durable-invariants.md) and the [Quint model](spec/README.md)
- [Design documents](docs/designs/README.md)
- [Roadmap](docs/roadmap.md)
- [Changelog](CHANGELOG.md)

## Status

0.x prerelease: breaking changes are allowed and are listed in the
[changelog](CHANGELOG.md). The design and boundary are described in
[issue #1](https://github.com/Ch4mpl00/ouro/issues/1).

## Checks

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p ouro-quickstart-example
cargo run -p ouro-restart-example
```

Details, Postgres test servers and the benchmarks are in [docs/testing.md](docs/testing.md).
The durable contract is modelled in Quint ([spec/README.md](spec/README.md));
the model and the model-based tests that replay it against every adapter's
public `RunStore` API are described in [docs/testing.md](docs/testing.md#contract-model).
