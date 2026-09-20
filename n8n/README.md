# n8n — the news digest, built statically

A side-by-side experiment, not a replacement. `packages/agent` **compiles** a
plan for every scheduler signal (an LLM writes the workflow, the runtime walks
it). This subproject implements the exact same news digest the way n8n wants
it: three declarative workflows, edited on a canvas, versioned as JSON, with no
planner in the loop.

Nothing here imports from `packages/*` and nothing there knows this exists. The
only contract between them is the MCP protocol — the same one the supervisor
uses.

## What it mirrors

Langfuse trace `bcd6ebda2eba7db1b3786c4258100d8b` — a `source=scheduler`
signal from `scheduled_tasks` row #4 (`0 21 * * *`, body: *"Сгенерируй сводку
новостей … Используй навык news-digest … Отправь результат в Telegram"*). The
planner turned it into a five-step plan; the runner executed it in 34.9s for
$0.2193 across 20 observations.

| compiled plan (what the agent ran) | this project |
| --- | --- |
| cron row fires → signal on the queue → supervisor picks it up | **Every day at 21:00** (Schedule Trigger, timezone in workflow settings) |
| `planner` step: gpt-5.4 writes the plan (5.7s, $0.0379) | — the plan *is* the canvas |
| `step[0]` parallel: `list_news(source=channel, sinceISO=<watermark>, chunks=3)` + `get_telegram_chat_history(limit=30)` | **Window + MCP calls** → **MCP: posts + history + skills** — four `tools/call` on one MCP session |
| skills read from disk by the agent runtime (`composer.md` + `news-digest.md`, patch appended) | the same two files, read through `read_skill` in that same batch |
| `step[1]` parallel: 3 × `llm_compose` (preset `base` → gpt-5.4-mini, skill `news-digest`) | **Chunk posts** → **Map: select candidates** (one LLM call per chunk item) |
| `step[2]` `llm_compose` (preset `smart` → gemini-3.7-flash) | **Prepare reduce input** → **Reduce: compose digest** |
| `step[3]` parallel: `send_telegram_message` + `set_memory news_digest.last_read_at` | **Digest deliverable?** → **Build send call** → **MCP: send digest** → **Stamp watermark** |
| `step[4]` terminal | end of the canvas |
| a failed step → `recovery` skill phrases it to the user | **News digest — on error** (n8n error workflow) |

Both prompt strings — the map framing and the reduce framing — are copied
**verbatim** from the compiled plan into the two chain nodes. The digest quality
is tuned against exactly those words; paraphrasing them is the one change that
silently degrades output.

## Files

```
n8n/
├── docker-compose.n8n.yml      overlay: + mcp-n8n (scoped MCP) + n8n
├── .env.n8n.example            n8n's own env (encryption key, timezone)
├── workflows/
│   ├── news-digest-daily.json  the pipeline
│   ├── mcp-tool-call.json      reusable MCP client (sub-workflow)
│   └── news-digest-on-error.json   error workflow
└── scripts/
    ├── validate-workflows.mjs  static lint of the exports (no deps)
    └── import-workflows.sh     validate + n8n CLI import
```

## Design decisions

**MCP stays the tool layer.** n8n could read `news_items` straight from
Postgres and post with the native Telegram node, and it would be less
plumbing. It would also be wrong twice over: Postgres is MCP's private store
(the root `CLAUDE.md` rule), and `send_telegram_message` does more than deliver
— it appends the outgoing text to the `telegram_messages` log in `tokens.db`.
That log is what the *next* run reads through `get_telegram_chat_history` to
avoid re-sending yesterday's events. Deliver around MCP and the dedup goes
blind. So every side effect and every read goes through one reusable
sub-workflow, and the native nodes stay unused on purpose.

**A third MCP instance, not the supervisor's.** The full instance is
single-session with "newest wins" semantics: an arriving `initialize` **evicts**
the live session (`packages/mcp/src/http-transport.ts`). Pointing n8n at
`mcp:3000` would kick the agent off its own MCP. `mcp-n8n` sets
`MCP_TOOLSETS=news-read,telegram,skills`, which makes the instance restricted →
multi-session → safe to connect and disconnect at will, with no signals tools
to race over and no gateway upstreams exposed. `MCP_NO_POLLERS=1` keeps Telegram
`getUpdates` exclusive to the `mcp` service.

**The system prompt is read, not copied.** `read_skill` returns
`effectiveInstructions` — the skill body with frontmatter stripped and the
improver's patch appended, i.e. what the agent actually runs. The workflow layers
`composer.md` then `news-digest.md`, same order as the agent's compose step. A
skill edit (or a `dreaming` self-revision) reaches this workflow on its next run
without touching any JSON. The alternative — pasting the prompt into the node —
is more self-contained and instantly stale.

**Chunking moved into n8n.** The agent asks `list_news` for `chunks: 3` because
its plan DSL can only reference `${bind.chunks.0}`, `${bind.chunks.1}` …
statically — the chunk count has to be known before the data is seen. n8n
iterates over items, so **Chunk posts** splits the window itself, by character
budget (100k) rather than by a fixed count: one map call's context stays flat
whether the window holds 40 posts or 900. Chunks stay contiguous, never
round-robin, because `list_news` is time-ordered and neighbouring posts are the
ones that consolidate into one bullet.

The chunks are then processed concurrently, but by a different mechanism than
the agent's: not three steps in a `parallel` block, one node with **Batch
Size 4**. Same wall time for a 3-chunk window, invisible on the diagram.

**Side effects are never retried.** The handshake nodes retry (idempotent);
`tools/call` does not. A replayed `send_telegram_message` is a second message in
the user's chat. Same rule the agent runtime follows — automatic retries in one
layer only, never over tool side effects.

**The watermark moves last.** `Stamp watermark` runs only after
`send_telegram_message` confirms `delivered: true`, so a failed delivery leaves
the window open and the next run re-reads the same posts. It lives in n8n's own
per-workflow static data (`$getWorkflowStaticData('global')`), which is this
project's equivalent of the agent's `set_memory news_digest.last_read_at`. Note
n8n only persists static data for **production** executions — a manual test run
computes the window from `now − bootstrapHours` and does not advance anything.

**No secrets in git.** Provider keys live in n8n's encrypted credential store;
`telegramChatId` ships as `0` and the first Code node fails loudly if it is
still `0`; the MCP instance takes its integration credentials from the root
`.env.mcp`, exactly like `mcp` and `mcp-tunnel`. The editor is published on
`127.0.0.1:5678` only — reach it over an SSH tunnel, never on the droplet's
public interface.

## First run

```bash
cp n8n/.env.n8n.example n8n/.env.n8n
# generate the encryption key it asks for:
openssl rand -hex 32

docker compose -f docker-compose.yml -f n8n/docker-compose.n8n.yml \
  up -d --build mcp-n8n n8n

n8n/scripts/import-workflows.sh
```

Then, in the editor (`ssh -L 5678:localhost:5678 root@<droplet>` →
<http://localhost:5678>):

1. Create two credentials — an OpenAI one and a Google Gemini (PaLM) one, with
   the same keys as `.env.agent` — and re-select them on the two model nodes.
   The exports carry `REPLACE_WITH_…` placeholders, which
   `validate-workflows.mjs` reports as warnings until you do.
2. **News digest — daily 21:00** → `Config` → set `telegramChatId`.
3. Run it once manually (nothing is stamped, so it is repeatable), check the
   message, then activate the workflow.

⚠️ **Only one of the two should be active.** The agent's `scheduled_tasks` row
#4 and this workflow have separate watermarks, so leaving both on posts the
digest twice and each one advances only its own window. Cancel the row
(`cancel_scheduled_task`) while experimenting here, and re-create it when you
stop.

After any edit in the editor, export back to `workflows/` (**⋯ → Download**),
run `node n8n/scripts/validate-workflows.mjs`, and commit — git is the source of
truth, the running instance is a cache of it.

## What the static version gives up

- **No adaptation.** The planner can react to the signal body — a different
  period, a topical ask, a missing watermark — and it can `replan` mid-run. This
  canvas does one thing. A second trigger and a second prompt is a second
  workflow.
- **No sub-agent escape hatch.** `invoke_sub_agent` / `code_agent` have no
  equivalent here by design. (An AI Agent node with `mcp-n8n` attached as a tool
  server would reintroduce runtime tool choice — and with it the failure modes
  this project exists to avoid.)
- **Parallelism has no shape on the canvas.** A branch is not a thread: n8n
  walks branches one after another, and a node normally loops over its items.
  The agent's three-way `parallel` step is one node here, run per item. It is
  still concurrent — `chainLlm` at `typeVersion 1.7+` fires **Batch Size**
  items at once (`Promise.allSettled`), set to 4 on the map node — but that is
  a node setting you open a panel to see, not something the diagram shows.
- **No per-node judging.** The agent's runs are traced to Langfuse and scored
  per node (`packages/agent/src/judging.ts`). Here the audit trail is n8n's own
  execution list — full input/output per node, kept for failures forever and
  successes for 30 days (`EXECUTIONS_DATA_MAX_AGE`).

## What it gains

- The plan is **inspectable before it runs**, and identical on every run. No
  planner tokens ($0.0379 of the trace's $0.2193), no compile failures, no
  "degrade to the AgentLoop" path.
- Every step's real input and output is one click away in the executions view,
  including the 291k-character `list_news` payload.
- Retry/timeout/error routing are node settings, not code.
- Editing the pipeline is dragging a node, and the diff is a JSON file.

## Caveats worth knowing

- **Node `typeVersion`s are deliberately conservative** (the 1.x-era versions a
  2.x n8n still loads), with one exception: the two chain nodes are on `1.7`,
  the version that introduced batching — below it, `batching.batchSize` is
  ignored and the map phase silently goes back to one chunk at a time. The
  image is pinned to `n8nio/n8n:2.40.3`; after an upgrade, open each node once
  — n8n keeps old versions working but new parameters default in.
- **`mcp-tool-call` is four HTTP requests per batch**, because Streamable HTTP
  is stateful: `initialize` (session id in a response header) →
  `notifications/initialized` → `tools/call` (one per item) → `DELETE`. The
  server answers each POST as an SSE frame, so the parser handles both
  `data: {…}` and plain JSON. Batch your calls into one Execute Workflow node
  and you pay the handshake once.
- **Session hygiene matters.** A restricted MCP instance builds one server per
  session, so the `DELETE` is not optional politeness; leaked sessions leak
  memory in a long-running instance.
