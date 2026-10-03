import "dotenv/config";
import { databaseUrl } from "../../db/pg/client";
import { connectStateDb, type StateDb } from "../../db/state";
import { createGmailModule } from "./client";
import { listMessages } from "./messages";

function readArg(name: string): string | undefined {
  const flag = `--${name}`;
  const idx = process.argv.indexOf(flag);
  if (idx === -1 || idx === process.argv.length - 1) return undefined;
  return process.argv[idx + 1];
}

async function main(db: StateDb): Promise<void> {
  const gmail = createGmailModule({ db });
  const accountKey = readArg("account") ?? (await gmail.resolveAccountKey());
  if (!accountKey) {
    throw new Error("No Gmail account in DB. Run `pnpm gmail:auth` first.");
  }

  const query = readArg("query") ?? "is:unread";
  const limit = Number(readArg("limit") ?? "10");

  const { messages } = await listMessages(await gmail.client(accountKey), { query, maxResults: limit });

  console.log(`\n${accountKey} — query=\`${query}\` — ${messages.length} match(es)\n`);
  for (const m of messages) {
    console.log(`• ${m.subject ?? "(no subject)"}`);
    console.log(`  from: ${m.from ?? "?"}`);
    if (m.date) console.log(`  date: ${m.date}`);
    if (m.snippet) console.log(`  ${m.snippet}`);
    console.log();
  }
}

const db = await connectStateDb(databaseUrl());
main(db)
  .catch((err: unknown) => {
    console.error(err);
    process.exitCode = 1;
  })
  .finally(() => db.close());
