import type { ChatCompletionTool } from "openai/resources/chat/completions";
import { z } from "zod";
import type { CodexClient } from "./codex-client";

// `code_agent` — delegate any task that needs real computation or code to
// Codex, which WRITES and RUNS code in its own sandbox and returns the result.
// It is the agent's "do the math / process the data correctly" capability: an
// LLM composing a number out of its head is unreliable, so anything that must
// be computed — exact arithmetic, counting, date math, parsing/aggregating a
// CSV or spreadsheet, transforming data — goes here instead.
//
// Like `set_memory` / `invoke_sub_agent`, this is an AGENT-SIDE synthetic tool:
// the agent already owns the Codex service connection (CODEX_URL), so there's no
// reason to round-trip through the MCP integration server. Both execution paths
// dispatch it through the one `runCodeAgent` below — the workflow executor as a
// `tool` step, the AgentLoop via the synthetic-tools registry.

export const CODE_AGENT_TOOL_NAME = "code_agent";

export const CODE_AGENT_TOOL: ChatCompletionTool = {
  type: "function",
  function: {
    name: CODE_AGENT_TOOL_NAME,
    description:
      "Delegate a computational / coding task to a sandboxed code agent (Codex). " +
      "It writes AND runs code (Python with pandas/openpyxl, or Node) and returns " +
      "ONLY the final result. Use it for anything that must be COMPUTED rather " +
      "than recalled or written by an LLM: exact arithmetic, counting, date/time " +
      "math, statistics, parsing or aggregating CSV/Excel/JSON data, string/data " +
      "transformations, regex extraction, unit conversions. Do NOT use it for " +
      "web access, reasoning, or composing prose — only code/computation. State " +
      "the task precisely and say exactly what the output should be (e.g. 'return " +
      "just the number'). Put any data the code needs in `data` — it is delivered " +
      "to the program on stdin (for a binary file like .xlsx, base64-encode it and " +
      "say so in the task).",
    parameters: {
      type: "object",
      properties: {
        task: {
          type: "string",
          description:
            "Precise description of the computation to perform and the exact " +
            "output expected. E.g. 'Sum the `total` column of the CSV on stdin; " +
            "return only the rounded integer.'",
        },
        data: {
          type: "string",
          description:
            "Optional input data delivered to the program on stdin (CSV/JSON/" +
            "numbers/text, or base64 for a binary file). Omit if the task is " +
            "self-contained.",
        },
      },
      required: ["task"],
    },
  },
};

export const CodeAgentArgsSchema = z.object({
  task: z.string().min(1),
  data: z.string().optional(),
});
export type CodeAgentArgs = z.infer<typeof CodeAgentArgsSchema>;

const SYSTEM_FRAMING =
  "You are a code-execution agent in a sandbox with Python (pandas, openpyxl, " +
  "numpy) and Node available. Write and run whatever code accomplishes the task, " +
  "then print ONLY the final result to stdout — no source code, no explanation, " +
  "no markdown fences. If input data is provided it is on stdin.";

// Single dispatch point shared by the workflow executor and the AgentLoop
// synthetic-tools registry. Resolves to the trimmed final result; throws on a
// Codex failure (the client rejects non-ok responses) — callers decide whether
// that becomes a tool_error (workflow) or an error string (AgentLoop).
export async function runCodeAgent(codex: CodexClient, args: CodeAgentArgs): Promise<string> {
  const result = await codex.run({
    prompt: `${SYSTEM_FRAMING}\n\nTask:\n${args.task}`,
    input: args.data,
    // Needs to write + execute scratch scripts; an ephemeral cwd keeps it
    // isolated. No network is required for pure computation.
    sandbox: "workspace-write",
    approvalPolicy: "never",
    timeoutMs: 120_000,
  });
  return result.content.trim();
}
