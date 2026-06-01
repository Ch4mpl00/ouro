import "dotenv/config";
import { defineConfig } from "drizzle-kit";

// drizzle-kit reads this when generating migrations from schema diffs
// (`pnpm db:generate:pg`). Runtime (apply migrations on boot) is in
// `src/db/pg/client.ts`. DATABASE_URL is only required for `push` /
// introspect; for `generate` the in-memory diff doesn't need a live DB.

export default defineConfig({
  dialect: "postgresql",
  schema: "./src/db/pg/schema.ts",
  out: "./src/db/pg/migrations",
  dbCredentials: {
    url: process.env.DATABASE_URL ?? "postgres://mcp:mcp@localhost:5432/mcp",
  },
  verbose: true,
  strict: true,
});
