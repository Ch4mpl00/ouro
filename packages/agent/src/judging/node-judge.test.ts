import { describe, expect, it } from "vitest";
import { judgeNode } from "./node-judge";
import type { JudgeBackend } from "./judge-backend";
import type { JudgeNodeInput } from "./schema";

// A backend that records which completions it's asked for and returns a minimal
// valid payload for each (scorecard / faithfulness). Lets us assert the gate's
// cost knob (skipFaithfulness) without any network.
function recordingBackend(): { backend: JudgeBackend; names: string[] } {
  const names: string[] = [];
  const backend: JudgeBackend = {
    async complete(req) {
      names.push(req.name);
      if (req.name === "faithfulness") {
        return { applicable: true, claims: [], score: 1, note: "" };
      }
      return {
        axes: [{ axis: "coverage", applicable: true, score: 0.4, label: "weak", rationale: "r", evidence: "e" }],
        overall_note: "",
      };
    },
  };
  return { backend, names };
}

const composeNode: JudgeNodeInput = {
  kind: "compose",
  skill: "news-digest",
  contract: "C",
  inputText: "I",
  outputText: "O",
};

describe("judgeNode skipFaithfulness", () => {
  it("runs both passes by default (scorecard + faithfulness)", async () => {
    const { backend, names } = recordingBackend();
    const v = await judgeNode(backend, composeNode);
    expect(names.sort()).toEqual(["faithfulness", "scorecard"]);
    expect(v.faithfulness).not.toBeNull();
  });

  it("skips the faithfulness pass when asked — halving codex for a compose node", async () => {
    const { backend, names } = recordingBackend();
    const v = await judgeNode(backend, composeNode, { skipFaithfulness: true });
    expect(names).toEqual(["scorecard"]);
    expect(v.faithfulness).toBeNull();
  });
});
