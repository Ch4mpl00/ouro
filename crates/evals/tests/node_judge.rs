// A port of packages/agent/src/judging/node-judge.test.ts: the gate's cost
// knob (skip_faithfulness) without any network.
use std::sync::Mutex;

use async_trait::async_trait;
use evals::backend::{JudgeBackend, JudgeCompletion};
use evals::node_judge::judge_node;
use evals::schema::{JudgeNodeInput, NodeKind};
use serde_json::{Value, json};

// Records which completions it's asked for and returns a minimal valid
// payload for each (scorecard / faithfulness).
#[derive(Default)]
struct Recording {
    names: Mutex<Vec<String>>,
}

#[async_trait]
impl JudgeBackend for Recording {
    async fn complete(&self, req: &JudgeCompletion<'_>) -> anyhow::Result<Value> {
        self.names.lock().unwrap().push(req.name.into());
        if req.name == "faithfulness" {
            return Ok(json!({ "applicable": true, "claims": [], "score": 1, "note": "" }));
        }
        Ok(json!({
            "axes": [{ "axis": "coverage", "applicable": true, "score": 0.4, "label": "weak", "rationale": "r", "evidence": "e" }],
            "overall_note": "",
        }))
    }
}

fn compose_node() -> JudgeNodeInput {
    JudgeNodeInput {
        kind: NodeKind::Compose,
        skill: "news-digest".into(),
        contract: Some("C".into()),
        input_text: "I".into(),
        output_text: "O".into(),
    }
}

#[tokio::test]
async fn runs_both_passes_by_default() {
    let backend = Recording::default();
    let v = judge_node(&backend, &compose_node(), false).await.unwrap();
    let mut names = backend.names.lock().unwrap().clone();
    names.sort();
    assert_eq!(names, ["faithfulness", "scorecard"]);
    assert!(v.faithfulness.is_some());
}

#[tokio::test]
async fn skips_the_faithfulness_pass_when_asked() {
    let backend = Recording::default();
    let v = judge_node(&backend, &compose_node(), true).await.unwrap();
    assert_eq!(*backend.names.lock().unwrap(), ["scorecard"]);
    assert!(v.faithfulness.is_none());
}
