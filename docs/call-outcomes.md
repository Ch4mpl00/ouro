# Call outcomes

A handler's error is a definite failure unless its type says how far the
call got. Three constructors build those types; a process runner and a dry
run's mock answer the same way:

| Error | Recorded as | The workflow sees | Recovery and retries |
| --- | --- | --- | --- |
| any other `FunctionError` | `failed` | the error, caught like any | restored; a retry sends the call again unless it is at most once |
| `FunctionError::not_executed(detail)` (`urn:ouro:errors:call.not_executed`, 503) | `not_executed`: the call refused before doing anything | the error, caught like any | restored, not sent again; a retry (a new attempt) may send it again, an at-most-once call included |
| `FunctionError::unknown_outcome(detail)` (`urn:ouro:errors:call.unknown`, 409) | at most once: `unknown`, the intent without an answer; at least once: `pending` | at most once: `urn:ouro:errors:call.unknown` with the call's reference, handled only by a `catch` that names that type; at least once: the error, caught like any | at most once: never sent again, by recovery or a retry; at least once: may be sent again |
| `FunctionError::interrupted(detail)` (`urn:ouro:errors:call.interrupted`, 499) | nothing new: the intent stays | nothing: the run stops `Interrupted` (an at-most-once call is unknown instead) | the run recovers and sends an at-least-once call again |

A function that did not even send its request (its own validation refused,
its client could not connect) answers `not_executed`; one whose request went
out and whose answer never came (a timeout, a dropped connection) answers
`unknown_outcome`; a plain error is an answer the recipient gave. When in
doubt, it is unknown: only a proof that nothing happened is `not_executed`.

An unknown at-most-once outcome is the same record a crash between the
dispatch and the commit of its answer leaves, so a recovered run raises the
same error and takes the same branch. The handler's own detail reaches the
observers and the call's span, not the workflow, since recovery after such a
crash could not reproduce it. A catch-all `catch` or a status filter does not
take an unknown outcome for a definite failure: the run faults with it (an
unhandled error for the breaker) unless a `catch` names its type, and that
`catch` never retries the attempt. For example, a carrier API that creates a
shipment and accepts no idempotency key, so `create_shipment` is at most
once:

```rust
Function::new("create_shipment", input, output, |args: Value, cx: FunctionContext| async move {
    match carrier.create(&args, &cx.span).await {           // the host's client
        Ok(shipment) => Ok(json!({ "shipment": shipment.id })),
        Err(e) if e.not_sent() => Err(FunctionError::not_executed(e.to_string())),
        Err(e) if e.timed_out_after_send() => Err(FunctionError::unknown_outcome(e.to_string())),
        Err(e) => Err(FunctionError::communication(e.status(), e.to_string())),
    }
})
.with_delivery(Delivery::AtMostOnce)
```

```yaml
do:
  - ship:
      try:
        - create: { call: create_shipment, with: { order: '${ .order }' } }
      catch:
        errors: { with: { type: urn:ouro:errors:call.unknown } }
        as: unknown              # its instance names the create's task
        do:
          - find: { call: find_shipment, with: { order: '${ .order }' } }
          - escalate:
              if: '${ .shipment == null }'
              call: open_ticket
              with: { about: '${ $unknown.instance }' }
```

The workflow continues with the shipment the lookup finds, or escalates when
there is none; the create is never sent twice (`crates/runtime/tests/outcomes.rs`
runs this definition). A refusal is different: `try` with `errors:
{ with: { type: urn:ouro:errors:call.not_executed } }` and a `retry` sends the
call again, since nothing was done.

**Cancellation.** `FunctionContext::cancel` fires when the run is cancelled (in
this process at once, from another one at its next lease renewal), when the
worker stops, and when a renewal finds the lease lost or the emergency stop
set. The runtime does not wait for the handler: it drops the handler's future,
even one that never looks at the token, so a handler that started work outside
its future (a remote job, a spawned task) watches the token to stop it. What
the handler would have answered is not recorded: the call keeps its intent,
`unknown` if it is at most once (never sent again), `pending` otherwise (sent
again when the run recovers on another worker, never for a cancelled run).
