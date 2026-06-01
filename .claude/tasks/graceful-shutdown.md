# Graceful shutdown

**Status:** pending
**Priority:** P1
**Area:** infra / lifecycle
**Created:** 2026-06-01

## Context

`server.ts main()` has no SIGTERM/SIGINT handler. On `docker compose
down` the HTTP server, the pg pool, the userbot poller (gramjs
session), the gmail poller and the scheduler all die mid-operation.
Symptoms today: in-flight `news_items` upserts can leave the row
without an embedding (backfill catches it), and gramjs sometimes logs
an ungraceful disconnect on restart.

## Acceptance

- Top-level SIGTERM + SIGINT handler in `server.ts main()`.
- Pollers expose `stop()` that clears their interval and awaits the
  current tick (no new ticks fire; in-flight ones complete).
- `pg.close()` runs after pollers stopped, after HTTP server stopped.
- Userbot gramjs client disconnects cleanly.
- Total shutdown budget: 10s (matches default Docker stop timeout). If
  we exceed it, exit with code 1.

## Notes

Pattern (rough):

```ts
const stoppers: Array<() => Promise<void>> = [];
stoppers.push(startUserbotPoller({...}).stop);
...
process.on("SIGTERM", async () => {
  await Promise.all(stoppers.map((s) => s()));
  await pg.close();
  process.exit(0);
});
```

Each `startXxxPoller` returning `{ stop }` is a small but uniform
change across all four pollers.
