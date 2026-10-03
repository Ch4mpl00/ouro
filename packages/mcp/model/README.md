# The reference model of memory documents

What `write_doc`, `append_doc`, `patch_doc`, `revert_patch`, `read_doc` and
`doc_history` mean, written as plainly as possible: a document is a body, a
version and a list of patches. No database, no concurrency, no indexing, no
async — `step(world, call)` returns the next world and the answer.

```bash
pnpm test                         # includes properties.test.ts (the model alone, ~1 s)
pnpm spec:model                   # the real server against the model (Docker)
TEST_DATABASE_URL=postgres://… pnpm spec:model
MODEL_CASES=1000 pnpm spec:model  # more cases; default 200 (properties: 1000)
```

It is a line-by-line port of the Lean model on `feat/rust-rewrite`
(`crates/mcp/model/MemoryModel/Model.lean`), checked with fast-check instead
of Lean.

## What to read

- **`model.ts`** — the rules. This file is the specification.
- **`properties.test.ts`** — what must hold of the model: the six theorems
  the Lean model proves, here checked on thousands of random reachable
  worlds. Lean proved them for every world; fast-check samples, and on a
  failure shrinks to the smallest call sequence that breaks one.

`calls.ts` (generated calls), `answers.ts` (both sides in one shape),
`linearizable.ts` and `server.check.ts` are plumbing.

## How it checks the server

`server.check.ts` generates random calls and runs them against the real
server over MCP, with Postgres:

1. **Sequentially.** Each call goes to the model and to the server. The
   answers must be equal, compared on what the contract fixes: ok or error
   code, version, size, body, history, and the version a conflict reports.
   Messages, ids and suggestions are not compared.
2. **Concurrently.** 2–4 sessions send overlapping calls to one document.
   The check then searches for one order of those calls, consistent with
   real time, that makes the model give exactly the answers the server gave.
   If none exists, the history is not linearizable. A write that lost a
   compare-and-swap race may answer "raced" and change nothing.

Calls are generated abstractly ("a quote from the current text", "the
current version", "the newest patch") and made concrete against the model's
view, so most of them reach the rules instead of bouncing off validation.

## What it catches

Each of the three bugs the Lean model found in the Rust port also existed in
the TS server; with the fixes reverted one at a time, this check finds them.
Detection is a matter of probability, measured at the default 200 cases:

| Reverted fix | Caught |
|---|---|
| overlapping quotes counted as unique | ~4 runs in 5 |
| concurrent creation leaks a unique violation | every run |
| body and patch in two statements, history by time | ~2 runs in 3 |
| compare-and-swap dropped from the UPDATE | every run |

The generator decides what is found: overlapping quotes need repeated
characters, so `text()` sometimes produces a run of one character — before
that, 100 cases almost never hit the bug. Raise `MODEL_CASES` before a
release.

What it cannot see: anything the generator never produces (other projects,
`under_heading`, summaries, facts), and anything not observable through the
answers.

## Changing a rule

The model belongs to the person and the code to the agent, the same as the
`.feature` files. When the server and the model disagree, fix the server or
ask. A deliberate change of behaviour starts in `model.ts`, and
`properties.test.ts` must still pass.
