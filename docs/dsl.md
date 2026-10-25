# The DSL

The core implements the DSL's semantics; what it does not implement are
protocol clients, processes, authentication and catalogs, which are host
functions or packs. `src/model.rs` in `ows` lists the supported subset and
the choices made where the specification is silent (`CHOICE` comments).

## Engine extensions live under `metadata.ouro`

The engine reads its extensions only from an `ouro` object inside `metadata`;
every other `metadata` key belongs to the host and is never read, validated
or rejected. `ouro` must be an object (absent or `{}`: no extensions), and a
key it does not know is a validation error that lists the known ones, so a
typo never falls back to a default (and a document using a newer extension
does not validate on an older engine).

| Where | Keys |
| --- | --- |
| `document.metadata.ouro` | `limits`, `at`, `timezone`, `catchup`, `overlap` |
| a call task's `metadata.ouro` | `delivery`: `at_least_once` or `at_most_once` (tighten only) |
| a `for` task's `metadata.ouro` | `concurrency`: a positive integer, not with `while` |
| a `listen` task's `metadata.ouro` | `hold`: a literal ISO 8601 duration |

A known key on another kind of task is a validation error at its
`…/metadata/ouro/<key>` pointer.

## The jump budget (an ouro extension)

OWS puts no limit on `then:` jumps, so a `switch` that jumps back can loop
forever. Only jumps to a named task count: `then: <task>` on a task, a
`switch` case or a `catch`. `continue`, `exit` and `end` are not jumps; `for`
is bounded by its collection and `retry` by its attempts.

- **One budget per run tree.** A child started with `run: workflow` (or
  `start_child`) and its descendants spend their root's budget, so starting a
  fresh child on every pass multiplies nothing. An independent child
  (`await: false`) starts a tree of its own. Nesting is capped separately:
  a child deeper than `Config.limits.max_depth` (default 10) is refused with
  a catchable `budget.exhausted`, which ends recursion through either kind.
  The branches of a `fork` are part of the same run and share its budget the
  same way: together they may spend no more than one run alone, so branches
  that each fit the ceiling can still exhaust it between them.
- **The ceiling is the host's.** `Config.limits.max_jumps` (default 100) is
  set only by the embedding application; each run is admitted with its budget
  pinned in its runtime bindings (the ceiling for a root, its parent's budget
  for an owned child).
- **A definition may only tighten it**, in `document.metadata.ouro.limits` (the
  OWS document admits no other property; `metadata` is free-form, see
  [Extensions](#engine-extensions-live-under-metadataouro)):

  ```yaml
  document:
    metadata:
      ouro:
        limits:
          max_jumps: 30            # above the host's ceiling: rejected at validation
          on_exceeded:             # optional
            call: report_stuck     # a task (typically a call); the run ends with its output
            with: { state: '${ . }' }
          # on_exceeded: end       # end now, with the jumping task's output
          # omitted: the jumping task raises budget.exhausted (catchable)
  ```

  The error is `https://open-workflow-specification.org/spec/1.0.0/errors/budget.exhausted`
  (status 508), located at the task whose jump was refused; every later jump
  of the tree is refused too. An `on_exceeded` task runs under the jumping
  task, may run at most ten tasks, cannot jump and cannot be caught: its
  output or its fault ends the run. A run that ended through its budget says
  so: `Outcome::limit_exceeded`, the `ouro.limit_exceeded` attribute of its
  span, and the refused decision in its records.
- **Expressions read the counts:** `$run.jumps` (the jumps this run took),
  `$run.maxJumps` (its budget) and `$run.visits.<task>` (how often a task of
  the enclosing lists was entered), so a workflow can leave before the
  budget: `when: '${ .choice == "rewrite" and $run.visits.compose < 3 }'`.
  `run` is reserved: no loop or catch variable takes the name.
- **Validation rejects a loop that can never end:** every jump cycle (a
  strongly connected component of a list's flow graph) needs a conditional
  edge out of it (a switch case, an `if`, a catch's `then`) or a task inside
  that can leave it (a nested `raise` or `end`). This proves nothing about
  termination; the budget does that at runtime.
- **Durable.** Each jump spends one unit under `<task>#jump` with
  `RunStore::spend`, which saves the decision (`Record::Budget`) and moves the
  tree's counter in one fenced commit; a child's unit updates its root's
  counter in that same commit. A restart neither resets the count nor spends
  a unit twice, and replay restores the recorded decisions. A run's first
  refusal is decided in the commit that refuses it (`Record::Exceeded`), and
  every refusal names it: only that jump acts on `on_exceeded`, while fork
  branches refused at the same moment end as it does, so the handler runs
  once and recovery and replay pick the same branch.

- `run: workflow` starts a durable child run of a saved definition (the
  catalog entry named `name`, the active version for `latest`, else the
  saved version whose document has that `version`), owned and joined
  atomically; recovery joins the same child and never reruns a completed
  one. `await: false` starts an independent run. A tripped definition admits
  no child (`urn:ouro:errors:definition.tripped`). `run: container | script |
  shell` need a `ProcessRunner` (`with_process_runner`); it is recorded like
  a function call (`run:<kind>`), with the delivery the runner declares per
  kind.
- `emit` builds a CloudEvents envelope, records it like a call (`emit`, at
  least once), delivers it to the runs listening for it and hands it to an
  optional `EventSink`.
  `publish` delivers an external CloudEvent the same way, idempotently per
  `source` + `id`. `listen` supports `one`, `any` (empty: any event), `all`,
  `until` (a condition or events), filters (literal, regular expression or
  runtime expression per attribute), `correlate`, `read` and `foreach`;
  each event is consumed atomically with its record, and the subscription
  stays open for the whole listen. `deliver` keeps the targeted delivery to
  the oldest waiting run.
- `use.extensions` run `before`/`after` tasks around matching tasks.
  `use.secrets` resolve through a `SecretResolver` (`with_secrets`), are
  readable as `$secrets` in `input.from` only and are redacted from records,
  spans, errors and outcomes. `schema.resource` resolves through a
  `ResourceResolver` (`with_resources`) and is pinned at admission.
- `schedule` (`every`, `cron`, `after`, `on`, and the one-shot `at`) keeps generic
  triggers in the store, armed in the commit that activates the version
  (a version without a schedule removes them, and so does a breaker trip,
  in its own commit): indexed by the time they are next due and by the
  event types they react to. A trigger of an older epoch never
  fires. The `worker` fires due slots on each tick (`fire_due`, a range
  read of the due index); the trigger moves to its next slot in the commit
  that admits the slot's run, whose id derives from the slot, so concurrent
  workers start each once. A missed slot under `document.metadata.ouro.catchup:
  none` is skipped (the default `latest` runs it once, late). Overlap is
  selected in `document.metadata.ouro.overlap`, for unfinished runs of the same
  trigger, waiting runs and earlier epochs included:

  ```yaml
  document:
    metadata:
      ouro:
        overlap: skip    # default: skip with a record
        # allow         # admit each slot concurrently
        # queue         # buffer one slot durably; skip later ones
        # replace       # cancel unfinished runs and their owned children
  ```

  A slot whose run the host refuses to admit (a `MissingFunction` or an
  `InvalidDefinition`) follows the same overlap policy. A queued refusal is
  pinned until promotion; its failed run is recorded and counted then.
  Otherwise, when the overlap guard permits it, the firing's commit consumes
  the slot, stores the run under the slot's id as ended faulted at its
  admission (`configuration` or `validation`; `get`, `list`) and counts the
  fault toward the definition's breaker, so a definition that stays broken
  trips after the threshold and its schedule stops. CHOICE: the slot is
  consumed, not retried at the next tick, which would refire it on every
  tick; it is not lost either, since its failed run holds it. Only such
  deterministic refusals count: a schema resolver that fails
  (`ResourceUnavailable`) interrupts the admission, so the trigger keeps
  the slot, uncounted, and retries it after a backoff of 1 s doubling to
  five minutes, stored with the trigger (`retry` in its state); an `on`
  schedule leaves the event untaken and `publish` returns the error.

  `Config::max_concurrent_runs` (default 10, positive) is the host's ceiling
  across all unfinished runs of a definition. A schedule slot at it is
  skipped with `overlap: ceiling` in its skip record; replacement releases
  its old runs' capacity first. Explicit starts and children count toward
  this ceiling; the schedule policy does not refuse them. Workers sharing a
  store use the same ceiling. A queue captures the ceiling with its pinned
  definition, input and bindings; it respects it when buffering and promoting.
  `Runtime::queued(name)` exposes these buffers. Promotion commits with the
  previous run's end, and its admission time and timeout start then. A new
  activation or breaker trip discards the replaced epoch's buffers. Every
  policy keeps the original slot as the run ID across restarts, and policy,
  cancellation, trigger movement and admission share a transaction. Skips
  are recorded in the trigger's state and time skips are traced. `after`
  re-arms when its run ends, never across epochs; a skip leaves a future
  retry. `on` uses the same overlap policies, takes a matched event's identity
  even on a skip or buffer, keeps the events
  matched so far in its trigger and starts the workflow, with the consumed
  events as input, once the strategy is satisfied (a refused run consumes
  the match like a refused slot); a publish reads only the triggers indexed
  under its type. An identified event is taken from
  ingress by at most one trigger (a tombstone kept for
  `Policy::ingress_ms`, default seven days): one event starts at most one
  run.

  Three extensions of a schedule live in `document.metadata.ouro`, because the
  official `schedule` object admits no other property:

  ```yaml
  document:
    metadata:
      ouro:
        timezone: Europe/Berlin          # cron on this IANA zone's wall clock; default UTC
        at: '2030-01-31T09:00:00+01:00'  # a one-shot, instead of a `schedule`
  ```

  `timezone` applies to `cron` only (the compiled-in tz database of
  `chrono-tz`; an unknown name is a validation error, so is the combination
  with `every`, `after` or `on`). A daylight saving transition makes some
  local times not exist (a gap, the clock jumps forward) or exist twice (an
  overlap, it goes back). The rule is the one of `croner` (after Vixie cron),
  pinned by tests. A job is fixed-time when its second, minute and hour are
  single values (`30 2 * * *`); anything with a wildcard, list, range or
  step (`0 * * * *`, `*/15 2 * * *`) is an interval job.

  | | gap (02:30 skipped) | overlap (02:30 twice) |
  | --- | --- | --- |
  | fixed-time | once, at the first instant after the gap (03:00) | once, at the first of the two instants |
  | interval | no occurrence inside the gap, none made up | at both passes |

  Every slot is a distinct instant after the previous one, so none repeats.

  `at` is a schedule of its own (an RFC 3339 timestamp with an offset; a
  `schedule` next to it is a validation error) and follows the same `catchup`
  and `overlap` metadata. Its single slot consumes the trigger however it
  ends: a run, an overlap or ceiling skip (recorded in the trigger's state),
  a queue (the buffered slot starts later, from the queue, not the trigger)
  or a refusal. The trigger then stays as a dormant record (`done`, no due
  time) that nothing re-arms, not even the end of its run. A one-shot fires
  once per `at` value: its run id is the instant's
  (`schedule:<name>:at:<ms>`), and an activation (a deploy, clearing a breaker
  trip or a pause) arms the instant again, but a run that exists under that id
  means it ran, so the slot is consumed (recorded as skipped, `already run`)
  without a second run. A changed `at` is another id and runs. An instant
  already past at its first activation fires at the next tick, whatever
  `catchup` says: nothing follows a one-shot, so it can only run late or not
  at all.

  A run started by a trigger reads `$run.schedule`: `{ kind, slot,
  previous }` (`every`, `cron`, `after`, `at` or `on`), with `slot` and
  `previous` as datetimes like `$workflow.startedAt` (`iso8601`, `epoch`).
  `slot` is the scheduled instant, not the start time, so a late run still
  sees the slot it is for (for `on`, the instant the matching event
  completed the strategy). `previous` is the last slot the trigger started a
  run for (`null` for the first, always for `at`); a slot skipped by overlap
  does not count, a refused one does. It is pinned in the admission, so a
  recovered or replayed run sees the same values, and absent (`null`) for a
  run that was not started by a trigger.
