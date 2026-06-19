# MCP gateway — aggregate own + third-party MCP servers

**Status:** done (Tavily wired, pilot passed) — extract-to-container deferred
**Priority:** P2
**Area:** infra / mcp / agent
**Created:** 2026-06-19
**Implemented:** 2026-06-19 (`feat/mcp-gateway`)

## Context

The agent connects to exactly **one** MCP upstream: `connectMcp()` opens one
`Client`, lists tools once at boot (`mcp-client.ts:85`), and the engine takes one
`McpHandle`. There was no mechanism to add more MCP servers. We want to keep our
own MCP (`packages/mcp`: first-party integrations + pollers + signal queue) **and**
connect a growing set of third-party MCP servers (Tavily first — `web_search` /
extract for the agent, see [[eval-external-agent-benchmarks]] — more over time).

## Decision (revised during research — see below)

**Build a thin in-process aggregator module inside `packages/mcp`.** own-MCP
becomes both an MCP *server* (to the agent) and an MCP *client* (to N third-party
upstreams) and re-exposes their tools under a namespace. The agent still talks to
**one** endpoint (`mcp:3000`) and its code is untouched — the tool list just grows.

### Why this, and why NOT the original plan (adopt mcpproxy-go in a separate container)

The original task picked **mcpproxy-go in a dedicated container** on the premise
that it's a *thin, stateless, config-only* aggregator. Reading both candidates'
source disproved the premise. The hard requirement is that the agent connects to
**one** StreamableHTTP endpoint and lists all tools once — so the gateway must
**merge** every upstream's tools into one endpoint, namespaced. Mapping the field
to two axes (merges-into-one-endpoint × thin/stateless):

| | Merges to 1 endpoint | Thin / stateless |
|---|---|---|
| **TBXark/mcp-proxy** | ❌ serves each upstream under its own path (`/own/`, `/tavily/`) | ✅ |
| **mcpproxy-go** | ✅ `routing_mode:"direct"` → `/mcp/all`, tools as `srv__tool` | ❌ |
| **MetaMCP** | ✅ | ❌ Postgres + Next.js |

- **TBXark** doesn't merge — each upstream is a separate MCP endpoint, so the agent
  would need N connections. Out.
- **mcpproxy-go** *can* merge (verified: `routing_mode:"direct"`, namespaced
  `server__tool`, StreamableHTTP at `/mcp/all`) — but it is **not** thin. Source
  shows a security control-plane: BoltDB-backed quarantine + per-tool approval
  (a new upstream is quarantined by default → its tools are hidden until
  approved), Docker isolation per upstream, PII detection, OAuth, web UI/TUI,
  telemetry. To make it a dumb pass-through you disable all of that by config.
  The "thin, safe SPOF" rationale — the decisive argument in the original plan —
  does not hold for it.

**The "merges AND thin" quadrant is empty among off-the-shelf tools — you only
reach it by building.** Given a small, slowly-growing set of *trusted* upstreams,
a single user, and curation already agent-side, a ~250-line module is a better fit
than bending a heavy security proxy. (User decision, 2026-06-19.)

### Why in-process (not a separate container)

The separate-container argument was failure isolation: a flaky third-party MCP must
not disturb the time-sensitive pollers (Gmail 1 min; Telegram getUpdates is
*exclusive* → a second poller = 409). That argument was strongest against a heavy
opaque binary. For our **own thin code calling remote HTTP upstreams**, the risk is
the same class as own-MCP's existing outbound calls (Gmail API, OpenAI embeddings,
Monobank). Mitigated explicitly: per-call timeout + connect timeout + error
isolation, so a hung/failing upstream can't block a tool call or the pollers.

Kept clean per CLAUDE.md ("new domain → new module"): the gateway is its own module
(`services/gateway/`), isolated at the **module** level, not the process level.
**Container-ready by construction** — extracting it later means swapping the own
client's in-memory transport for a StreamableHTTP client to `mcp:3000`; nothing
else changes. (User decision, 2026-06-19.)

## Implementation

`packages/mcp/src/services/gateway/`:

- **`config.ts`** — loads `packages/mcp/gateway.config.json` (git-tracked,
  secret-free), validates with zod, resolves `${ENV_VAR}` refs from the mcp
  container's env. Returns only enabled + fully-resolvable upstreams; a missing
  file → `[]`, a missing secret → that upstream skipped (logged), never fatal.
- **`client.ts`** — `GatewayClient` over two flavours: `connectOwnClient` (own-MCP
  via in-process `InMemoryTransport`) and `connectRemoteClient` (StreamableHTTP,
  per-call timeout, single-flight reconnect mirroring `mcp-client.ts`).
- **`module.ts`** — `createGatewayModule({ownServer, upstreams})` wires own + remote
  clients, lists each source's tools once, merges them (own un-prefixed, upstream as
  `${prefix}__${tool}`), and serves a **low-level `Server`** whose `tools/list` /
  `tools/call` pass tool **schemas through verbatim** and route by name. Low-level
  (not `McpServer`) precisely to avoid the Zod round-trip — schemas reach the model
  exactly as the upstream declared them.
- Wired in `server.ts main()`: if `loadGatewayConfig()` yields ≥1 upstream, front
  own-MCP with the gateway; otherwise serve own-MCP directly (**zero behaviour
  change** until an upstream is configured).

**Key design choices**
- Namespace separator `__` (OpenAI function names forbid `.`); exposed names
  validated against `^[a-zA-Z0-9_-]{1,64}$`, collisions/illegal names dropped+logged.
- own-MCP tools win collisions (registered first, un-prefixed) — existing skills
  unaffected.
- Static tool list built at boot (agent caches at boot; we deploy in lockstep) —
  no `tools/list_changed` handling needed yet.
- stdio upstreams **not** supported (would spawn child processes in the poller
  process) — config rejects non-`http` transport. This is the trigger to extract
  the gateway to its own container.

## Pilot — PASSED (live, 2026-06-19)

Drove the gateway with own-MCP + Tavily + a deliberately-broken upstream:
1. ✅ **Downstream StreamableHTTP** — connected to Tavily, listed 5 tools.
2. ✅ **Direct namespaced passthrough** — `tavily__tavily_search`,
   `tavily__tavily_extract`, `_crawl`, `_map`, `_research`; input schemas verbatim;
   own tools un-prefixed. (No `retrieve_tools` indirection — the thing that would
   have fought our per-skill allow-list.)
3. ✅ **Graceful degrade** — the broken upstream was skipped (logged), gateway still
   served own + Tavily.

Tests committed: `config.test.ts` + `module.test.ts` (own passthrough via in-memory
Client; 9 tests). The live Tavily check was run manually, not committed (network +
key dependent).

## Onboarding contract — adding a new MCP

Two layers: **connect** (config, no code) + **enable** (skill).

1. Find the server's StreamableHTTP endpoint + auth.
2. **+1 entry** in `packages/mcp/gateway.config.json` (`name`, `url` with
   `${SECRET}` refs, optional `prefix`/`headers`).
3. **+1 secret** in `.env.mcp` (the mcp container's env — gateway runs inside
   own-MCP now, so creds live here, *not* a separate gateway env).
4. Restart mcp → check the boot log `[gateway] aggregated N tools …` for the
   namespaced names.
5. **+1 name** in the relevant skill's `tools:` frontmatter (e.g.
   `tavily__tavily_search`) — without this the tool is connected but invisible to
   the model (curation is agent-side).
6. Restart agent (tool list cached at boot, `mcp-client.ts:85`).
7. Deploy: `git pull && docker compose up -d --build` on the droplet.

Steady state: **3 small edits (config + secret + skill) + redeploy, no code.**

## Notes / follow-ups

- **Tavily**: `gateway.config.json` already has the entry; set `TAVILY_API_KEY` in
  `.env.mcp` on the droplet to activate. Until then the upstream is skipped and the
  gateway stays inert (own-MCP served directly).
- **Enable Tavily for a skill**: add `tavily__tavily_search` (and/or
  `tavily__tavily_extract`) to whichever skill should search the web. Not done yet
  — pick the skill(s) deliberately. Unblocks the `web_search` gap in
  [[eval-external-agent-benchmarks]] (GAIA Tier 1, Phase A).
- **Extract to a container** when: a stdio-based third-party MCP is needed, or a
  remote upstream is observed to destabilize the pollers. Migration = swap the own
  client's in-memory transport for StreamableHTTP to `mcp:3000`.
- **OAuth upstreams** need a persisted auth flow — not a one-line config like
  API-key servers. Out of scope here.
- Relates to [[mcp-connection-lifecycle]], [[centralized-config]],
  [[graceful-shutdown]].
