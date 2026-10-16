#!/usr/bin/env python3
"""Readiness probes that never initialize or replace an MCP session."""
import sys
import time
import urllib.error
import urllib.request


def ready(service):
    port = {"mcp": 3000, "mcp-tunnel": 3001, "codex": 3010}[service]
    path = "/health" if service == "codex" else "/mcp"
    request = urllib.request.Request(
        f"http://127.0.0.1:{port}{path}",
        method="GET" if service == "codex" else "POST",
        headers={"Accept": "application/json, text/event-stream"},
    )
    try:
        with urllib.request.urlopen(request, timeout=2) as response:
            return service == "codex" and response.status == 200
    except urllib.error.HTTPError as error:
        # An empty POST is rejected before a session is allocated.
        return service != "codex" and error.code == 400
    except (OSError, TimeoutError):
        return False


def wait(service, timeout=90):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if ready(service):
            print(f"[health] {service} ready", flush=True)
            return
        time.sleep(0.5)
    raise RuntimeError(f"{service} did not become ready in {timeout}s")


if __name__ == "__main__":
    wait(sys.argv[1])
