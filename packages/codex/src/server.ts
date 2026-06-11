import "dotenv/config";
import http from "node:http";
import { runCodex, type CodexRunRequest } from "./runner";

const port = Number(process.env.CODEX_SERVICE_PORT ?? "3010");
const maxBodyBytes = Number(process.env.CODEX_SERVICE_MAX_BODY_BYTES ?? String(10 * 1024 * 1024));

function readBody(req: http.IncomingMessage): Promise<string> {
  return new Promise((resolve, reject) => {
    let size = 0;
    let body = "";
    req.setEncoding("utf8");
    req.on("data", (chunk: string) => {
      size += Buffer.byteLength(chunk);
      if (size > maxBodyBytes) {
        reject(new Error(`request body exceeds ${maxBodyBytes} bytes`));
        req.destroy();
        return;
      }
      body += chunk;
    });
    req.on("end", () => resolve(body));
    req.on("error", reject);
  });
}

function send(res: http.ServerResponse, status: number, body: unknown): void {
  const text = JSON.stringify(body);
  res.writeHead(status, {
    "content-type": "application/json; charset=utf-8",
    "content-length": Buffer.byteLength(text),
  });
  res.end(text);
}

function validateRunRequest(input: unknown): CodexRunRequest {
  if (!input || typeof input !== "object") throw new Error("body must be an object");
  const obj = input as Record<string, unknown>;
  if (typeof obj.prompt !== "string" || obj.prompt.length === 0) {
    throw new Error("prompt must be a non-empty string");
  }
  return obj as unknown as CodexRunRequest;
}

const server = http.createServer(async (req, res) => {
  try {
    if (req.method === "GET" && req.url === "/health") {
      send(res, 200, { ok: true });
      return;
    }

    if (req.method !== "POST" || req.url !== "/run") {
      send(res, 404, { ok: false, error: "not found" });
      return;
    }

    const raw = await readBody(req);
    const parsed = raw ? JSON.parse(raw) : {};
    const runReq = validateRunRequest(parsed);
    const result = await runCodex(runReq);
    send(res, result.ok ? 200 : 502, result);
  } catch (err) {
    send(res, 400, { ok: false, error: err instanceof Error ? err.message : String(err) });
  }
});

server.listen(port, "0.0.0.0", () => {
  console.log(`[codex] service listening on :${port}`);
});
