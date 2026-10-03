# The executable specification

These `.feature` files are what the MCP server must do, in plain English.
They are not documentation next to the tests — they *are* the tests: each
sentence is executed against the real server (over MCP, with a real Postgres
started by Testcontainers) by `tests/specs.rs`.

```bash
cargo test --test specs        # or: pnpm spec   (needs Docker)
```

## Reading them

A scenario is one rule, written as Given (the situation) → When (what an
agent does) → Then (what must be true). A failing scenario names the rule
that broke and shows what the server actually answered.

Results are checked with two forms:

- `Then the result is:` — a table of `field | value`. Fields are dotted paths
  into the JSON answer (`failures.0.reason`); values are JSON (`2`, `true`,
  `null`, `"text"`).
- `Then the result includes:` — a JSON shape; the answer must contain it
  (extra fields are fine, listed arrays must match element by element).

## Writing them

Reuse the sentences that already exist (search this folder); `I call "<tool>"
with:` reaches any tool. A new kind of sentence needs a step in
`tests/specs.rs` — a sentence nobody taught the harness fails the run rather
than being skipped, so a spec can never pass on words that check nothing.

`{{name}}` inserts a value saved earlier with `I remember "<field>" as "<name>"`.

Every scenario starts on an empty, freshly migrated database.

## Ownership

The specification belongs to the person, the code to the agent: an agent may
propose a change to a `.feature` file, but never edits one to make a failing
run pass. Fix the code, or ask.
