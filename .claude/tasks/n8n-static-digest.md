# n8n — the news digest as a static workflow

**Status:** done (built, not deployed — nothing is activated)
**Priority:** P3
**Area:** n8n / infra / comparison study
**Created:** 2026-09-19
**Implemented:** 2026-09-19 (`feat/n8n-news-digest`)

## Context

Our news digest is *compiled*: a planner LLM writes a 5-step plan per scheduler
signal and `workflow.ts` walks it. The question was what the same pipeline looks
like when a workflow engine owns it instead — built up front, in n8n's own
idioms, with no planner in the loop. Reference run: Langfuse trace
`bcd6ebda2eba7db1b3786c4258100d8b` (34.9s, $0.2193, 20 observations).

Result: `n8n/` — three workflow exports (pipeline, reusable MCP client
sub-workflow, error workflow), a compose overlay adding a tool-scoped
`mcp-n8n` instance plus the engine, a dependency-free export linter, and a
README with the step-by-step mapping.

## Decisions that shaped it

- **MCP stays the tool layer**, even though native Postgres + Telegram nodes
  would be less plumbing: PG is MCP's private store, and
  `send_telegram_message` also appends to the `telegram_messages` log that the
  next run's dedup reads. Deliver around MCP → dedup goes blind.
- **A third MCP instance** (`mcp-n8n`, `MCP_TOOLSETS=news-read,telegram,skills`,
  `MCP_NO_POLLERS=1`). Not the supervisor's: the full instance is single-session
  and an arriving `initialize` *evicts* the live session, so n8n on `mcp:3000`
  would knock the agent off. Restricted ⇒ multi-session ⇒ safe.
- **System prompt via `read_skill`**, not pasted into the node, so a skill edit
  (or a `dreaming` revision) reaches the workflow with no JSON change.
- **Chunking moved client-side** (character budget, contiguous) — `chunks: N`
  exists for the plan DSL's static `${bind.chunks.0}` references, which n8n
  doesn't need.
- **Watermark in n8n static data**, stamped only after a confirmed delivery.
  Separate from the agent's `news_digest.last_read_at` → the two must never run
  at once.
- **No retry on `tools/call`** (side effects), retry on the handshake only.

## Acceptance

- [x] `node n8n/scripts/validate-workflows.mjs` clean (2 credential-placeholder
      warnings by design).
- [x] Every Code node dry-run against mocked n8n helpers with trace-shaped data
      (367 posts / 30 history messages): chunking, skill layering, lenient JSON
      parse of map output, delivery guard, watermark stamping, error phrasing.
- [x] `docker compose -f docker-compose.yml -f n8n/docker-compose.n8n.yml
      config` valid.
- [ ] Imported and run once against live MCP — not done; needs the two model
      credentials and a chat id in the editor.

## Notes

- Not activated anywhere. Activating it means cancelling `scheduled_tasks` row
  #4 first, or the digest is posted twice with two independent watermarks.
- Node `typeVersion`s are the conservative 1.x-era ones; image pinned to
  `n8nio/n8n:2.40.3`.
- The obvious next comparison, if this is worth continuing: `tech-digest`
  (search_news fan-out over query facets) — it exercises batch tool args and
  `matchedQueries` dedup, which this one doesn't.
