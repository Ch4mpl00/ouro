# Injectable logger

**Status:** pending
**Priority:** P2
**Area:** infra / observability
**Created:** 2026-06-01

## Context

`console.log` / `console.error` everywhere. No structured fields, no
levels, no way to attach trace context. The day we want pino /
langfuse-spans / json-logs in prod it's a sweep of every file.

## Acceptance

- `Logger` interface (debug/info/warn/error with structured fields)
  declared once, injected via factory `deps`.
- A default impl that wraps `console` so we don't have to take pino
  immediately — but `createLogger()` factory lives in one place.
- All factories accept `logger: Logger`; modules pass it down to
  storages/repos/pollers.
- No `console.log` in business code (server.ts main + scripts may
  bootstrap-log before the logger exists).

## Notes

Should land together with — or right after — centralized config. Both
are foundational and easier to do once than twice.

Pair `logger.child({ component: "userbot-poller" })` for component
scoping if we adopt pino later.