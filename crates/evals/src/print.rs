// Terminal rendering of node verdicts.

use crate::schema::{Faithfulness, JUDGE_MODEL, JUDGE_PROMPT_VERSION, NodeJudgement, Verdict};

// Header line for a judged node: its display label, kind, and owner skill.
pub struct NodeHeader<'a> {
    pub label: &'a str,
    pub kind: &'a str,
    pub skill: &'a str,
}

pub fn print_trace_header(trace_id: &str) {
    println!("\n=== JUDGE {JUDGE_MODEL} (prompt {JUDGE_PROMPT_VERSION}) · trace {trace_id} ===");
}

// One block per node: the axis scorecard (planner: query_formulation/process;
// composer/agent: coverage/composition) and, for compose/agent, faithfulness.
pub fn print_node_judgement(node: &NodeHeader<'_>, j: &NodeJudgement) {
    println!("\n── node {} · {} · skill {} ──", node.label, node.kind, node.skill);
    for a in &j.scorecard.axes {
        let score = match (a.applicable, a.score) {
            (true, Some(s)) => format!("{s:.2}"),
            _ => "n/a".into(),
        };
        println!("● {}: {} ({score})", a.axis.as_str(), a.label.as_str());
        println!("  {}", a.rationale);
        if !a.evidence.is_empty() {
            println!("  ↳ {}", a.evidence);
        }
    }
    println!("  overall: {}", j.scorecard.overall_note);
    if let Some(f) = &j.faithfulness {
        print_faithfulness(f);
    }
}

fn print_faithfulness(f: &Faithfulness) {
    if !f.applicable {
        println!("● faithfulness: n/a — {}", f.note);
        return;
    }
    let score = f.score.map_or("—".into(), |s| format!("{s:.2}"));
    let bad = f.claims.iter().filter(|c| c.verdict != Verdict::Supported).count();
    println!("● faithfulness: {score}  ({} claims, {bad} not fully supported)", f.claims.len());
    for c in &f.claims {
        let mark = match c.verdict {
            Verdict::Supported => "✓",
            Verdict::Partial => "~",
            Verdict::Unsupported => "✗",
        };
        println!("  {mark} {}", c.claim);
        if c.verdict != Verdict::Supported {
            println!("      ↳ {}", c.evidence);
        }
    }
    if !f.note.is_empty() {
        println!("  {}", f.note);
    }
}

// Compact one-liner per node for a multi-node / multi-trace summary table:
//   node llm_compose:digest · compose · skill news-digest · coverage 0.80 composition 0.68 faithfulness 0.91
pub fn node_summary_line(node: &NodeHeader<'_>, j: &NodeJudgement) -> String {
    let mut parts: Vec<String> = j
        .scorecard
        .axes
        .iter()
        .filter(|a| a.applicable)
        .filter_map(|a| a.score.map(|s| format!("{} {s:.2}", a.axis.as_str())))
        .collect();
    if let Some(f) = j.faithfulness.as_ref().filter(|f| f.applicable)
        && let Some(s) = f.score
    {
        parts.push(format!("faithfulness {s:.2}"));
    }
    let scores = if parts.is_empty() { "n/a".into() } else { parts.join(" ") };
    format!("node {} · {} · skill {} · {scores}", node.label, node.kind, node.skill)
}
