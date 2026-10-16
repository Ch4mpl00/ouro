# Deploy native Rust services with systemd

**Status:** in-progress
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
