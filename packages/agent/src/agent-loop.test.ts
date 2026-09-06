import type { ChatCompletionMessage, ChatCompletionMessageParam, ChatCompletionTool } from "openai/resources/chat/completions";
import { describe, expect, it, vi } from "vitest";
import { createEngine } from "./engine";
import { DEFAULT_PRESETS } from "./models";
import type { CompletionParams, ChatProvider } from "./providers";
import { createSessionContext } from "./session-context";
import { nullTracer, type Tracer } from "./tracing";
import { createLocalRecorderTracer } from "./tracing/local-recorder";
import { teeTracer } from "./tracing/tee";
import type { StoredTraceInput, TraceStore } from "./db/trace-store";
import { createSupervisorModule } from "./supervisor/module";
import type { WorkflowRunner, WorkflowRunResult } from "./workflow";
import type { Step } from "./workflow/dsl";
import { createStore } from "./workflow/variables";
import { createSkillStore } from "./skills";
import { createLangfuseTracer } from "./tracing/langfuse";
import { LangfuseOtelSpanAttributes } from "@langfuse/tracing";
import type { ReadableSpan } from "@opentelemetry/sdk-trace-node";
import { trace as otelTrace, context as otelContext, propagation } from "@opentelemetry/api";

// Exercise the actual Langfuse adapter + OTel SDK. Only the network exporter
// is replaced, so tests cannot send telemetry to a live project.
const exported = vi.hoisted(() => new Array<ReadableSpan>());
vi.mock("@langfuse/otel", () => ({
  LangfuseSpanProcessor: vi.fn(function () {
    return {
      onStart() {},
      onEnd(span: ReadableSpan) { exported.push(span); },
      async forceFlush() {},
      async shutdown() {},
    };
  }),
}));

const searchTool: ChatCompletionTool = {
  type: "function",
  function: { name: "search_news", parameters: { type: "object", properties: { query: { type: "string" } }, required: ["query"] } },
};

function answer(content: string): ChatCompletionMessage {
  return { role: "assistant", content, refusal: null };
}

function call(name: string, args: Record<string, unknown> = {}, id = "call_1"): ChatCompletionMessage {
  return {
    role: "assistant", content: null, refusal: null,
    tool_calls: [{ id, type: "function", function: { name, arguments: JSON.stringify(args) } }],
  };
}

function lastResult(messages: ChatCompletionMessageParam[]): string {
  const last = messages.at(-1);
  if (last?.role !== "tool" || typeof last.content !== "string") throw new Error("Expected a tool reply");
  return last.content;
}

function context(id = "task") {
  return createSessionContext({
    id,
    env: { now: new Date("2026-09-06T12:00:00Z"), timezone: "UTC", userEmail: null, newsLastReadAt: null },
  });
}

type Turn = (params: CompletionParams) => ChatCompletionMessage;

function harness(turns: Turn[], toolResult = "found", tracer: Tracer = nullTracer) {
  const requests: ChatCompletionMessageParam[][] = [];
  const provider: ChatProvider = {
    kind: "openai",
    async complete(params) {
      requests.push(structuredClone(params.messages));
      const turn = turns.shift();
      if (!turn) throw new Error("Unexpected LLM call");
      return { message: turn(params), finishReason: "stop", usage: { input: 10, output: 5, total: 15, cached: 2 } };
    },
  };
  const callTool = vi.fn(async () => toolResult);
  const persist = vi.fn();
  const engine = createEngine({
    providers: { openai: provider, gemini: provider, deepseek: provider },
    mcp: { tools: [searchTool], callTool, close: async () => {} },
    presets: DEFAULT_PRESETS,
    tracer,
    memory: { get: () => null, set: persist },
    codex: { run: async () => { throw new Error("Unexpected code_agent call"); } },
    skillStore: {
      readSkill: async () => ({ body: "test skill", tools: "*", source: "default" }),
      readSkillRaw: async () => "test skill",
      readPatch: async () => null,
      saveSkill: async () => ({ path: "test.md", sizeBytes: 0 }),
      savePatch: async () => ({ path: "test.patch.md", sizeBytes: 0 }),
      deletePatch: async () => false,
      listSkills: async () => [],
      validateAll: async () => {},
    },
  });
  engine.log = vi.fn();
  const sessionContext = context();
  return {
    engine, sessionContext, callTool, persist, requests,
    start: () => engine.startAgentLoop({ id: "parent", sessionContext, skills: ["test"] }),
  };
}

describe("AgentLoop working memory", () => {
  it("saves small MCP and synthetic outputs without changing MCP schemas or arguments", async () => {
    const h = harness([
      ({ tools }) => {
        expect(tools?.find((t) => t.function.name === "search_news")).toEqual(searchTool);
        return call("search_news", { query: "today" });
      },
      ({ messages }) => {
        const reply = JSON.parse(lastResult(messages));
        expect(reply).toMatchObject({ content: "found", truncated: false });
        expect(h.sessionContext.memory.get(reply.memory_key)).toBe("found");
        return call("read_skill", { name: "test" });
      },
      ({ messages }) => {
        const reply = JSON.parse(lastResult(messages));
        expect(JSON.parse(reply.content).content).toBe("test skill");
        expect(reply.format).toBe("json");
        expect(h.sessionContext.memory.get(reply.memory_key)).toBe(reply.content);
        return answer("done");
      },
    ]);
    expect(await (await h.start()).send("search")).toBe("done");
    expect(h.callTool).toHaveBeenCalledWith("search_news", { query: "today" });
    expect(h.sessionContext.memory.list()).toHaveLength(2);
  });

  it("keeps large results out of the next LLM turn and allows an explicit full read", async () => {
    const payload = "news ".repeat(2_000) + "secret tail";
    const h = harness([
      () => call("search_news", { query: "today" }),
      ({ messages }) => {
        const reply = JSON.parse(lastResult(messages));
        expect(reply.truncated).toBe(true);
        expect(JSON.stringify(messages)).not.toContain("secret tail");
        expect(h.sessionContext.memory.get(reply.memory_key)).toBe(payload);
        return call("working_memory_get", { key: reply.memory_key });
      },
      ({ messages }) => {
        expect(lastResult(messages)).toBe(payload);
        return answer("read");
      },
    ], payload);
    await (await h.start()).send("search");
    expect(h.sessionContext.memory.list()).toHaveLength(1);
  });

  it("supports put/get/list/delete without creating recursive memory entries", async () => {
    const h = harness([
      () => call("working_memory_put", { key: "mem.news", value: "", format: "json" }),
      ({ messages }) => {
        expect(JSON.parse(lastResult(messages))).toEqual({ memory_key: "mem.news", format: "json", size_bytes: 0 });
        return call("working_memory_put", { key: "mem.news", value: "overwrite" });
      },
      ({ messages }) => {
        expect(lastResult(messages)).toContain('Key "mem.news" already exists');
        return call("working_memory_get", { key: "mem.news" });
      },
      ({ messages }) => {
        expect(lastResult(messages)).toBe("");
        return call("working_memory_list");
      },
      ({ messages }) => {
        expect(JSON.parse(lastResult(messages))).toEqual([{ key: "mem.news", format: "json", sizeBytes: 0 }]);
        return call("working_memory_delete", { key: "mem.news" });
      },
      ({ messages }) => {
        expect(JSON.parse(lastResult(messages)).deleted).toBe(true);
        return call("working_memory_delete", { key: "mem.news" });
      },
      ({ messages }) => {
        expect(JSON.parse(lastResult(messages)).deleted).toBe(false);
        return call("working_memory_get", { key: "mem.news" });
      },
      ({ messages }) => {
        expect(lastResult(messages)).toContain('Key "mem.news" not found');
        return answer("done");
      },
    ]);
    await (await h.start()).send("use memory");
    expect(h.sessionContext.memory.list()).toEqual([]);
    expect(h.callTool).not.toHaveBeenCalled();
    expect(h.persist).not.toHaveBeenCalled();
  });

  it.each([
    { key: "", value: "x" },
    { key: "x", value: 42 },
    { key: "x", value: "x", format: "yaml" },
  ])("validates memory tool arguments before writing: %j", async (args) => {
    const h = harness([
      () => call("working_memory_put", args),
      ({ messages }) => {
        expect(lastResult(messages)).toContain("[working_memory_put error]");
        return answer("recovered");
      },
    ]);
    await (await h.start()).send("store");
    expect(h.sessionContext.memory.list()).toEqual([]);
  });

  it("passes references directly to a child and shares memory without copying parent history", async () => {
    const payload = "source ".repeat(2_000) + "source tail";
    const h = harness([
      () => call("search_news"),
      ({ messages }) => call("invoke_sub_agent", {
        skills: ["test"], prompt: "summarize", input_refs: [JSON.parse(lastResult(messages)).memory_key],
      }),
      ({ messages, tools }) => {
        expect(JSON.stringify(messages)).toContain("source tail");
        expect(JSON.stringify(messages)).not.toContain("parent-only instruction");
        expect(tools?.some((t) => t.function.name === "invoke_sub_agent")).toBe(false);
        return call("working_memory_put", { key: "summary", value: "short summary" });
      },
      () => answer("summary saved"),
      ({ messages }) => {
        expect(JSON.stringify(messages)).not.toContain("source tail");
        expect(JSON.parse(lastResult(messages)).content).toBe("summary saved");
        expect(h.sessionContext.memory.get("summary")).toBe("short summary");
        return answer("done");
      },
    ], payload);
    const starts = vi.spyOn(h.engine, "startAgentLoop");
    await (await h.start()).send("parent-only instruction");
    expect(starts).toHaveBeenCalledTimes(2);
    expect(starts.mock.calls[1]?.[0].sessionContext).toBe(h.sessionContext);
    expect(h.sessionContext.memory.list()).toHaveLength(3);
  });

  it("reports a missing input ref without starting a child", async () => {
    const h = harness([
      () => call("invoke_sub_agent", { skills: ["test"], prompt: "summarize", input_refs: ["missing"] }),
      ({ messages }) => {
        expect(JSON.parse(lastResult(messages)).content).toContain('Key "missing" not found');
        return answer("recovered");
      },
    ]);
    const starts = vi.spyOn(h.engine, "startAgentLoop");
    await (await h.start()).send("delegate");
    expect(starts).toHaveBeenCalledTimes(1);
  });

  it("stores a large sub-agent answer without adding it to the parent's context", async () => {
    const payload = "child answer ".repeat(1_000) + "child tail";
    const h = harness([
      () => call("invoke_sub_agent", { skills: ["test"], prompt: "report" }),
      () => answer(payload),
      ({ messages }) => {
        const reply = JSON.parse(lastResult(messages));
        expect(reply.truncated).toBe(true);
        expect(JSON.stringify(messages)).not.toContain("child tail");
        expect(h.sessionContext.memory.get(reply.memory_key)).toBe(payload);
        return answer("done");
      },
    ]);
    await (await h.start()).send("delegate");
  });

  it.each(["null", "[]", "{broken"])("returns invalid arguments as a recoverable tool reply: %s", async (raw) => {
    const h = harness([
      () => ({
        ...answer(""),
        tool_calls: [{ id: "bad_call", type: "function", function: { name: "search_news", arguments: raw } }],
      }),
      ({ messages }) => {
        expect(JSON.parse(lastResult(messages)).content).toContain("[tool error] arguments must be a JSON object");
        return answer("recovered");
      },
    ]);
    await (await h.start()).send("search");
    expect(h.callTool).not.toHaveBeenCalled();
  });

  it("keeps independent tasks isolated even on the same engine", async () => {
    const h = harness([]);
    h.sessionContext.memory.put("private", "first task");
    const secondContext = context("second");
    const first = await h.start();
    const second = await h.engine.startAgentLoop({ id: "second-loop", sessionContext: secondContext });
    expect(first.sessionContext).toBe(h.sessionContext);
    expect(second.sessionContext).toBe(secondContext);
    expect(second.sessionContext.memory.list()).toEqual([]);
  });

  it("keeps persistent set_memory writes separate from temporary result storage", async () => {
    const h = harness([
      () => call("set_memory", { key: "watermark", value: "today" }),
      () => answer("done"),
    ]);
    await (await h.start()).send("save");
    expect(h.persist).toHaveBeenCalledWith("watermark", "today");
    expect(() => h.sessionContext.memory.get("watermark")).toThrow("not found");
    expect(h.sessionContext.memory.list()).toHaveLength(1);
  });

  it("stores parallel calls under distinct keys and preserves message order", async () => {
    const first = call("search_news", { query: "first" });
    const second = call("search_news", { query: "second" });
    const h = harness([
      () => ({ ...first, tool_calls: [...first.tool_calls ?? [], ...second.tool_calls ?? []] }),
      ({ messages }) => {
        const replies = messages.filter((m) => m.role === "tool").map((m) => JSON.parse(String(m.content)));
        expect(replies.map((r) => r.content)).toEqual(["first result", "second result"]);
        expect(replies[0].memory_key).not.toBe(replies[1].memory_key);
        return answer("done");
      },
    ]);
    let resolveFirst: (value: string) => void = () => { throw new Error("First call has not started"); };
    h.callTool.mockImplementationOnce(() => new Promise<string>((resolve) => { resolveFirst = resolve; }));
    h.callTool.mockImplementationOnce(async () => { resolveFirst("first result"); return "second result"; });
    await (await h.start()).send("parallel");
    expect(h.sessionContext.memory.list()).toHaveLength(2);
  });

  it("returns parallel successes and failures to the model for its next decision", async () => {
    const first = call("search_news", { query: "first" }, "first");
    const second = call("search_news", { query: "second" }, "second");
    const h = harness([
      () => ({ ...first, tool_calls: [...first.tool_calls ?? [], ...second.tool_calls ?? []] }),
      ({ messages }) => {
        const replies = messages.filter((m) => m.role === "tool").map((m) => JSON.parse(String(m.content)));
        expect(replies[0].content).toBe("completed result");
        expect(replies[1].content).toContain("connection lost");
        return answer("handled failure");
      },
    ]);
    h.callTool.mockResolvedValueOnce("completed result");
    h.callTool.mockRejectedValueOnce(new Error("connection lost"));
    expect(await (await h.start()).send("parallel")).toBe("handled failure");
    const entries = h.sessionContext.memory.list();
    expect(entries).toHaveLength(2);
    expect(h.sessionContext.memory.get(entries[0]!.key)).toBe("completed result");
  });

  it("supports a general worker without a domain skill", async () => {
    const h = harness([
      () => call("invoke_sub_agent", { prompt: "extract the answer" }),
      () => answer("42"),
      ({ messages }) => {
        expect(JSON.parse(lastResult(messages)).content).toBe("42");
        return answer("done");
      },
    ]);
    const starts = vi.spyOn(h.engine, "startAgentLoop");
    await (await h.start()).send("extract");
    expect(starts.mock.calls[1]?.[0].skills).toEqual(["worker"]);
  });
});

function recording() {
  const written: StoredTraceInput[] = [];
  const store: TraceStore = {
    writeTrace: (trace) => { written.push(trace); }, getTrace: () => null,
    listRecent: () => [], writeJudgement: () => {}, listJudgements: () => [], listJudgedSkills: () => [],
  };
  return { tracer: createLocalRecorderTracer(store), written };
}

describe("primary AgentLoop and tracing", () => {
  const signal = { id: 1, source: "telegram", content: "chatId=42: summarize", envContext: "chatId=42", created_at: "2026-09-06T12:00:00Z" };

  // Routing is by source, not by content: scheduler compiles a workflow,
  // everything else runs the AgentLoop and must never reach the runner.
  const neverWorkflow: WorkflowRunner = {
    runForSignal: () => { throw new Error("Workflow must not run for this source"); },
  };

  function supervisor(h: ReturnType<typeof harness>, workflow: WorkflowRunner = neverWorkflow) {
    return createSupervisorModule({
      engine: h.engine,
      env: { mcp: { callTool: async () => '{"timezone":"UTC"}' }, memory: h.engine.memory, userEmail: null },
      workflow,
    });
  }

  it("loads Telegram history before the first generation and shares its reference with workers", async () => {
    const { tracer, written } = recording();
    const history = { messages: [
      { id: 1, chat_id: 42, thread_id: null, role: "assistant", text: "Собрать сводку за неделю?", created_at: "2026-09-06 11:59:00" },
    ] };
    const h = harness([
      ({ messages }) => {
        expect(h.callTool).toHaveBeenCalledExactlyOnceWith("get_telegram_chat_history", { chatId: "42", limit: 20 });
        expect(JSON.stringify(messages)).toContain("Собрать сводку за неделю?");
        return call("invoke_sub_agent", { prompt: "summarize", input_refs: ["telegram.history"] });
      },
      ({ messages }) => {
        expect(JSON.stringify(messages)).toContain("Собрать сводку за неделю?");
        return answer("summary");
      },
      () => answer("done"),
    ], JSON.stringify(history), tracer);
    const runner = supervisor(h);
    const content = 'Telegram message in chat 42.\nText: "давай"';
    expect(await runner.runSignal({ ...signal, content })).toBe("done");
    expect(h.callTool).toHaveBeenCalledTimes(1); // no worker fetch or extra LLM retrieval turn
    const trace = written[0]!;
    expect(trace.input).toBe(content);
    expect(trace.observations.find((o) => o.name === "get_telegram_chat_history")).toMatchObject({
      type: "TOOL", metadata: { automatic: true, memory_key: "telegram.history" },
    });
    expect(trace.observations.filter((o) => o.type === "GENERATION")).toHaveLength(3);
  });

  it("compiles scheduler signals into a workflow instead of running the primary loop", async () => {
    const { tracer, written } = recording();
    const h = harness([], "unused", tracer);
    const runForSignal = vi.fn(async (): Promise<WorkflowRunResult> => (
      { ok: true, attempts: 1, stepCount: 3, store: createStore({}) }
    ));
    const starts = vi.spyOn(h.engine, "startAgentLoop");
    const output = await supervisor(h, { runForSignal }).runSignal({ ...signal, source: "scheduler" });
    expect(JSON.parse(output)).toEqual({ ok: true, attempts: 1, stepCount: 3 });
    expect(runForSignal).toHaveBeenCalledOnce();
    expect(starts).not.toHaveBeenCalled();
    expect(h.callTool).not.toHaveBeenCalled(); // no Telegram preload on this path
    expect(written[0]).toMatchObject({ tags: ["scheduler", "planner-mode"], output: { ok: true } });
  });

  it("degrades to the primary loop in the same trace when a scheduler workflow does not compile", async () => {
    const { tracer, written } = recording();
    const h = harness([({ messages }) => {
      expect(messages.at(-1)?.content).toBe(signal.content); // no Telegram preload
      return answer("done");
    }], "unused", tracer);
    const workflow: WorkflowRunner = {
      runForSignal: async () => ({ ok: false, stage: "compile", reason: "schema_invalid", errors: ["step 0: unknown tool"], attempts: 3 }),
    };
    const starts = vi.spyOn(h.engine, "startAgentLoop");
    expect(await supervisor(h, workflow).runSignal({ ...signal, source: "scheduler" })).toBe("done");
    expect(starts.mock.calls[0]?.[0]).toMatchObject({ preset: "base", skills: ["scheduler"] });
    expect(written).toHaveLength(1);
    expect(written[0]?.observations.find((o) => o.name === "fallback")).toMatchObject({
      type: "EVENT", level: "WARNING", metadata: { stage: "compile", reason: "schema_invalid" },
    });
  });

  it("reports a scheduler execution failure instead of restarting the task agentically", async () => {
    const { tracer, written } = recording();
    const h = harness([() => answer("recovery report")], "unused", tracer);
    const step: Step = { kind: "tool", tool: "send_telegram_message", args: {} };
    const workflow: WorkflowRunner = {
      runForSignal: async () => ({ ok: false, stage: "execute", reason: "tool_error", error: new Error("telegram 400"), stepIndex: 2, step }),
    };
    const starts = vi.spyOn(h.engine, "startAgentLoop");
    await expect(supervisor(h, workflow).runSignal({ ...signal, source: "scheduler" })).rejects.toThrow("telegram 400");
    expect(starts.mock.calls.map(([opts]) => opts.skills)).toEqual([["recovery"]]);
    expect(written[0]?.observations.find((o) => o.name === "fallback")).toMatchObject({
      type: "EVENT", level: "ERROR", metadata: { stage: "execute", reason: "tool_error", step_index: 2 },
    });
    expect(written[0]?.observations.find((o) => o.name === "recovery")).toMatchObject({ output: "recovery report" });
  });

  it("runs a signal directly and records parent/worker/tool IO, memory references and per-iteration usage", async () => {
    const primary = recording();
    const mirror = recording();
    const payload = "article ".repeat(2_000) + "source tail";
    const h = harness([
      () => call("search_news", { query: "latest" }),
      ({ messages }) => call("invoke_sub_agent", { skills: ["news-query"], prompt: "summarize", input_refs: [JSON.parse(lastResult(messages)).memory_key] }),
      () => answer("short summary"),
      () => answer("finished"),
    ], payload, teeTracer(primary.tracer, mirror.tracer));
    const runner = supervisor(h);
    const starts = vi.spyOn(h.engine, "startAgentLoop");
    expect(await runner.runSignal(signal)).toBe("finished");
    expect(starts.mock.calls[0]?.[0]).toMatchObject({ preset: "smart", skills: ["telegram"] });
    expect(starts.mock.calls[1]?.[0].sessionContext).toBe(starts.mock.calls[0]?.[0].sessionContext);
    expect(primary.written).toHaveLength(1);
    expect(mirror.written).toHaveLength(1);
    const trace = mirror.written[0]!;
    expect(trace).toMatchObject({ input: signal.content, output: "finished", sessionId: "telegram:1", tags: ["telegram", "agent-loop"] });
    const agent = trace.observations.find((o) => o.name === "agent_loop")!;
    const worker = trace.observations.find((o) => o.name === "invoke_sub_agent")!;
    const search = trace.observations.find((o) => o.name === "search_news")!;
    const generations = trace.observations.filter((o) => o.type === "GENERATION");
    expect(worker).toMatchObject({ type: "AGENT", parentObservationId: agent.id, output: "short summary", metadata: { skill: "news-query" } });
    expect(String(worker.input)).toContain("source tail");
    expect(search).toMatchObject({ type: "TOOL", output: payload, metadata: { result_truncated: true, result_size_bytes: Buffer.byteLength(payload), memory_key: expect.any(String) } });
    expect(generations).toHaveLength(4);
    expect(generations.filter((o) => o.parentObservationId === worker.id)).toHaveLength(1);
    for (const generation of generations) expect(generation.usageDetails).toEqual({ input: 10, output: 5, total: 15, cached: 2 });
    expect(generations[0]!.input).toHaveLength(2); // snapshot before any tool calls
    expect(JSON.stringify(generations[1]!.input)).not.toContain("source tail");
    const primaryIds = primary.written[0]!.observations.slice(1).map((o) => o.id);
    expect(trace.observations.slice(1).map((o) => o.id)).toEqual(primaryIds);
    expect(trace.observations.some((o) => /compiler|workflow|planner/.test(o.name))).toBe(false);
  });

  it("records tool and child failures as errors, then lets the parent continue", async () => {
    const { tracer, written } = recording();
    const h = harness([
      () => call("search_news"),
      () => call("invoke_sub_agent", { prompt: "work" }),
      () => { throw new Error("model offline"); },
      () => answer("reported failure"),
    ], "[tool error] timeout", tracer);
    await (await h.start()).send("do work");
    const observations = written[0]!.observations;
    expect(observations.find((o) => o.name === "search_news")?.level).toBe("ERROR");
    expect(observations.find((o) => o.name === "invoke_sub_agent")?.level).toBe("ERROR");
    expect(observations.find((o) => o.statusMessage === "model offline")?.type).toBe("GENERATION");
  });

  it("recovers a failed primary loop in the same trace and releases both loops", async () => {
    const { tracer, written } = recording();
    const h = harness([
      () => { throw new Error("primary model offline"); },
      ({ messages }) => {
        expect(JSON.stringify(messages)).toContain(signal.content);
        return answer("recovery report");
      },
    ], "unused", tracer);
    const starts = vi.spyOn(h.engine, "startAgentLoop");
    const ends = vi.spyOn(h.engine, "endAgentLoop");
    const runner = supervisor(h);
    await expect(runner.runSignal(signal)).rejects.toThrow("primary model offline");
    expect(starts.mock.calls[1]?.[0].sessionContext).toBe(starts.mock.calls[0]?.[0].sessionContext);
    expect(starts.mock.calls[1]?.[0]).toMatchObject({ skills: ["recovery"], includeEngineSkills: false, maxIterations: 5 });
    expect(ends.mock.calls.map(([id]) => id)).toEqual(["telegram:1__recovery", "telegram:1"]);
    expect(written).toHaveLength(1);
    expect(written[0]?.output).toEqual({ error: "primary model offline" });
    expect(written[0]?.observations.find((o) => o.name === "recovery")).toMatchObject({ type: "AGENT", output: "recovery report" });
  });

  it("delegates domain signals without loading the domain skill into the parent", async () => {
    const h = harness([() => answer("done")]);
    const starts = vi.spyOn(h.engine, "startAgentLoop");
    const runner = supervisor(h);
    await runner.runSignal({ ...signal, source: "news-digest" });
    expect(starts.mock.calls[0]?.[0].skills).toEqual([]);
    expect(starts.mock.calls[0]?.[0].systemPrompt).toContain('Delegate the domain work to skill "news-digest"');
  });

  it("loads the shipped orchestration and worker prompts with valid frontmatter", async () => {
    const skills = createSkillStore({ liveDir: "/nonexistent-agent-test-skills" });
    const root = await skills.readSkill("orchestrator");
    const worker = await skills.readSkill("worker");
    expect(root?.tools).toBe("*");
    expect(root?.body).toContain("input_refs");
    expect(worker?.tools).toBe("*");
  });

  it("exports the primary loop and nested worker through the real Langfuse adapter", async () => {
    const local = recording();
    const langfuse = createLangfuseTracer({ publicKey: "test-public", secretKey: "test-secret" });
    const tracer = teeTracer(langfuse, local.tracer);
    const h = harness([
      () => call("search_news", { query: "latest" }),
      ({ messages }) => call("invoke_sub_agent", { prompt: "summarize", input_refs: [JSON.parse(lastResult(messages)).memory_key] }),
      () => answer("summary"),
      () => answer("done"),
    ], "large result ".repeat(1_000), tracer);
    const runner = supervisor(h);
    try {
      await runner.runSignal(signal);
      const attrs = LangfuseOtelSpanAttributes;
      const root = exported.find((s) => s.name === "signal:telegram")!;
      const primary = exported.find((s) => s.name === "agent_loop")!;
      const worker = exported.find((s) => s.name === "invoke_sub_agent")!;
      const tool = exported.find((s) => s.name === "search_news")!;
      expect(root.attributes[attrs.TRACE_SESSION_ID]).toBe("telegram:1");
      expect(root.attributes[attrs.OBSERVATION_OUTPUT]).toContain("done");
      expect(worker.attributes[attrs.OBSERVATION_TYPE]).toBe("agent");
      expect(worker.parentSpanContext?.spanId).toBe(primary.spanContext().spanId);
      expect(worker.attributes[attrs.OBSERVATION_INPUT]).toContain("large result");
      expect(tool.attributes[attrs.OBSERVATION_TYPE]).toBe("tool");
      expect(tool.attributes[`${attrs.OBSERVATION_METADATA}.memory_key`]).toBeDefined();
      expect(tool.attributes[attrs.OBSERVATION_OUTPUT]).toContain("large result");
      const generations = exported.filter((s) => s.attributes[attrs.OBSERVATION_TYPE] === "generation");
      expect(generations).toHaveLength(4);
      expect(generations.some((s) => s.parentSpanContext?.spanId === worker.spanContext().spanId)).toBe(true);
      expect(JSON.parse(String(generations[0]!.attributes[attrs.OBSERVATION_USAGE_DETAILS]))).toEqual({ input: 10, output: 5, total: 15, cached: 2 });
      expect(local.written[0]?.id).toBe(root.spanContext().traceId);
      expect(local.written[0]?.observations.find((o) => o.name === "invoke_sub_agent")?.id).toBe(worker.spanContext().spanId);
    } finally {
      await tracer.shutdown();
      otelTrace.disable();
      otelContext.disable();
      propagation.disable();
    }
  });
});
