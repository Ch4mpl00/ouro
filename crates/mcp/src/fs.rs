// read_file: a UTF-8 text file from disk, path absolute or relative to the
// repo root (MCP runs from it, so the working directory is the anchor).

use std::path::PathBuf;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{schemars, tool, tool_router};
use serde::Deserialize;
use serde_json::json;

use crate::server::{McpTools, ToolResult, respond};

#[derive(Deserialize, schemars::JsonSchema)]
struct ReadFileParams {
    /// Absolute path or path relative to the repo root.
    path: String,
}

#[tool_router(router = fs_tools, vis = "pub(crate)")]
impl McpTools {
    #[tool(
        name = "read_file",
        title = "Read a text file",
        description = "Read a UTF-8 text file (markdown, txt, etc) and return its contents. \
            Use this to load skill instructions like `skills/telegram.md` when \
            handling a signal. Path may be absolute or relative to the repo root."
    )]
    async fn read_file(&self, Parameters(p): Parameters<ReadFileParams>) -> ToolResult {
        respond(
            async {
                let resolved: PathBuf = std::path::absolute(&p.path)?;
                let content = tokio::fs::read_to_string(&resolved).await?;
                Ok(json!({ "path": resolved, "content": content }))
            }
            .await,
        )
    }
}
