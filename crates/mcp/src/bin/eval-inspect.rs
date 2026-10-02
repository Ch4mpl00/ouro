// Per-query top-k with gold/acceptable marks, for debugging labels.
//   eval-inspect --qids q-014,q-017 [--k 15] [--config <path>]

use std::path::{Path, PathBuf};

use mcp_tools::cli;
use mcp_tools::embeddings::OpenAiEmbedder;
use mcp_tools::eval::{self, EVAL_MAX_CHARS, EvalPaths};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    cli::init();
    let dir = Path::new(cli::EVAL_DIR);
    let config_path = cli::arg("config").map_or_else(|| dir.join("configs/baseline.json"), PathBuf::from);
    let qids: Vec<String> = cli::arg("qids")
        .ok_or_else(|| anyhow::anyhow!("--qids q-014,q-017,... required"))?
        .split(',')
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect();
    let k: usize = cli::arg("k").map_or(Ok(15), |k| k.parse())?;
    anyhow::ensure!(k > 0, "--k must be a positive integer");
    let api_key =
        std::env::var("OPENAI_API_KEY").map_err(|_| anyhow::anyhow!("OPENAI_API_KEY is required (set in .env.mcp)"))?;
    let config = eval::load_config(&config_path)?;
    println!("[inspect] config: {} ({})", config.name, config_path.display());
    println!("[inspect] qids: {} · k={k}\n", qids.join(", "));
    let embedder = OpenAiEmbedder::with_model(
        reqwest::Client::new(),
        api_key,
        &config.retrieval.embed.model,
        config.retrieval.embed.dimensions,
        EVAL_MAX_CHARS,
    );
    println!("{}", eval::inspect(&config, &EvalPaths::under(dir), &embedder, &qids, k).await?);
    Ok(())
}
