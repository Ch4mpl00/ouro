# ows

An interpreter for the Open Workflow Specification DSL 1.0.4. Parse and
validate YAML with `model::parse_yaml` and `run::validate`; execute with
`run::run`, or recover with `run::resume` and a `durable::Journal`.

Hosts inject `Functions` (with the optional seams `Events`, `Children`,
`Secrets`, `Resources`, emitting and process runs), `Observer`, and `Clock`.
The interpreter implements the DSL's semantics: flow, jq data
transformations, errors and retries, timers, listen (strategies, filters,
correlation, foreach) and emit, child workflows, extensions, secrets and
schedules (which a host's scheduler reads). Protocol clients (`call: http`,
…), process runners, authentication and catalogs belong to the host; the
supported subset and its choices are in `src/model.rs`. A construct whose
host seam is absent fails validation.

Recovery replays the definition, restoring completed tasks and call results.
Protected calls preserve their logical identity across retries. At-most-once
intent without a response produces `outcome_unknown`; it is never resent.
See the invariants in `src/durable.rs` for the complete persistence contract.

Tests include the official conformance kit (`tests/ctk`, run with
cucumber), the reference model, differential properties, invariants,
metamorphic checks, durable recovery and upstream specification examples.
Upstream schema attribution is in `schema/README.md`.
