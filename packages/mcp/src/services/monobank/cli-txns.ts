import "dotenv/config";
import { getStatement } from "./client";

function readArg(name: string): string | undefined {
  const flag = `--${name}`;
  const idx = process.argv.indexOf(flag);
  if (idx === -1 || idx === process.argv.length - 1) return undefined;
  return process.argv[idx + 1];
}

async function main(): Promise<void> {
  const accountId = readArg("account") ?? "0";
  const days = Number(readArg("days") ?? "7");

  if (!Number.isFinite(days) || days < 1 || days > 31) {
    throw new Error(`--days must be between 1 and 31 (got ${days})`);
  }

  const to = new Date();
  const from = new Date(to.getTime() - days * 86_400_000);

  const transactions = await getStatement(accountId, from, to);

  console.log(
    JSON.stringify(
      {
        accountId,
        from: from.toISOString(),
        to: to.toISOString(),
        days,
        count: transactions.length,
        transactions,
      },
      null,
      2,
    ),
  );
}

main().catch((err: unknown) => {
  console.error(err);
  process.exitCode = 1;
});