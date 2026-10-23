# owf

An interpreter for the Open Workflow Specification DSL 1.0.4. Parse and
validate YAML with `model::parse_yaml` and `run::validate`; execute with
`run::run`, or recover with `run::resume` and a `durable::Journal`.

Hosts inject `Functions`, `Events`, `Observer`, and `Clock`. The interpreter
implements flow, jq data transformations, errors and retries, timers,
events, reusable functions and extensions. Protocol clients and executable
processes belong to host functions. Unsupported constructs fail validation.

Recovery replays the definition, restoring completed tasks and call results.
Protected calls preserve their logical identity across retries. At-most-once
intent without a response produces `outcome_unknown`; it is never resent.
See the invariants in `src/durable.rs` for the complete persistence contract.

Tests include the reference model, differential properties, invariants,
metamorphic checks, durable recovery and upstream specification examples.
Upstream schema attribution is in `schema/README.md`.
