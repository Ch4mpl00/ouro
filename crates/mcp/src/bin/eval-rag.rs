// Score the RAG fixtures under one config; writes a markdown report.
//   eval-rag --config crates/mcp/eval/configs/baseline-dedup-003.json

use std::path::{Path, PathBuf};
use std::time::Instant;

use mcp_tools::cli;
use mcp_tools::embeddings::OpenAiEmbedder;
use mcp_tools::eval::{self, EVAL_MAX_CHARS, EvalPaths};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    cli::init();
    let dir = Path::new(cli::EVAL_DIR);
    let config_path = cli::arg("config").map_or_else(|| dir.join("configs/baseline.json"), PathBuf::from);
    let api_key =
        std::env::var("OPENAI_API_KEY").map_err(|_| anyhow::anyhow!("OPENAI_API_KEY is required (set in .env.mcp)"))?;
    let config = eval::load_config(&config_path)?;
    println!("[eval-rag] config: {} ({})", config.name, config_path.display());
    let embedder = OpenAiEmbedder::with_model(
        reqwest::Client::new(),
        api_key,
        &config.retrieval.embed.model,
        config.retrieval.embed.dimensions,
        EVAL_MAX_CHARS,
    );

    let started = Instant::now();
    let result = eval::run(config, &EvalPaths::under(dir), &embedder).await?;
    let reports = dir.join("reports");
    std::fs::create_dir_all(&reports)?;
    let path = reports.join(format!("{}-{}.md", result.config.name, chrono::Utc::now().format("%Y%m%d-%H%M")));
    std::fs::write(&path, eval::render_markdown(&result))?;

    let a = &result.aggregate;
    println!(
        "\n[eval-rag] {} · {}ms",
        if result.cache_hit { "cache hit" } else { "fresh embed" },
        started.elapsed().as_millis()
    );
    println!("[eval-rag] scored queries: {}", a.scored_queries);
    println!("[eval-rag] P@5   {:.3}   R@5   {:.3}", a.precision_at5, a.recall_at5);
    println!("[eval-rag] P@10  {:.3}   R@10  {:.3}   R@30  {:.3}", a.precision_at10, a.recall_at10, a.recall_at30);
    println!("[eval-rag] MRR   {:.3}", a.mrr);
    println!("[eval-rag] uniq sources @5/@10  {:.2} / {:.2}", a.mean_unique_sources_at5, a.mean_unique_sources_at10);
    println!("[eval-rag] report → {}", path.display());
    Ok(())
}
