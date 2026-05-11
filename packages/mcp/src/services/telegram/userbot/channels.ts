// Normalize various forms of channel references the LLM might pass to
// gramjs. Accepts: "tginsider", "@tginsider", "https://t.me/tginsider/".

export function normalizeHandle(handle: string): string {
  let h = handle.trim();
  h = h.replace(/^https?:\/\/t\.me\//, "");
  h = h.replace(/^@+/, "");
  h = h.replace(/\/+$/, "");
  return h;
}
