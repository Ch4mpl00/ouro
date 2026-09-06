import type { Engine } from "../engine";
import type { AgentLoop } from "../agent-loop";
import { createSessionContext, gatherEnvData, renderContext, type EnvDataDeps, type SessionContext } from "../session-context";
import type { Span, Trace } from "../tracing";
import type { WorkflowRunner } from "../workflow";
import { prepareTelegramInput } from "./telegram-context";

export interface PendingSignal {
  id: number;
  source: string;
  content: string;
  envContext: string | null;
  created_at: string;
}

export interface SupervisorModule {
  runSignal(signal: PendingSignal): Promise<string>;
}

export interface SupervisorModuleDeps {
  engine: Engine;
  env: EnvDataDeps;
  workflow: WorkflowRunner;
}

// Routing is deterministic: scheduler uses compile → execute; every other
// source uses AgentLoop. Both paths share one context and trace per signal.
export function createSupervisorModule({ engine, env, workflow }: SupervisorModuleDeps): SupervisorModule {
  async function recover(signal: PendingSignal, context: SessionContext, error: string, loop: AgentLoop | undefined, trace: Trace): Promise<void> {
    const span = trace.span({ name: "recovery", kind: "agent", metadata: { skill: "recovery" } });
    const id = `${context.id}__recovery`;
    try {
      const recovery = await engine.startAgentLoop({
        id,
        sessionContext: context,
        parentId: context.id,
        skills: ["recovery"],
        includeEngineSkills: false,
        systemPrompt: [renderContext(context.env), signal.envContext].filter(Boolean).join("\n\n"),
        preset: "base",
        maxIterations: 5,
        traceScope: span,
      });
      const transcript = JSON.stringify(loop?.messages ?? []);
      const output = await recovery.send([
        `Original signal (${signal.source}):\n${signal.content}`,
        `Error: ${error}`,
        `Recent transcript (may be truncated):\n${transcript.slice(-20_000)}`,
        "Report the failure only. Do not repeat the original actions; some may already have succeeded.",
      ].join("\n\n"));
      span.end({ output });
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      span.end({ output: { error: message }, level: "ERROR", statusMessage: message });
      engine.log(context.id, `recovery failed: ${message}`);
    } finally {
      engine.endAgentLoop(id);
    }
  }

  return {
    async runSignal(signal) {
      const id = `${signal.source}:${signal.id}`;
      const useWorkflow = signal.source === "scheduler";
      const trace = engine.tracer.trace({
        id,
        name: `signal:${signal.source}`,
        kind: "agent",
        sessionId: id,
        tags: [signal.source, useWorkflow ? "planner-mode" : "agent-loop"],
        metadata: { signal_id: signal.id, signal_source: signal.source, signal_created_at: signal.created_at },
      });
      trace.update({ input: signal.content });
      let context: SessionContext | undefined;
      let loop: AgentLoop | undefined;
      let span: Span | undefined;
      try {
        context = createSessionContext({ id, env: await gatherEnvData(env) });
        if (useWorkflow) {
          const result = await workflow.runForSignal(signal, context, trace);
          if (result.ok) {
            const summary = { ok: true, attempts: result.attempts, stepCount: result.stepCount };
            trace.update({ output: summary });
            engine.log(id, `workflow ok (attempts=${result.attempts}, steps=${result.stepCount})`);
            return JSON.stringify(summary);
          }
          if (result.stage === "execute") {
            // An earlier step may already have delivered. Report the error;
            // never restart the task in a fresh agentic loop after execution.
            trace.event({ name: "fallback", level: "ERROR", metadata: {
              stage: result.stage, reason: result.reason, step_index: result.stepIndex,
            } });
            throw new Error(`Workflow execution failed at step ${result.stepIndex} (${result.reason}): ${result.error.message}`);
          }
          // Preserve the previous compile/replan fallback. The same context
          // and trace continue into the agentic path below.
          trace.event({ name: "fallback", level: "WARNING", metadata: {
            stage: result.stage,
            reason: result.stage === "compile" ? result.reason : "replan_exhausted",
            attempts: result.attempts,
            ...(result.stage === "compile" ? { errors: result.errors } : { passes: result.passes }),
          } });
          trace.update({ metadata: { fallback: "agent-loop", workflow_failure_stage: result.stage } });
          engine.log(id, `workflow ${result.stage} failed; falling back to AgentLoop`);
        }
        // A chain groups the primary loop without hiding its child AGENT nodes
        // from the per-node judge, which treats AGENT descendants as a black box.
        span = trace.span({ name: "agent_loop", kind: "chain" });
        const input = await prepareTelegramInput(signal, context, engine.mcp, trace);
        // Transport skills describe delivery. Domain skills belong in workers;
        // a new source is a delegation hint, so adding one needs no code change.
        const transport = signal.source === "telegram" || signal.source === "scheduler";
        loop = await engine.startAgentLoop({
          id,
          sessionContext: context,
          skills: transport ? [signal.source] : [],
          systemPrompt: [
            renderContext(context.env),
            `Signal source: ${signal.source}`,
            transport ? "" : `Delegate the domain work to skill ${JSON.stringify(signal.source)}. You own the final delivery.`,
            signal.envContext,
          ].filter(Boolean).join("\n\n"),
          preset: useWorkflow ? "base" : "smart",
          traceScope: span,
        });
        const output = await loop.send(input);
        span.end({ output });
        trace.update({ output, metadata: { skills: transport ? [signal.source, "orchestrator", "routing"] : ["orchestrator", "routing"] } });
        return output;
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        span?.end({ output: { error: message }, level: "ERROR", statusMessage: message });
        trace.update({ output: { error: message }, metadata: { error: true } });
        if (context) await recover(signal, context, message, loop, trace);
        throw err;
      } finally {
        if (span) engine.endAgentLoop(id);
        trace.end();
      }
    },
  };
}
