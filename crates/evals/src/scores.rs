// Persists ONE node's verdict to two places: the local `judgements` table (the
// per-node corpus the self-improvement loop queries) and Langfuse scores (the
// UI / dashboards). The local PK is (trace, observation), and the Langfuse
// score carries `observationId` = the judged node, so both link with no
// mapping and scores render ON THE STEP in the trace UI. dry_run prints and
// persists NOTHING — so flipping JUDGE_WRITE_SCORES on later re-judges every
// node instead of finding them already recorded.

use agent::db::{AgentDb, JudgementInput};
use serde_json::{Value, json};

use crate::langfuse_api::Langfuse;
use crate::noise::extract_axis_scores;
use crate::schema::{Faithfulness, NodeJudgement, NodeKind, Scorecard, Verdict};

pub struct NodeScoreWriteOpts<'a> {
    pub trace_id: &'a str,
    pub observation_id: &'a str,
    pub node_kind: NodeKind,
    pub skill: &'a str,
    pub provider: &'a str,
    pub prompt_version: &'a str,
    pub dry_run: bool,
}

fn comment(parts: &[&str]) -> String {
    parts.iter().filter(|p| !p.is_empty()).copied().collect::<Vec<_>>().join("\n")
}

pub fn score_payloads(card: &Scorecard, faith: Option<&Faithfulness>, o: &NodeScoreWriteOpts<'_>) -> Vec<Value> {
    let base = json!({
        "judge_provider": o.provider,
        "judge_prompt_version": o.prompt_version,
        "node_kind": o.node_kind.as_str(),
        "skill": o.skill,
    });
    let mut payloads = Vec::new();
    for axis in &card.axes {
        let (true, Some(score)) = (axis.applicable, axis.score) else { continue };
        let mut metadata = base.clone();
        metadata["label"] = axis.label.as_str().into();
        payloads.push(json!({
            "traceId": o.trace_id,
            "observationId": o.observation_id,
            "name": format!("judge.{}", axis.axis.as_str()),
            "value": score,
            "comment": comment(&[axis.label.as_str(), &axis.rationale, &axis.evidence]),
            "metadata": metadata,
        }));
    }
    if let Some(f) = faith.filter(|f| f.applicable)
        && let Some(score) = f.score
    {
        let issues: Vec<String> = f
            .claims
            .iter()
            .filter(|c| c.verdict != Verdict::Supported)
            .map(|c| {
                let verdict = if c.verdict == Verdict::Partial { "partial" } else { "unsupported" };
                format!("{verdict}: {} ({})", c.claim, c.evidence)
            })
            .collect();
        let mut metadata = base.clone();
        metadata["claim_count"] = f.claims.len().into();
        metadata["unsupported_count"] = f.claims.iter().filter(|c| c.verdict == Verdict::Unsupported).count().into();
        metadata["partial_count"] = f.claims.iter().filter(|c| c.verdict == Verdict::Partial).count().into();
        payloads.push(json!({
            "traceId": o.trace_id,
            "observationId": o.observation_id,
            "name": "judge.faithfulness",
            "value": score,
            "comment": comment(&[&f.note, &issues.join("\n")]),
            "metadata": metadata,
        }));
    }
    payloads
}

pub struct ScoreWriter {
    pub db: AgentDb,
    pub langfuse: Option<Langfuse>,
}

impl ScoreWriter {
    // `verdict.faithfulness` is None for planner nodes (no faithfulness axis).
    pub async fn write(&self, verdict: &NodeJudgement, o: &NodeScoreWriteOpts<'_>) -> anyhow::Result<()> {
        let payloads = score_payloads(&verdict.scorecard, verdict.faithfulness.as_ref(), o);
        if o.dry_run {
            for p in &payloads {
                println!(
                    "[judge] dry-run score {}/{} {}={}",
                    o.trace_id,
                    o.observation_id,
                    p["name"].as_str().unwrap_or(""),
                    p["value"]
                );
            }
            return Ok(());
        }
        // Local corpus first — it's the source of truth for the improver and
        // doesn't depend on Langfuse being up.
        self.db.write_judgement(&JudgementInput {
            trace_id: o.trace_id.into(),
            observation_id: o.observation_id.into(),
            node_kind: o.node_kind.as_str().into(),
            skill: o.skill.into(),
            provider: o.provider.into(),
            prompt_version: o.prompt_version.into(),
            scores: extract_axis_scores(verdict).to_db(),
            detail: json!({ "scorecard": verdict.scorecard, "faithfulness": verdict.faithfulness }),
        })?;
        let Some(langfuse) = &self.langfuse else {
            println!("[judge] wrote local judgement {}/{} (langfuse disabled)", o.trace_id, o.observation_id);
            return Ok(());
        };
        for p in &payloads {
            langfuse.post("/scores", p).await?;
            println!(
                "[judge] wrote score {}/{} {}={}",
                o.trace_id,
                o.observation_id,
                p["name"].as_str().unwrap_or(""),
                p["value"]
            );
        }
        Ok(())
    }
}
