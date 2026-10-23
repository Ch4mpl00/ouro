# ouro

An embeddable durable runtime for workflows defined at runtime: declarative
YAML in the [Open Workflow Specification](https://github.com/serverlessworkflow/specification)
DSL, executed by a Rust core, with your own functions as the extension point.

Status: design. The plan and its reasoning are in
[Ch4mpl00/ouro-agent#61](https://github.com/Ch4mpl00/ouro-agent/issues/61);
the code is being extracted from [ouro-agent](https://github.com/Ch4mpl00/ouro-agent),
which becomes this runtime's first consumer.

## Shape

- **Core = DSL semantics.** Flow, data, errors, durable waits, events,
  schedules, child runs and extensions. Protocol calls (`http`, `openapi`,
  `grpc`, …) are functions, not core.
- **Functions are the extension point.** A function declares its input and
  output schemas and its effect (`read` or `write`). The runtime records
  results, never replays a write on resume, mocks writes in dry runs and
  replays recorded traces in tests.
- **Storage through adapters.** SQLite (embedded default), Postgres, more on
  demand; every adapter passes the same conformance suite.
- **Bindings.** A napi-rs package for Node (`npm i ouro`); a server binary for
  other languages later.
- **Observability.** OpenTelemetry, GenAI conventions for model steps.

## Planned layout

```
crates/
  owf/          the interpreter
  durable/      run state, leases, checkpoints, events; the store trait
  store-*/      storage adapters (sqlite, postgres) + the conformance suite
  runtime/      function registry, versions and activation, triggers, OTel
  agent/        agent-loop, guard, confirmations, sub-agents (optional layer)
bindings/
  node/         napi-rs package
```
