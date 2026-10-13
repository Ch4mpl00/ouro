import { createCodexServer } from "./server";

// Entry point of the codex service (`pnpm codex:serve`).
const port = Number(process.env.CODEX_SERVICE_PORT ?? "3010");
const maxBodyBytes = Number(process.env.CODEX_SERVICE_MAX_BODY_BYTES ?? String(10 * 1024 * 1024));

createCodexServer({ maxBodyBytes }).listen(port, "0.0.0.0", () => {
  console.log(`[codex] service listening on :${port}`);
});
