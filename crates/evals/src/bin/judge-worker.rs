// The online per-node judge (compose service `judge-worker`, `evals` profile).
use agent::db::AgentDb;
use agent::skills::FsSkillStore;
use evals::env::{db_path, load_env, root};
use evals::langfuse_api::Langfuse;
use evals::scores::ScoreWriter;
use evals::source::local_with_langfuse;
use evals::worker::{JudgeWorkerDeps, JudgeWorkerOpts, run_judge_worker};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    load_env();
    let root = root()?;
    let db = AgentDb::open(db_path(&root))?;
    // Langfuse is optional: a get_trace fallback for ids not mirrored
    // locally, and the scores sink. Listing always comes from local.
    let source = local_with_langfuse(db.clone())?;
    let langfuse = if Langfuse::enabled() { Some(Langfuse::from_env()?) } else { None };
    let writer = ScoreWriter { db, langfuse };
    let skills = FsSkillStore::new(&root);
    let deps = JudgeWorkerDeps { source: &source, skills: &skills, writer: &writer };
    if let Err(err) = run_judge_worker(&deps, &JudgeWorkerOpts::from_env()).await {
        eprintln!("[judge-worker] fatal: {err:#}");
        std::process::exit(1);
    }
    Ok(())
}
