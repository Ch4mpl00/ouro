// Process environment for the eval binaries: .env files, flags in env vars,
// and the repository root the skills and baselines live under.

use std::path::PathBuf;

// `.env` then `.env.agent` (the agent container's env file), neither
// overriding what the process already has.
pub fn load_env() {
    let _ = dotenvy::from_filename(".env");
    let _ = dotenvy::from_filename(".env.agent");
}

pub fn bool_env(name: &str, fallback: bool) -> bool {
    match std::env::var(name) {
        Err(_) => fallback,
        Ok(v) => ["1", "true", "yes", "on"].contains(&v.to_lowercase().as_str()),
    }
}

pub fn num_env(name: &str, fallback: f64) -> f64 {
    std::env::var(name).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(fallback)
}

// AGENT_ROOT, else the nearest ancestor of the working directory holding
// skills.default — the repo root locally, /app in the image.
pub fn root() -> anyhow::Result<PathBuf> {
    if let Some(path) = std::env::var_os("AGENT_ROOT") {
        return Ok(PathBuf::from(path));
    }
    let mut path = std::env::current_dir()?;
    loop {
        if path.join("skills.default").is_dir() {
            return Ok(path);
        }
        if !path.pop() {
            anyhow::bail!("cannot find skills.default; set AGENT_ROOT or run from the repository root");
        }
    }
}

// agent.db: AGENT_DB_PATH, else its place under the root.
pub fn db_path(root: &std::path::Path) -> PathBuf {
    std::env::var_os("AGENT_DB_PATH").map(PathBuf::from).unwrap_or_else(|| root.join(agent::db::DEFAULT_DB_PATH))
}
