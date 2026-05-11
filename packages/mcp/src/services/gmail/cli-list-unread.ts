import "dotenv/config";
import { closeDb, getDb } from "../../db/client";
import { listMessages } from "./messages";

function readArg(name: string): string | undefined {
  const flag = `--${name}`;
  const idx = process.argv.indexOf(flag);
  if (idx === -1 || idx === process.argv.length - 1) return undefined;
  return process.argv[idx + 1];
}

async function main(): Promise<void> {
  let accountKey = readArg("account");
  if (!accountKey) {
    const row = getDb()
      .prepare(
        `SELECT account_key FROM integration_account
         WHERE provider = 'gmail'
         ORDER BY created_at DESC LIMIT 1`,
      )
      .get() as { account_key: string } | undefined;
    if (!row) {
      throw new Error("No Gmail account in DB. Run `pnpm gmail:auth` first.");
    }
    accountKey = row.account_key;
  }

  const query = readArg("query") ?? "is:unread";
  const limit = Number(readArg("limit") ?? "10");

  const { messages } = await listMessages(accountKey, { query, maxResults: limit });

  console.log(`\n${accountKey} — query=\`${query}\` — ${messages.length} match(es)\n`);
  for (const m of messages) {
    console.log(`• ${m.subject ?? "(no subject)"}`);
    console.log(`  from: ${m.from ?? "?"}`);
    if (m.date) console.log(`  date: ${m.date}`);
    if (m.snippet) console.log(`  ${m.snippet}`);
    console.log();
  }
}

main()
  .catch((err: unknown) => {
    console.error(err);
    process.exitCode = 1;
  })
  .finally(() => {
    closeDb();
  });
