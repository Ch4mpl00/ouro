import { z } from "zod";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { getStatement, STATEMENT_PAGE_LIMIT } from "../services/monobank";
import { jsonResult } from "../result";

export function registerMonobankTools(server: McpServer): void {
  server.registerTool(
    "list_monobank_transactions",
    {
      title: "List Monobank transactions",
      description:
        "Fetch recent transactions for a Monobank account. accountId can be a specific " +
        "account.id or '0' for the default UAH account. days is the lookback window " +
        "(default 7, max 31). Rate limit: 1 request per 60s per account — surface 429s " +
        "rather than retrying. Monobank returns at most 500 transactions, newest first: " +
        "`complete: false` means older ones in the window were left out (the list stops at " +
        "`oldestReturned`), so don't total the window from it.",
      inputSchema: {
        accountId: z
          .string()
          .optional()
          .describe("Monobank account id, or '0' for default UAH. Defaults to '0'."),
        days: z
          .number()
          .int()
          .min(1)
          .max(31)
          .optional()
          .describe("Lookback window in days (default 7, max 31)."),
      },
    },
    async ({ accountId, days }) => {
      const account = accountId ?? "0";
      const window = days ?? 7;
      const to = new Date();
      const from = new Date(to.getTime() - window * 24 * 60 * 60 * 1000);
      const transactions = await getStatement(account, from, to);
      // A full page means Monobank cut the list: there may be older ones.
      const complete = transactions.length < STATEMENT_PAGE_LIMIT;
      return jsonResult({
        accountId: account,
        from: from.toISOString(),
        to: to.toISOString(),
        days: window,
        count: transactions.length,
        complete,
        ...(complete ? {} : { oldestReturned: transactions[transactions.length - 1]?.time ?? null }),
        transactions,
      });
    },
  );
}
