# MCP auth + per-client tool scoping

**Status:** in progress — A6 wired + A2 shipped per-*instance*; A1/A3 token
auth not started
**Priority:** P1
**Area:** mcp / transport, security
**Created:** 2026-08-21

## Context

[unified-memory.md] needs several agents — the droplet supervisor, Claude
Code on the laptop, possibly ChatGPT — talking to the same MCP server.
Today that is impossible and unsafe:

- `runHttpTransport` binds one process-wide `McpServer`; a second
  `initialize` throws `Already connected` (see D1 in unified-memory and
  [mcp-connection-lifecycle.md]).
- The endpoint is **unauthenticated**. It is only safe because compose
  `expose`s port 3000 on the private network instead of publishing it.
  Anything reachable from outside changes that immediately.
- Every client sees **every tool** — Gmail, Telegram send, Monobank,
  fs read, fetch, the signals queue, plus whatever the gateway
  re-exposes from third-party upstreams (e.g. `tavily__*`, which costs
  money per call). A third-party agent must not get `send_telegram_message`
  just because it wanted to read a project document.

## What the clients actually support (verified 2026-08-21)

| Client | Static API key | OAuth | Notes |
|---|---|---|---|
| **ChatGPT** custom connector (developer mode) | **No** | Yes | Explicitly cannot present custom API keys, client-credentials grants, service accounts, JWT bearer assertions or mTLS certs. Auth is OAuth or none. Streamable HTTP / SSE, public HTTPS required. |
| **OpenAI Responses API** MCP tool | **Yes** | Yes | Arbitrary `headers` forwarded on every request, e.g. `Authorization: Bearer …`. Values aren't stored by the API — resent per call. |
| **Claude Code** | **Yes** | Yes | `.mcp.json` `{"type":"http","url":…,"headers":{"Authorization":"Bearer ${ENV}"}}`, or `claude mcp add --transport http … --header`. |
| **Our own agents** | **Yes** | — | We control the client. |

So a **static bearer token covers everything except the ChatGPT UI**.
For that one client the answer is not OAuth but **Secure MCP Tunnel**
(A6), which sidesteps the credential question entirely: ChatGPT
authenticates to OpenAI, and a daemon of ours does the rest.

### What OAuth entails (MCP spec 2025-06-18 → 2025-11-25)

The MCP server is an OAuth 2.1 **resource server only**; the
authorization server is separate (Auth0 / Stytch / Keycloak / WorkOS /
Logto — writing one is not advisable).

1. Unauthenticated request → **401** with
   `WWW-Authenticate: Bearer resource_metadata="https://…/.well-known/oauth-protected-resource"`.
2. Client fetches that **Protected Resource Metadata** doc (RFC 9728,
   MUST implement) → `authorization_servers: [...]`.
3. Client discovers the AS (OAuth AS metadata or OIDC Discovery).
4. Client registration: **CIMD** (Client ID Metadata Documents) is the
   2025-11-25 default, replacing Dynamic Client Registration. ChatGPT
   supports CIMD with public-client (`none`) or `private_key_jwt`.
5. Authorization Code + **PKCE S256** (plain PKCE is banned).
6. Every subsequent MCP request carries `Authorization: Bearer <token>`;
   the server validates it *and* its audience (RFC 8707 resource
   indicators) so a token minted for another resource is rejected.

## Decisions

**A1 — two accepted credential forms, both static-token based in v1.**
The server accepts a token via either:

- `Authorization: Bearer <token>` — preferred; used by Claude Code, our
  agents, the Responses API.
- `?api-key=<token>` in the MCP URL — the escape hatch for clients that
  cannot send headers (ChatGPT UI treats it as a "no auth" connector).

Same token store, same scoping, different carrier. Anything without a
valid token gets 401.

**A2 — the token carries the role and the toolset; clients never declare
their own.** This replaces the "client declares `role=memory` on
initialize" sketch in unified-memory D1/Q9 — self-declared roles are
forgeable and pointless as a boundary. A token row is:

```
mcp_tokens  id, name, token_hash, role, toolsets[], created_at,
            last_used_at, revoked_at
```

`supervisor` is the only role that gets the signals tools; everything
else is **deny-by-default** and must opt into named toolsets
(`memory`, `news`, `telegram-read`, …). Per-session `McpServer`
instances (D1) register only the permitted tools, so an unauthorised
tool is not merely rejected — it is **invisible**, which is also what
keeps prompts small.

> **Shipped so far (2026-08-23):** scoping is **per instance**, not yet
> per token — `MCP_TOOLSETS` picks the groups an MCP process registers,
> and the tunnel gets its own process. Per-session `McpServer` instances
> now exist too (`http-transport.ts`, `multiSession`), enabled for
> restricted instances so several clients can connect at once. The token
> store, `mcp_tokens`, and role-derived scoping are still to do.

**A3 — prefer not publishing the endpoint at all.** Every client can be
served without a public listener:

| Client | Path in |
|---|---|
| droplet supervisor | compose network, as today |
| ChatGPT / Codex / Responses API | **Secure MCP Tunnel** (A6) |
| Claude Code on the laptop | Tailscale or `ssh -L` to the droplet |

If a public endpoint ever *is* needed, TLS via a reverse proxy (Caddy is
the least effort) is a hard prerequisite — but the point of A6 is that
this day may never come, and an unpublished port has no attack surface
to reason about.

**A4 — `?api-key=` is fine once the secret stays inside our
infrastructure.** The original objection was that ChatGPT would store our
token in its "URL" field — a third party holding our credential as
ordinary config. With A6 that disappears: the URL carrying the key is
configured in **tunnel-client**, which runs on our droplet. ChatGPT only
ever knows a `tunnel_id`.

One mitigation remains: query strings land in access logs, so redact the
query in the proxy/app log format and treat those logs accordingly.

Tokens stay **per-client, narrowly scoped and independently revocable**
regardless of carrier — a leaked memory token costs memory contents, not
the Telegram account.

**A6 — use OpenAI's Secure MCP Tunnel for ChatGPT instead of exposing
anything.** `openai/tunnel-client` is a customer-run daemon that makes an
**outbound-only** HTTPS long-poll to an OpenAI-hosted endpoint, pulls
queued MCP requests, forwards them to a local MCP server, and returns
responses the same way. No inbound ports, no public listener, no
WebSocket, no custom protocol.

```
ChatGPT / Codex / Responses API
        ↓ (OpenAI-hosted MCP endpoint)
   [ long-poll over HTTPS, outbound only ]
        ↑
  tunnel-client  (our droplet, new compose service)
        ↓ http://mcp:3000/mcp?api-key=…   ← secret never leaves our box
      mcp
```

- **Credentials:** a `tunnel_id` from Platform tunnel settings plus a
  *restricted* runtime API key with `Tunnels Read + Use` (explicitly not
  an admin or all-access key; the admin key is only for tunnel CRUD).
- **Target:** either stdio (`--mcp-command`) or HTTP
  (`--mcp-server-url`) — we use HTTP against the existing container.
- **Access control** rides on the OpenAI org/workspace the tunnel is
  scoped to, rather than a public ingress. In ChatGPT the connector is
  created with *Connection → Tunnel*.
- **Run it as a service** next to mcp in `docker-compose.yml`; it must
  stay up, since it is the only thing polling for work.

Caveats worth knowing before committing:

- Tunnels are for **private / developer-mode use** — a connector built
  on one can't be submitted as a public plugin. Fine for us.
- **This is OpenAI-only.** Claude Code and our own agents still need
  A1/A3 paths; the tunnel doesn't replace token auth, it removes the
  need for a public endpoint for one family of clients.
- **The tunnel grants access to whatever MCP serves it** — so A2 scoping
  is *more* important here, not less. tunnel-client points at a
  memory-scoped token; otherwise ChatGPT would get `send_telegram_message`.
- An OAuth authorization server, if we ever add one, is **not**
  tunnelled automatically — it must be reachable on its own.
- Role/permission changes take up to ~30 minutes to propagate; a tunnel
  visible in Platform is not necessarily visible in ChatGPT (workspace
  scope + `Use` permission + a healthy daemon are all required).

**A5 — OAuth is deferred.** It buys per-user identity and delegation,
neither of which exists in a one-person system, and costs an external
authorization server plus PRM/CIMD/PKCE plumbing. Revisit when a second
human needs access, or when the ChatGPT UI specifically becomes a daily
driver and a URL-embedded key is no longer tolerable. The design above
doesn't block it: the token check is one middleware, and OAuth adds a
second validator behind the same interface.

## Shipped so far

The ChatGPT slice of A2 + A6 is in `docker-compose.yml` and
`packages/mcp/src/toolsets.ts`:

- Scoping is **per instance, not per token** — `MCP_TOOLSETS` picks the
  toolsets a process registers. That is the whole of A2's *mechanism*
  (deny-by-default, unregistered = invisible) without its *carrier* (the
  token store), which needs A1 first. The `mcp_tokens` table, per-session
  `McpServer`s and the 401 middleware are still to do; until then the
  boundary is one process per audience.
- The tunnel's audience is a second service `mcp-tunnel`
  (`MCP_TOOLSETS=news-read,telegram-send`, `MCP_NO_POLLERS=1`, `expose`
  only) plus `tunnel-client`. Nothing is published.
- A restricted instance never attaches the gateway, so the `tavily__*`
  clause below already holds for this path — upstream tools can't be named
  in the allow-list, so they're excluded wholesale rather than filtered.
  That matters concretely: `.env.mcp` on the droplet holds `TAVILY_API_KEY`,
  so a gateway-fronted tunnel would let ChatGPT spend upstream credits.
- Open hygiene item: `tunnel-client` takes `env_file: .env.mcp`, so a
  third-party binary's process env holds every integration secret we own.
  It only reads `OPENAI_TUNNEL_*`, and the tunnel key is presently a
  project key rather than a `Tunnels Read + Use` restricted one. Splitting
  the key out into its own env file is the tightening step.
- `?api-key=` (A4) is not needed yet: the tunnel reaches an unauthenticated
  endpoint on the private compose network, exactly as the agent does. It
  becomes necessary the moment a second audience shares one instance.

## Acceptance

- Port 3000 is never published. ChatGPT reaches MCP through
  `tunnel-client` running as a compose service; Claude Code reaches it
  over Tailscale/ssh. If a public endpoint is added later, it is HTTPS
  behind a proxy.
- A request with no token, an unknown token, or a revoked token gets 401
  (and, once OAuth lands, a `WWW-Authenticate` pointing at PRM).
- Both carriers work: `Authorization: Bearer …` and `?api-key=…`.
- Two clients with different tokens are connected simultaneously; each
  `tools/list` returns only that token's toolsets, verified by diffing
  the two lists.
- The supervisor's token is the only one whose `tools/list` contains
  `get_next_signal`; a memory-scoped token cannot call it even by exact
  name.
- Gateway-provided third-party tools (`tavily__*`) are scoped like any
  other toolset — a memory client cannot spend upstream credits.
- Proxy logs contain no `api-key` value.
- Revoking a token drops its next request, without restarting MCP.

## Notes

- Related: [unified-memory.md] (the driver; A2 supersedes its Q9),
  [mcp-connection-lifecycle.md] (per-session servers), [mcp-gateway.md]
  (upstream tools need scoping too).
- Claude Code has open issues about headers not being sent during
  session establishment on HTTP transport (anthropics/claude-code
  #29562, #17069) — worth verifying our own client path early rather
  than debugging it during rollout.
- Tool-count pressure is a real second motive for scoping, not just
  security: own tools plus gateway upstreams already put ~25+ tools in
  front of every client.
