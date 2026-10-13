// A stand-in for the Gmail API on a loopback port, for the googleapis client
// pointed at it with `createGmailModule({ apiRootUrl })`. (googleapis'
// gaxios uses node-fetch, so a global-fetch interceptor can't stand in.)
//
// Behaves like Gmail where the poller cares:
//   - messages.list answers the mails matching the query — a mail says
//     whether the subscription query matches it — honours `after:<seconds>`
//     non-strictly, newest first, `maxResults` per page with a
//     nextPageToken;
//   - messages.get answers metadata headers or the full payload, with one
//     part per attachment;
//   - `failGet(id, times)` makes the next `times` gets of a mail fail with a
//     500.

import http from "node:http";

export interface FakeMail {
  id: string;
  internalDate: number;
  subject: string;
  matches: boolean;
  attachments: { attachmentId: string; filename: string; mimeType: string; size: number }[];
}

export interface FakeGmail {
  rootUrl: string;
  mails: FakeMail[];
  // Every list call's query, in order.
  queries: string[];
  failGet(id: string, times?: number): void;
  close(): Promise<void>;
}

export async function startFakeGmail(): Promise<FakeGmail> {
  const mails: FakeMail[] = [];
  const queries: string[] = [];
  const failures = new Map<string, number>();

  const server = http.createServer((req, res) => {
    const url = new URL(req.url ?? "/", "http://fake");
    const json = (status: number, body: unknown) => {
      res.writeHead(status, { "content-type": "application/json" }).end(JSON.stringify(body));
    };
    const list = /^\/gmail\/v1\/users\/me\/messages$/.exec(url.pathname);
    if (list && req.method === "GET") {
      const q = url.searchParams.get("q") ?? "";
      queries.push(q);
      const after = /after:(\d+)/.exec(q);
      const afterMs = after ? Number(after[1]) * 1000 : -Infinity;
      const max = Number(url.searchParams.get("maxResults") ?? 100);
      const offset = Number(url.searchParams.get("pageToken") ?? 0);
      const hits = mails.filter((m) => m.matches && m.internalDate >= afterMs).sort((a, b) => b.internalDate - a.internalDate);
      const page = hits.slice(offset, offset + max);
      return json(200, {
        messages: page.map((m) => ({ id: m.id, threadId: `t-${m.id}` })),
        ...(offset + max < hits.length ? { nextPageToken: String(offset + max) } : {}),
        resultSizeEstimate: hits.length,
      });
    }
    const get = /^\/gmail\/v1\/users\/me\/messages\/([^/]+)$/.exec(url.pathname);
    if (get && req.method === "GET") {
      const id = decodeURIComponent(get[1] ?? "");
      const mail = mails.find((m) => m.id === id);
      if (!mail) return json(404, { error: { code: 404, message: "Requested entity was not found." } });
      const left = failures.get(id) ?? 0;
      if (left > 0) {
        failures.set(id, left - 1);
        return json(500, { error: { code: 500, message: "Backend Error" } });
      }
      return json(200, {
        id: mail.id,
        threadId: `t-${mail.id}`,
        snippet: `snippet of ${mail.subject}`,
        internalDate: String(mail.internalDate),
        labelIds: ["INBOX"],
        payload: {
          mimeType: "multipart/mixed",
          headers: [
            { name: "From", value: "NashDom <bills@nashdom.example>" },
            { name: "To", value: "me@example.com" },
            { name: "Subject", value: mail.subject },
            { name: "Date", value: new Date(mail.internalDate).toUTCString() },
          ],
          parts: [
            { partId: "0", mimeType: "text/plain", filename: "", body: { size: 5, data: "aGVsbG8" } },
            ...mail.attachments.map((a, i) => ({
              partId: String(i + 1),
              mimeType: a.mimeType,
              filename: a.filename,
              body: { attachmentId: a.attachmentId, size: a.size },
            })),
          ],
        },
      });
    }
    json(404, { error: { code: 404, message: `no route ${req.method} ${url.pathname}` } });
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  if (address === null || typeof address === "string") throw new Error("fake Gmail: no TCP address");
  return {
    rootUrl: `http://127.0.0.1:${address.port}/`,
    mails,
    queries,
    failGet: (id, times = 1) => void failures.set(id, (failures.get(id) ?? 0) + times),
    close: () => new Promise((resolve) => server.close(() => resolve())),
  };
}
