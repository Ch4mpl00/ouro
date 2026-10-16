# Deploy native Rust services with systemd

**Status:** done
**Priority:** P1
**Area:** deployment
**Created:** 2026-10-16

## Context

The last deployment spent 90 s building and about six minutes sending the
entire 1.3 GB Docker image over SSH. The application binaries total 53 MB.
The user approved moving MCP, the supervisor and restricted MCP to systemd,
keeping Postgres, Codex and tunnel-client containerized. Build off the
1 CPU / 2 GB production host. Do not retain a previous release for rollback.

## Acceptance

- Only affected Linux binaries and runtime assets are transferred.
- Test/doc-only changes skip compilation and service restarts.
- Stateful data, credentials, tool scoping and the single Telegram poller survive.
- MCP endpoints, Postgres and Codex stay private to the host.
- Services survive reboot and reconnect in the correct order.
- No previous-release directory or rollback artifact is created.
- The new deployment is running on production and its health is verified.

## Result

Migrated production on 2026-10-16, runtime revision `fbe0aa2` (PR #32 plus
deployment-only fixes). MCP, restricted MCP and the agent run as `ouro`
under systemd; Postgres, Codex and tunnel-client remain containerized.
All application/database listeners bind loopback. Five boot units enabled.

Agent sqlite passed `quick_check` before and after migration, with all 516
traces retained. Native services connected and validated 45 MCP tools;
restricted endpoint and Codex readiness passed. No previous release or
failed-attempt staging directory remains in `/opt/ouro`.

The first native build took 45 s and delivered 54 MB of binaries as a
20 MB compressed transfer (about 16 s), compared with the old 1.3 GB image
and roughly six-minute transfer. A cached completion deploy took 15.5 s,
omitted all unchanged binaries and sent about 53 KB. Readiness uses an MCP
ping without initialization (expected HTTP 422); it cannot evict the agent.

Validation: 87 Rust test programs, the additional address-binding regression,
12 deployment checks, Compose validation and systemd verification passed.
