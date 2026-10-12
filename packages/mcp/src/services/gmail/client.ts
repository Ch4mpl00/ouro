import { google, type gmail_v1 } from "googleapis";
import type { StateDb } from "../../db/state";
import { createOAuth2Client } from "./auth";
import { createGmailStore, type GmailStore } from "./storage";

// The Gmail domain's handles: its state, and an authorized API client per
// account. Built once in the composition root and passed to the tools, the
// poller and the CLIs.

export interface GmailModule {
  store: GmailStore;
  // GMAIL_ACCOUNT_KEY, else the most recently authorized account.
  resolveAccountKey(): Promise<string | null>;
  client(accountKey: string): Promise<gmail_v1.Gmail>;
}

export interface GmailModuleDeps {
  db: StateDb;
  // Where the Gmail API lives. Tests point it at a local fake; production
  // leaves it to googleapis' default (https://gmail.googleapis.com/).
  apiRootUrl?: string;
}

export function createGmailModule(deps: GmailModuleDeps): GmailModule {
  const store = createGmailStore(deps.db);

  return {
    store,

    resolveAccountKey: async () => process.env.GMAIL_ACCOUNT_KEY || (await store.latestAccountKey()),

    client: async (accountKey) => {
      const account = await store.account(accountKey);
      if (!account) {
        throw new Error(`No Gmail account "${accountKey}". Run \`pnpm gmail:auth\` to authorize.`);
      }
      if (!account.refresh_token) {
        throw new Error(`Gmail account "${accountKey}" has no refresh token. Re-authorize.`);
      }
      const auth = createOAuth2Client();
      auth.setCredentials({
        access_token: account.access_token ?? undefined,
        refresh_token: account.refresh_token,
        expiry_date: account.expires_at ? new Date(account.expires_at).getTime() : undefined,
      });
      // googleapis refreshes on its own; keep what it was handed.
      auth.on("tokens", (tokens) => {
        store.persistTokens(accountKey, tokens).catch((err) => {
          console.error(`[gmail] persisting refreshed tokens failed:`, err instanceof Error ? err.message : err);
        });
      });
      return google.gmail({ version: "v1", auth, ...(deps.apiRootUrl ? { rootUrl: deps.apiRootUrl } : {}) });
    },
  };
}
