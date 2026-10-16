// Shared reader for the committed judge-noise baseline (crates/evals/
// noise-baseline.json, written by judge-noise). The gate, the improver, and
// the cron worker all read the SAME per-(judge model | prompt version) σ floor
// from here so they can't drift. `found` lets a caller warn when there's no
// baseline (verdicts then read "no-baseline").

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::gate::Sigma;
use crate::noise::NoiseAxis;

pub const BASELINE_FILE: &str = "crates/evals/noise-baseline.json";

pub fn baseline_path(root: &Path) -> PathBuf {
    root.join(BASELINE_FILE)
}

pub fn load_sigma_baseline(root: &Path, judge_model: &str, prompt_version: &str) -> (Sigma, bool) {
    let Ok(text) = std::fs::read_to_string(baseline_path(root)) else { return (Sigma::new(), false) };
    let Ok(entries) = serde_json::from_str::<Value>(&text) else { return (Sigma::new(), false) };
    let entry = &entries[format!("{judge_model}|{prompt_version}")];
    let Some(axes) = entry["axes"].as_array() else { return (Sigma::new(), false) };
    let mut sigma = Sigma::new();
    for a in axes {
        if let (Some(axis), Some(s)) =
            (a["axis"].as_str().and_then(|x| NoiseAxis::parse(x).ok()), a["pooledSigma"].as_f64())
        {
            sigma.insert(axis, s);
        }
    }
    (sigma, true)
}
