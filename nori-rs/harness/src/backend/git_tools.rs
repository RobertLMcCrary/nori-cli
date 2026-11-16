//! The git tools of the `nori-client` MCP server.
//!
//! They let an agent land a prompt's changes as a series of small commits without a shell: look
//! at what changed with `git_status`, then commit one batch of paths per `git_commit`. Both run
//! in-process (see `nori_config::GitRepo`), so a batch costs milliseconds and needs no `git`.

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::schemars;
use rmcp::tool;
use rmcp::tool_router;
use serde::Deserialize;

use super::nori_client_mcp::NoriClientService;
use super::nori_client_mcp::tool_error;
use super::nori_client_mcp::tool_success;

/// What the server tells agents about the git tools, after its other instructions.
pub(crate) const INSTRUCTIONS: &str = "Git: `git_status` lists every file that differs from HEAD. To land work as several focused \
     commits, group the files by purpose and call `git_commit` once per group with exactly those \
     paths and a message that explains the why; whatever you leave out stays uncommitted for the \
     next call. Commit related changes together, unrelated ones apart, and never commit secrets.";

#[derive(Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
pub(crate) struct GitCommitRequest {
    /// Required. The files to put in this commit, relative to the repository root. A file that no
    /// longer exists is committed as a deletion. Directories are not accepted: list their files.
    pub(crate) paths: Vec<String>,
    /// Required. The commit message: a short summary line, and a body when the why needs one.
    pub(crate) message: String,
}

#[tool_router(router = git_tool_router, vis = "pub(crate)")]
impl NoriClientService {
    #[tool(
        description = "List every file that differs from HEAD (staged, unstaged or untracked) with its state (modified, added or deleted), plus the current branch and HEAD commit."
    )]
    pub(crate) async fn git_status(&self) -> Result<CallToolResult, McpError> {
        let Some(cwd) = self.shared_cwd() else {
            return Ok(tool_error("this session has no working directory"));
        };
        let result = tokio::task::spawn_blocking(move || {
            let repo = nori_config::GitRepo::discover(&cwd)?;
            let changes: Vec<serde_json::Value> = repo
                .changed_files()
                .into_iter()
                .map(|file| {
                    serde_json::json!({
                        "path": file.path,
                        "state": match file.state {
                            nori_config::ChangeState::Modified => "modified",
                            nori_config::ChangeState::Added => "added",
                            nori_config::ChangeState::Deleted => "deleted",
                        },
                    })
                })
                .collect();
            Some(serde_json::json!({
                "branch": repo.branch(),
                "head": repo.head_commit(),
                "changes": changes,
            }))
        })
        .await;
        Ok(match result {
            Ok(Some(status)) => tool_success(status),
            Ok(None) => tool_error("the working directory is not inside a git repository"),
            Err(error) => tool_error(format!("git_status was interrupted: {error}")),
        })
    }

    #[tool(
        description = "Commit exactly the given files as one commit on the current branch, with the given message. Everything else stays uncommitted. Call it once per logical change to build a series of small commits; returns the new commit id and how many changed files remain."
    )]
    pub(crate) async fn git_commit(
        &self,
        Parameters(GitCommitRequest { paths, message }): Parameters<GitCommitRequest>,
    ) -> Result<CallToolResult, McpError> {
        let Some(cwd) = self.shared_cwd() else {
            return Ok(tool_error("this session has no working directory"));
        };
        let message = message.trim().to_string();
        if message.is_empty() {
            return Ok(tool_error("a commit needs a message"));
        }
        if paths.is_empty() {
            return Ok(tool_error("a commit needs at least one path"));
        }
        let result = tokio::task::spawn_blocking(move || {
            let repo = nori_config::GitRepo::discover(&cwd).ok_or_else(|| {
                "the working directory is not inside a git repository".to_string()
            })?;
            let commit = repo
                .commit_paths(&paths, &message)
                .map_err(|error| error.to_string())?;
            Ok::<_, String>(serde_json::json!({
                "commit": commit,
                "remaining_changes": repo.changed_files().len(),
            }))
        })
        .await;
        Ok(match result {
            Ok(Ok(body)) => tool_success(body),
            Ok(Err(message)) => tool_error(message),
            Err(error) => tool_error(format!("git_commit was interrupted: {error}")),
        })
    }
}
