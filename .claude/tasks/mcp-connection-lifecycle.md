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
restarts ungracefully (which is its normal failure mode — `supervisor`
exits with code 1 on any MCP error and Docker restarts it), the old
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
- Verify: kill the agent ungracefully (`docker kill agent`), let Docker
  restart it, confirm it reconnects without an MCP restart.

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
