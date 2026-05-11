import "dotenv/config";
import { createInterface } from "node:readline/promises";
import { stdin, stdout } from "node:process";
import { closeDb } from "../../db/client";
import { createOAuth2Client, exchangeCodeAndPersist, getAuthUrl } from "./auth";

async function main(): Promise<void> {
  const client = createOAuth2Client();
  const url = getAuthUrl(client);

  console.log("\n1) Open this URL in a browser and grant access:\n");
  console.log(url);
  console.log(
    "\n2) After consent, Google will redirect to your GOOGLE_REDIRECT_URI with a `code` query param.",
  );
  console.log("   Copy that `code` value and paste it below.\n");

  const rl = createInterface({ input: stdin, output: stdout });
  const code = (await rl.question("code: ")).trim();
  rl.close();

  if (!code) throw new Error("No code provided");

  const { accountKey } = await exchangeCodeAndPersist(code);
  console.log(`\nGmail authorized for ${accountKey}`);
}

main()
  .catch((err: unknown) => {
    console.error("Gmail auth failed:", err);
    process.exitCode = 1;
  })
  .finally(() => {
    closeDb();
  });
