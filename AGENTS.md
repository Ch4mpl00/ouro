# Repository conventions

This is an embeddable workflow runtime. Keep application policy outside the
core. Code, comments, fixtures and documentation are in English. Explain
invariants and reasons at the source's existing comment density; avoid narrating
obvious statements.

## Modules and dependency injection

1. One domain lives in one module, with its types, store and service together.
   Grow numbered sections before splitting a domain into unrelated siblings.
2. Seams are traits. Stores, clocks, function handlers and other replaceable
   dependencies are injected as `Arc<dyn Trait>` or generics. Tests supply fakes.
3. Reusable infrastructure declares contracts without importing application
   tables, credentials, naming conventions or policy.
4. An adapter owns its schema, migrations and persistence code. Applied
   migrations never change; add the next numbered SQL file instead.
5. Build long-lived dependencies once in the embedding application's composition
   root. Handlers receive what they need and never construct clients or read
   environment variables during a call.
6. The composition root owns startup and shutdown. Libraries expose services;
   they do not start background workers implicitly.
7. Async orchestration uses Tokio, explicit concurrency bounds and cancellation
   tokens. Child futures stay within their owner's lifetime. Close finalizers
   before closing spans or clients. Storage errors and lease loss stop progress.
8. Recovery preserves admitted definitions, input, bindings and invocation
   identity. Never retry an ambiguous write, including inside a transport.
   A successful record save means durable commit, not merely enqueueing work.
9. Every store adapter passes the public conformance suite, including actual
   persisted-state audits, commit failures and liveness drains.

## Avoid

- Global pools, clients, service locators and business-state `static`/`OnceLock`
  handles. A compiled constant regex or a host-configured telemetry provider
  is a different concern.
- Generic modules importing concrete application schemas.
- Structs that only wrap a constructor and one stateless operation; use a
  function when there is no state or replaceable contract.
- Hidden environment reads or clients created inside request handlers.
- `unsafe`, transmutes or type coercions used to bypass the type system.
- Unbounded spawning, detached workflow tasks, or retries at multiple layers.
- Changing a reference-model rule or regression fixture to make a failing
  implementation pass. Fix the implementation or discuss the rule change.

## Design documents

Non-trivial changes start as a design document in `docs/designs/` (process,
template and the reviewer rubric there): one long-context session writes it,
reviewers with no prior context check it, executors implement strictly by it.
Every caught bug is pinned by a test that fails before the fix, and by the
Quint model when it is a contract rule.

## Working here

Do not run build or development-server scripts; assume the dev preview exists.
Cargo formatting, Clippy, tests and the documented Rust example are the checks
for this workspace. Never use a TypeScript type assertion to conceal an error.

Run:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Postgres conformance uses a disposable `postgres:16` testcontainer, or
`TEST_DATABASE_URL` with permission to create databases. Every case gets its
own database. Tests must never call real external services.
