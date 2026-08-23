# MCP HTTP connection lifecycle

**Status:** pending
**Priority:** P1
**Area:** mcp / transport
**Created:** 2026-06-15

## Context

`runHttpTransport` in `packages/mcp/src/server.ts` uses a single
process-wide `McpServer` instance and calls `server.connect(transport)`
on each `initialize` request. The SDK does not allow a `Server` to be
connected to a second transport — the second `connect` throws
`Already connected to a transport`.

This is intentional today: there's exactly one agent, and **only one
connection is allowed on purpose**. The signals queue has no
delivery-coordination — two connected agents would race / compete for
the same signals (double-processing, lost work). So single-connect is a
correctness constraint, not just a simplification.

The problem is resilience, not concurrency. When the agent container
restarts ungracefully (which was its normal failure mode — `supervisor`
exited with code 1 on any MCP error at startup and Docker restarted
it; see the client half of the update below), the old
session's transport is never closed, so `transport.onclose` never fires
and the stale session is never removed. The single `server` stays bound
to the dead transport. Every subsequent `initialize` from the
reconnecting agent then throws `Already connected` → HTTP 500 → agent
fatal → restart → 500 again. Self-sustaining crash-loop that only the
**MCP** container restart can break.

Incident 2026-06-15: agent in `Restarting (1)` loop for an extended
period; MCP logs spammed `Already connected to a transport` on every
POST. Fixed by `docker compose restart mcp` then `restart agent`.

## Acceptance

Resilience (P1, the real outage path):

- A reconnecting agent always succeeds, even after the previous session
  died without a clean close. Concretely: on a new `initialize`, evict
  any existing session before binding the new one — close the old
  transport and/or detach the old server so `server.connect` is valid
  again. Single-connect semantics preserved: at most one live session,
  newest wins.
- The agent survives an MCP that is unreachable or erroring **at
  startup**: it retries the initial connect with capped backoff and
  logs each attempt, instead of exiting 1 and letting Docker be the
  retry loop. Deterministic startup failures (missing env var, sqlite
  migration, a skill naming an unknown MCP tool, a 4xx from the MCP
  endpoint) stay fatal — retrying those forever only hides them.
- Verify: kill the agent ungracefully (`docker kill agent`), let Docker
  restart it, confirm it reconnects without an MCP restart.

> **Update 2026-08-23 (later): the P1 resilience item is DONE, both
> sides.**
>
> *Server* (`packages/mcp/src/http-transport.ts`): a new `initialize`
> evicts the bound session ("newest wins") before connecting, so a
> reconnecting agent always gets in. Covered by
> `http-transport.test.ts`, which fails without the eviction.
>
> *Client* (`packages/agent/src/mcp-client.ts`): `connectMcp` takes a
> `startupRetry` policy and the supervisor passes `RETRY_UNTIL_UP`
> (1s → 30s capped, unbounded attempts, one log line per attempt). The
> mid-flight reconnect inside `callTool` uses the bounded
> `CONNECT_RETRY` instead, so a tool call the workflow executor is
> awaiting can still fail rather than hang. `isFatalConnectError` keeps
> 4xx (bar 408/429) and `UnauthorizedError` fatal, and `MCP_URL` is
> parsed once outside the loop so a typo still crashes on boot.
> Covered by `mcp-client.test.ts`, which fails without the retry.
>
> Two halves because either one alone leaves a hole: the eviction stops
> the 500 that started the loop, the client retry stops *any* startup
> blip (an MCP that is merely slow to boot — compose has no healthcheck
> gate on `mcp` for the agent) from becoming exit-1-restart-repeat.
>
> Not verified: nobody has reproduced the live crash-loop against a real
> ungracefully-killed container. The `docker kill agent` check above is
> still owed.
>
> Runtime detachment was investigated and is NOT a third hole: the SDK's
> `StreamableHTTPClientTransport` does not close itself on a failed
> `send`, so after an MCP restart the agent's next call gets the 400
> "No valid session id", `isSessionLostError` fires, and it reconnects.
> The poll loop's own catch+sleep covers the interval.
>
> It recurred first: after an `mcp` restart the supervisor hit
> `Already connected` → 500 → exit 1 → restart, 78 times, and only a
> manual `docker compose restart mcp` broke it. Exactly the 2026-06-15
> incident, so the "resilience, not concurrency" framing below was right.
>
> **Update 2026-08-23.** Multi-connect now exists for *restricted*
> instances only: `runHttpTransport({ multiSession })` builds an
> `McpServer` per session, and `main()` turns it on exactly when
> `MCP_TOOLSETS` narrows the surface. Such an instance has no signals
> tools, so the delivery race below cannot occur — the constraint is
> sidestepped, not solved. The full instance still shares one server and
> still throws `Already connected` on a second `initialize`; the P1
> resilience item is therefore **still open**.
>
> Found the hard way: `tunnel-client` holds a probe session of its own,
> so ChatGPT arriving through the tunnel was permanently the second
> connection and got HTTP 500 every time.

Multi-connect (P3, deferred — do NOT start without solving delivery):

- Only meaningful once the signals queue coordinates delivery so
  multiple agents don't compete for the same signal. Needs a claim /
  lease mechanism on `signals` (atomic claim, visibility timeout,
  ack/nack, redelivery on agent death) before more than one connection
  is ever allowed.
- Until that exists, the server should actively reject a second
  concurrent connection rather than silently corrupt delivery.

## Notes

- "newest wins" eviction is the smallest fix and matches the
  one-agent deployment: a fresh `initialize` means the old agent is
  gone. Alternative (per-session `McpServer` factory) only makes sense
  bundled with the multi-connect + delivery work — don't do it just to
  dodge the `Already connected` error.
- Related but distinct: [graceful-shutdown.md] handles the *server*
  side stopping cleanly; this task handles the *server tolerating a
  client that died uncleanly*.
