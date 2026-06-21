export interface CodexRunRequest {
  prompt: string;
  input?: unknown;
  schema?: unknown;
  sandbox?: "read-only" | "workspace-write" | "danger-full-access";
  approvalPolicy?: "untrusted" | "on-request" | "never";
  cwd?: string;
  model?: string;
  timeoutMs?: number;
  includeEvents?: boolean;
  config?: Record<string, unknown>;
}

export interface CodexRunResult {
  ok: true;
  content: string;
  parsed?: unknown;
  usage?: unknown;
  threadId?: string;
  events?: unknown[];
  stderr: string;
}

export interface CodexClient {
  run(req: CodexRunRequest): Promise<CodexRunResult>;
}

export function createCodexClient(baseUrl = process.env.CODEX_URL ?? "http://localhost:3010"): CodexClient {
  const root = baseUrl.replace(/\/+$/, "");
  return {
    async run(req) {
      const res = await fetch(`${root}/run`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(req),
      });
      const body = await res.json() as { ok?: boolean; error?: string } & Partial<CodexRunResult>;
      if (!res.ok || body.ok !== true) {
        throw new Error(body.error ?? `codex service returned ${res.status}`);
      }
      return body as CodexRunResult;
    },
  };
}
