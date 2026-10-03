import { defineConfig } from "vitest/config";

// `pnpm spec:model`: the memory server against its reference model
// (packages/mcp/model). Needs Docker for Postgres, or TEST_DATABASE_URL.
export default defineConfig({
  test: {
    include: ["packages/mcp/model/**/*.check.ts"],
    environment: "node",
    testTimeout: 30 * 60_000,
    hookTimeout: 180_000,
    // One server, one database: the two checks run one after the other.
    fileParallelism: false,
  },
});
