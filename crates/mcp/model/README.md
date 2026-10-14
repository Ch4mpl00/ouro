# The reference model of memory documents

What `write_doc`, `append_doc`, `patch_doc`, `revert_patch`, `read_doc` and
`doc_history` mean, written as plainly as possible, in Lean 4. No database, no
concurrency, no indexing: a document is a body, a version and a list of
patches.

```bash
cargo test --test model     # or: pnpm spec:model   (Docker + Lean toolchain)
cd crates/mcp/model && lake build      # model + proofs only
```

Install Lean once: `curl https://elan.lean-lang.org/elan-init.sh -sSf | sh`.

## What to read

- **`MemoryModel/Model.lean`**: the rules. This file is the specification.
- **`MemoryModel/Theorems.lean`**: the statements at the top. Lean checks
  the proofs below them, so they need no review. If `lake build` succeeds,
  every statement holds for every world and every call.

`Check.lean` (the linearizability search) and `Main.lean` (the JSON protocol
the test talks to) are plumbing.

## How it checks the server

`tests/model.rs` generates random calls and runs them two ways:

1. **Sequentially.** Each call goes to the model and to the real server,
   over MCP with Postgres. The answers must be equal, compared on what the
   contract fixes: ok or error code, version, body, history, and the version
   a conflict reports. Messages, ids and suggestions are not compared.
2. **Concurrently.** 2–4 sessions send overlapping calls to one document.
   The model then searches for one order of those calls, consistent with
   real time, that gives exactly the answers the server gave. If no such
   order exists, the history is not linearizable. A write that lost a
   compare-and-swap race may answer "raced" and change nothing.

On failure, proptest shrinks the input to the smallest call sequence that
still fails and prints it, with the answer the model expected.

## What it found

All three bugs were in `memory.rs` and were invisible to the unit tests and
the Gherkin scenarios:

- **Overlapping quotes.** A quote that matches twice with overlap (`"  "` in
  `"   "`) was taken as unique, and the first match was edited.
- **Concurrent creation.** Two `write_doc` calls creating the same document
  at once failed with a raw database error instead of a version conflict.
- **History and revert under concurrency.** The body and its patch were
  written in two statements, and history was ordered by time. Racing writers
  could therefore reorder the history. `revert_patch` then took an older
  patch for the newest one and restored its whole body, silently discarding
  later writes.

## Changing a rule

The model belongs to the person and the code to the agent, the same as the
`.feature` files. When the server and the model disagree, fix the server or
ask. A deliberate change of behaviour starts in `Model.lean`, and
`Theorems.lean` must still prove.
