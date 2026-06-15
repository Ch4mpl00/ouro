import { defineConfig } from "drizzle-kit";

// drizzle-kit reads this when generating migrations from schema diffs
// (`pnpm db:generate:agent`). Runtime (apply migrations on boot) lives in
// `src/db/client.ts`. sqlite `generate` diffs the schema against the existing
// migration history in-memory — it needs no live DB.

export default defineConfig({
  dialect: "sqlite",
  schema: "./src/db/schema.ts",
  out: "./src/db/migrations",
  dbCredentials: {
    url: "file:./data/agent.db",
  },
  verbose: true,
  strict: true,
});
