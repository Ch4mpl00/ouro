import { UnauthorizedError } from "@modelcontextprotocol/sdk/client/auth.js";
import { StreamableHTTPError } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import type { Transport } from "@modelcontextprotocol/sdk/shared/transport.js";
import { describe, expect, it } from "vitest";
import { z } from "zod";
import {
  connectMcp,
  connectWithRetry,
  isFatalConnectError,
  CONNECT_RETRY,
  RETRY_UNTIL_UP,
  type ConnectRetryPolicy,
  type TransportFactory,
} from "./mcp-client";

// Client half of the crash-loop fix (.claude/tasks/mcp-connection-lifecycle.md).
// The supervisor used to connect to MCP with a bare `await connectMcp()`; any
// failure fell through to `main().catch` → exit(1) → Docker restart → the same
// failure. Production hit it twice (2026-06-15, and 2026-08-23 with 78
// restarts). The server-side eviction fix removes the usual *cause* of the
// 500; these tests cover the agent surviving it (and an MCP that is simply not
// up yet) regardless.

// Sleep is injected everywhere below so the backoff is asserted, not waited on.
function recordingSleep(): { slept: number[]; sleep: (ms: number) => Promise<void> } {
  const slept: number[] = [];
  return {
    slept,
    sleep: async (ms: number) => {
      slept.push(ms);
    },
  };
}

// The 500 the reconnecting agent actually saw on the droplet: the MCP still
// held the dead session of the agent it just replaced.
function alreadyConnected(): StreamableHTTPError {
  return new StreamableHTTPError(500, "Error POSTing to endpoint: Already connected to a transport");
}

describe("isFatalConnectError", () => {
  it("treats connectivity failures and 5xx as retryable", () => {
    // No HTTP status at all: ECONNREFUSED / DNS / socket reset.
    expect(isFatalConnectError(new Error("fetch failed"))).toBe(false);
    expect(isFatalConnectError(alreadyConnected())).toBe(false);
    expect(isFatalConnectError(new StreamableHTTPError(503, "unavailable"))).toBe(false);
    // The two 4xx that mean "come back later".
    expect(isFatalConnectError(new StreamableHTTPError(408, "timeout"))).toBe(false);
    expect(isFatalConnectError(new StreamableHTTPError(429, "slow down"))).toBe(false);
  });

  it("treats a deterministic rejection as fatal", () => {
    // Wrong MCP_URL path — the server is up and says so. Retrying forever
    // would turn a one-line config typo into a container that looks alive.
    expect(isFatalConnectError(new StreamableHTTPError(404, "not found"))).toBe(true);
    expect(isFatalConnectError(new StreamableHTTPError(403, "forbidden"))).toBe(true);
    expect(isFatalConnectError(new UnauthorizedError())).toBe(true);
  });
});

describe("connectWithRetry", () => {
  it("retries a transient failure and returns the eventual success", async () => {
    const { slept, sleep } = recordingSleep();
    let attempts = 0;
    const open = async (): Promise<string> => {
      attempts++;
      if (attempts < 3) throw alreadyConnected();
      return "connected";
    };

    await expect(connectWithRetry(open, CONNECT_RETRY, { sleep })).resolves.toBe("connected");
    expect(attempts).toBe(3);
    expect(slept).toEqual([500, 1000]);
  });

  it("caps the backoff instead of drifting into hour-long sleeps", async () => {
    const { slept, sleep } = recordingSleep();
    let attempts = 0;
    const open = async (): Promise<string> => {
      attempts++;
      if (attempts < 12) throw new Error("fetch failed");
      return "connected";
    };

    await expect(connectWithRetry(open, RETRY_UNTIL_UP, { sleep })).resolves.toBe("connected");
    // 1s doubling to the 30s ceiling, then flat.
    expect(slept).toEqual([1000, 2000, 4000, 8000, 16000, 30000, 30000, 30000, 30000, 30000, 30000]);
  });

  it("never gives up when maxRetries is null", async () => {
    const { sleep } = recordingSleep();
    let attempts = 0;
    // Far past any bounded policy's budget — RETRY_UNTIL_UP must still get there.
    const open = async (): Promise<string> => {
      attempts++;
      if (attempts < 250) throw new Error("connect ECONNREFUSED 172.18.0.3:3000");
      return "connected";
    };

    await expect(connectWithRetry(open, RETRY_UNTIL_UP, { sleep })).resolves.toBe("connected");
    expect(attempts).toBe(250);
  });

  it("gives up after maxRetries on a bounded policy", async () => {
    const { slept, sleep } = recordingSleep();
    const bounded: ConnectRetryPolicy = { maxRetries: 3, baseDelayMs: 10, maxDelayMs: 100 };
    let attempts = 0;
    const open = async (): Promise<string> => {
      attempts++;
      throw new Error("fetch failed");
    };

    await expect(connectWithRetry(open, bounded, { sleep })).rejects.toThrow("fetch failed");
    // 1 initial attempt + maxRetries.
    expect(attempts).toBe(4);
    expect(slept).toEqual([10, 20, 40]);
  });

  it("keeps the shipped mid-flight policy bounded", () => {
    // A tool call the workflow executor is awaiting must not hang forever.
    expect(CONNECT_RETRY.maxRetries).not.toBeNull();
    expect(RETRY_UNTIL_UP.maxRetries).toBeNull();
  });

  it("rethrows a fatal error immediately, without sleeping", async () => {
    const { slept, sleep } = recordingSleep();
    let attempts = 0;
    const open = async (): Promise<string> => {
      attempts++;
      throw new StreamableHTTPError(404, "not found");
    };

    await expect(connectWithRetry(open, RETRY_UNTIL_UP, { sleep })).rejects.toThrow("not found");
    expect(attempts).toBe(1);
    expect(slept).toEqual([]);
  });

  it("emits one onRetry per failed attempt so the droplet logs show the wait", async () => {
    const { sleep } = recordingSleep();
    const seen: number[] = [];
    let attempts = 0;
    const open = async (): Promise<string> => {
      attempts++;
      if (attempts < 3) throw alreadyConnected();
      return "connected";
    };

    await connectWithRetry(open, CONNECT_RETRY, {
      sleep,
      onRetry: ({ attempt }) => seen.push(attempt),
    });
    expect(seen).toEqual([1, 2]);
  });
});

// End-to-end over the real Client/Server handshake: `connectMcp` must come back
// with a usable handle after the first attempts fail, rather than throwing (the
// throw is what reached `main().catch` → exit(1) → Docker restart).
describe("connectMcp", () => {
  const handles: Array<{ close(): Promise<void> }> = [];

  function liveTransport(): Transport {
    const server = new McpServer({ name: "stub-mcp", version: "0.0.0" });
    server.registerTool(
      "ping",
      { title: "ping", description: "returns pong", inputSchema: { value: z.string().optional() } },
      async () => ({ content: [{ type: "text" as const, text: "pong" }] }),
    );
    const [clientSide, serverSide] = InMemoryTransport.createLinkedPair();
    void server.connect(serverSide);
    return clientSide;
  }

  // A transport whose start() rejects — the client-visible shape of both
  // "MCP container isn't listening yet" and "MCP answered 500".
  function deadTransport(err: Error): Transport {
    return {
      start: async () => {
        throw err;
      },
      send: async () => {},
      close: async () => {},
    };
  }

  function factoryFailingFirst(n: number, err: () => Error): TransportFactory {
    let calls = 0;
    return () => (calls++ < n ? deadTransport(err()) : liveTransport());
  }

  it("survives an MCP that 500s the first initializes and yields a working handle", async () => {
    const mcp = await connectMcp({
      startupRetry: { maxRetries: null, baseDelayMs: 0, maxDelayMs: 0 },
      createTransport: factoryFailingFirst(3, alreadyConnected),
    });
    handles.push(mcp);

    expect(mcp.tools.map((t) => t.function.name)).toEqual(["ping"]);
    await expect(mcp.callTool("ping", {})).resolves.toBe("pong");
    await mcp.close();
  });

  it("survives an MCP that is not listening yet", async () => {
    const mcp = await connectMcp({
      startupRetry: { maxRetries: null, baseDelayMs: 0, maxDelayMs: 0 },
      createTransport: factoryFailingFirst(5, () => new Error("fetch failed")),
    });
    handles.push(mcp);

    await expect(mcp.callTool("ping", {})).resolves.toBe("pong");
    await mcp.close();
  });

  it("still fails fast on a deterministic rejection", async () => {
    // A wrong MCP_URL must not be retried into invisibility — the supervisor
    // should die on it so the misconfiguration is obvious.
    await expect(
      connectMcp({
        startupRetry: { maxRetries: null, baseDelayMs: 0, maxDelayMs: 0 },
        createTransport: () => deadTransport(new StreamableHTTPError(404, "not found")),
      }),
    ).rejects.toThrow("not found");
  });
});
