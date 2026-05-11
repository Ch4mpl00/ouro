// Helper for MCP tool handlers: wrap an arbitrary value as a single text-content
// result containing pretty JSON. Simpler and broadly compatible across clients
// that don't yet handle structured content.
export function jsonResult(value: unknown): {
  content: Array<{ type: "text"; text: string }>;
} {
  return {
    content: [{ type: "text", text: JSON.stringify(value, null, 2) }],
  };
}
