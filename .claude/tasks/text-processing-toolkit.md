# Text-processing toolkit — strategy-per-task, by-handle, no inlining

**Status:** pending
**Priority:** P2
**Area:** agent / workflow / tools
**Created:** 2026-06-21

## Context

Surfaced while building the GAIA harness ([[eval-external-agent-benchmarks]]):
many tasks hinge on pulling one fact out of a large body of text (a fetched
web page, a PDF, a table). Today the only move is to inline the whole text
into a prompt and hand it to the expensive/slow `smart` model (DeepSeek v4
pro). That's costly, slow, and — per the GAIA runs — a leak magnet when the
model wants to act but sits in a tool-less compose.

We want a **toolkit of text strategies** and the agent picking the right one
per task/document: map/reduce, keyword search, vector search, raw-to-LLM,
structured parse. Not one-size-fits-all.

## Two hard requirements (the reason this is its own task)

1. **Cheap, approximate assessment of text** — so the planner can CHOOSE a
   strategy before paying to read: size (chars / est-tokens), line count,
   "looks tabular?", language. A `text_stats`-style probe.
2. **Operate on text held in memory WITHOUT inlining it into a prompt.**
   Today you can't: `${var}` substitution always materializes the value
   **by value**, so referencing a big `${doc}` in an `llm_*` step dumps the
   whole thing into the prompt. There is no by-handle path.

   Current nuance: a `tool` step on big text does NOT touch a prompt (flow is
   store → args → MCP handler → bind). But (a) the text still serializes
   across the MCP boundary per call, and (b) no tool can take a *handle* to a
   stored variable — values can only be pushed in by substitution.

## Design — what's a tool vs what's a pattern

Decided in discussion (mirrors the [[eval-external-agent-benchmarks]]
"planner over opaque agent" call): **do NOT build a `text_process` ReAct
tool** — that's the opaque agent we're moving away from (hides the strategy
choice from the trace/judge; if LLM-ReAct inside, re-introduces the tool-call
leak). Instead:

- **Retrieval primitives = deterministic tools** (no LLM-ReAct inside):
  `keyword_search`, `vector_search` (chunk + embed + rank top-k; reuses the
  existing embeddings stack — text-embedding-3-small).
- **map/reduce = a workflow PATTERN** the planner composes from `parallel`
  + `llm_compose` (cheap `base` preset for the map/extract, `smart` for the
  reduce). Not a single tool.
- **structured parse = `code_agent`** (already exists; cheapest + most
  reliable for tables/HTML/lists — beats any LLM).
- **raw-to-LLM = `llm_compose`** (already exists).

### The enabling primitive: by-handle agent-side text tools

Add **agent-side synthetic tools** (precedent: `set_memory` in
`synthetic-tools.ts`, dispatched by the executor which already holds the
`VariableStore`) that take a **variable NAME as a literal handle** and read
the store in-process — text never crosses MCP, never enters a prompt:

| Tool | Takes | Returns (small) |
|---|---|---|
| `text_stats(ref)` | var name | `{chars, est_tokens, lines, looks_tabular, lang}` |
| `keyword_search(ref, terms)` | var name | matching lines / spans |
| `vector_search(ref, query, k)` | var name | top-k chunks (chunk+embed+rank) |
| `read_span(ref, range)` | var name | a slice by offset |

DSL mechanism: the var name is passed as a **literal** (`{"ref": "doc"}`, not
`"${doc}"`), so the executor does NOT substitute it; the synthetic tool does
`store.get("doc")` itself. The store holds the big text (or a handle); only
stats / snippets / top-k ever come back. **Nothing inlined, nothing across
MCP.**

Chosen over server-side document handles (MCP holds the text, returns a
`doc_id`) — that's more scalable but adds MCP-side state + lifecycle (TTL,
eviction, surviving replan passes). Overkill for a personal agent on
page-sized docs; revisit only if texts get huge.

### Strategy selection — via classify → replan

> **Design note (2026-09-06):** the sketch below requires the document to
> survive the replan. Today's `workflow/index.ts` creates a new store per
> pass and carries only `context`, so carrying just `stats` loses `doc`.
> The proposed [[agent-working-memory]] module separates artifact lifetime
> from what the planner sees and would provide this shared data layer.
> That proposal is under discussion; it is not implemented yet.

The planner usually doesn't know the doc's shape until it's fetched → a
data-dependent decision → that's `replan`'s job:

```
fetch → bind "doc"                       (text in store, NOT in a prompt)
text_stats(ref="doc") → bind "stats"     (cheap, by-handle)
replan(context=["stats"])                (planner now sees size/shape)
  → next pass picks the strategy:
     tabular?  → code_agent(ref="doc")
     needle?   → vector_search(ref="doc", query=…) → top-k → map-reduce on top-k
     small?    → llm_compose("${doc}")            (the one deliberate inline)
     global?   → long-context single pass
```

The selection rule lives in `planner.md` (teachable, A/B-able via
`judge-replay --planner-file`).

### map/reduce quality guard

Map step returns structured `{found, quote, confidence}`; then
**programmatically verify the quote is a real substring** of the source chunk
(deterministic string match — kills fabricated citations cheaply). Use
overlapping chunk windows so a fact on a boundary isn't lost by both sides.

## Scope / acceptance

- Executor supports a literal var-name handle to a synthetic tool (the
  by-reference seam); no change to `${...}` by-value substitution.
- `text_stats`, `keyword_search`, `vector_search`, `read_span` as agent-side
  synthetic tools over the `VariableStore` (vector_search reuses the
  embeddings module).
- `planner.md`: strategy-selection rule (structured→code; needle→
  vector_search+map-reduce; small→inline; global→long-context) + the
  classify→replan shape.
- map/reduce as a documented planner-composed pattern (`parallel` extract on
  `base` + verbatim-verify + reduce on `smart`). Only promote to a compiled
  `map_reduce_extract` tool (deterministic fan-out, NOT ReAct) if measurement
  shows the planner can't compose it well.
- **Measured on GAIA** (the harness): does strategy-per-task improve accuracy
  / latency / cost vs inlining everything into `smart`.

## Notes

- This depends on the cause-#2 (tool-call leak) finding: tool-less compose on
  DeepSeek leaks. Run map/extract on a clean-tool-call model (`base` =
  gpt-5.4-mini), reserve `smart` for the final reduce. See
  [[eval-external-agent-benchmarks]].
- Don't reinvent chunking/embeddings — reuse `services/embeddings/` +
  text-embedding-3-small already in the stack.
