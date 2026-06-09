# Knowledge base (remember / recall)

**Status:** done
**Priority:** P2
**Area:** mcp / new domain module + agent skill
**Created:** 2026-06-08
**Resolved:** 2026-06-09

## Resolution

Shipped as a new `knowledge` domain module. Deltas from the proposal below:

- **Table** `knowledge_base_notes` (not `kb_facts`). Columns: `id, body,
  tags text[], source, created_at, updated_at, embedded_at, embedding
  vector(1536)`. Migration `0002_wooden_anthem.sql`; GIN index on `tags`,
  no ANN index on `embedding` (same small-corpus rationale as news_items).
- **Tools** `add_note` / `find_notes` (not `remember_fact` / `recall_facts`).
- **Tags:** new requirement — the **agent LLM** generates 3–6 short tags
  per note and passes them in `add_note({ tags })`; MCP stays primitive
  (no chat-completions in the server, only stores after trim/dedup).
  Stored as `text[]`. `find_notes` takes an optional `tags` array-overlap
  filter on top of the vector search.
- **Embedding:** `body` only — tags are NOT vectorised (decided). Folding
  tags into the embedded text later is a one-line change in
  `repository.ts embedRows` + a re-embed, no schema migration.
- **Module shape** is simpler than the proposal implies: there is no
  generic `EmbeddingRepository` interface in the codebase (the doc/task
  referenced one that doesn't exist). `createKnowledgeModule({ db })`
  wires the shared `EmbeddingService` into a `KnowledgeRepository`
  (`addNote` / `findNotes` / `embedMissingBatch`), mirroring news but with
  no poller, no batch-save, no cross-source dedup.
- **Skill:** handled **inline** in `telegram.md` (no sub-agent — each is a
  single tool call). `routing.md` notes add/find are plain MCP tools, not
  a delegable skill.
- **Robustness:** `embed:backfill` extended to also drain notes with a
  NULL embedding (a failed inline embed during `add_note` is recoverable,
  no silent black hole).

Files: `db/pg/schema.ts`, `services/knowledge/{module,repository,index}.ts`,
`tools/knowledge.ts`, `server.ts`, `scripts/embed-backfill.ts`,
`skills.default/{telegram,routing}.md`.

**Left for deploy:** migration applies automatically on next mcp boot
(`pg.ensureReady`); runtime smoke (`add_note` row gets a non-null
embedding, `find_notes` recalls on paraphrase) not yet run locally — no
local Postgres + must not run mcp:serve locally (409). Verify on the
droplet after `docker compose up -d --build`.

---

## Original proposal

## Context

Want a personal knowledge base: I tell the agent in plain language to
remember something ("запомни, что пароль от роутера — …", "запомни,
что Лёша платит за интернет 1-го числа"), it persists the fact, and
later I ask ("что ты помнишь про роутер?", "когда Лёша платит?") and it
recalls the relevant facts.

This is distinct from the two existing memory-ish stores:

- `agent.db memory` — opaque KV for agent internals (watermarks,
  `news_digest.last_read_at`). Not user-facing freeform facts.
- `news_items` (PG + pgvector) — harvested external content (HN/Habr/
  channels). Domain-specific table; per the module rules we don't bolt
  unrelated concerns onto `NewsModule`.

So this is a **new domain** (per CLAUDE.md "add a new domain → add a new
module"). The good news: the generic `services/embeddings/` layer
(`EmbeddingProvider` / `EmbeddingRepository` / `Chunker` +
`createEmbeddingsModule`) and pgvector are already in place — recall is
just semantic search over a new table, reusing that infra exactly like
news does.

## Shape (proposed)

- **Storage:** PG table `kb_facts (id, body text, source text, created_at,
  updated_at, embedding vector(1536))`. Owned by MCP. Drizzle schema in
  `packages/mcp/src/db/pg/schema.ts`, migration via `pnpm db:generate:pg`,
  applied on boot.
- **Module:** `packages/mcp/src/services/knowledge/module.ts` —
  `createKnowledgeModule({ db, embeddings })`. Supplies a
  `KbRepository` (impl of the generic `EmbeddingRepository` for
  `kb_facts`, living with the table per rule #4) and a small service
  with `remember(body, source)` (insert + inline embed, mirroring how
  fetch_article embeds) and `recall(query, k)` (embed query → cosine
  top-k).
- **Tools:** two MCP tools in `packages/mcp/src/tools/`:
  - `remember_fact({ body })` — persist + embed a fact.
  - `recall_facts({ query, limit? })` — semantic search, return top-k.
  Wire both in `server.ts main()` against the new module.
- **Skill:** the agent decides *when* to call these from natural
  language. Either a new `skills.default/knowledge.md` (signal.source
  based) or fold remember/recall guidance into the existing `telegram`
  skill + `routing.md` so a Telegram message like "запомни …" routes to
  it. Likely the latter — these come in as ordinary chat, not a
  dedicated signal source.

## Acceptance

- I can send the bot "запомни, что …" and a `kb_facts` row appears with
  a non-null embedding.
- I can later ask "что ты помнишь про …" / "напомни …" and get the
  relevant fact(s) back, ranked by semantic similarity, not just exact
  match.
- Recall tolerates paraphrase (query wording ≠ stored wording) — that's
  the whole point of going through embeddings rather than KV.
- No new global singletons; module is built in the composition root and
  injected (rules #1, #2, #5, #7). `kb_facts` repo lives in
  `services/knowledge/`, not in `embeddings/` (rule #4).

## Notes / open questions

- **Update vs append:** "запомни, что Лёша теперь платит 5-го" — new
  row or update an existing fact? v1: always append, recall returns
  newest-relevant. Dedup/supersede is a later refinement (could reuse a
  judge to merge), don't build it first.
- **Delete / forget:** "забудь про …" — out of scope for v1, but leave
  room (soft-delete column or a `forget_fact` tool later).
- **Why not agent.db KV:** KV has no semantic recall and lives on the
  agent side; this needs vector search and belongs with MCP-owned PG
  next to the embeddings infra. Keeps the agent talking only through
  MCP tools (architecture invariant).
- **Categorisation/tags:** skip for v1. Embedding similarity covers
  "про роутер" without an explicit taxonomy. Add `source`/free-text tag
  only if recall precision proves insufficient — measure first, like the
  multi-chunk-rag task argues.
