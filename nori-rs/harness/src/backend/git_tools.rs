//! The git tools of the `nori-client` MCP server.
//!
//! They let an agent land a prompt's changes as a series of small commits without a shell: look
//! at what changed with `git_status` and `git_diff`, see the house style with `git_log`, then
//! commit one batch of paths per `git_commit`. All of them run in-process (see
//! `nori_config::GitRepo`), so a batch costs milliseconds and needs no `git`.

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::model::Content;
use rmcp::schemars;
use rmcp::tool;
use rmcp::tool_router;
use serde::Deserialize;

use super::nori_client_mcp::NoriClientService;
use super::nori_client_mcp::tool_error;
use super::nori_client_mcp::tool_success;

/// What the server tells agents about the git tools, after its other instructions.
pub(crate) const INSTRUCTIONS: &str = "Git: to land work as focused commits, see what changed (`git_status`, `git_diff`), match \
     the message style of `git_log`, group the files by purpose, and call `git_commit` once per \
     group with a message that explains why. Related changes together, unrelated ones apart, \
     never secrets.";

/// The most diff text one `git_diff` answer carries.
const MAX_DIFF_BYTES: usize = 96 * 1024;
const DEFAULT_LOG_LIMIT: i64 = 10;
const MAX_LOG_LIMIT: i64 = 100;

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

#[derive(Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
pub(crate) struct GitDiffRequest {
    /// Only these files or directories, relative to the repository root. Default: every change.
    #[serde(default)]
    pub(crate) paths: Option<Vec<String>>,
    /// The revision to compare the working tree with. Default: `HEAD`.
    #[serde(default)]
    pub(crate) base: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
pub(crate) struct GitLogRequest {
    /// How many commits, newest first: default 10, at most 100.
    #[serde(default)]
    pub(crate) limit: Option<i64>,
}

#[tool_router(router = git_tool_router, vis = "pub(crate)")]
impl NoriClientService {
    #[tool(
        description = "List every file that differs from HEAD (staged, unstaged or untracked) as `modified`, `added` or `deleted`, with the branch and HEAD commit.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false),
        meta = super::orchestrator::always_load()
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
        description = "Unified diff of the working tree against HEAD (or `base`), untracked files as new files, optionally only `paths`. Past 96 KB it is cut at a line and says how much was left out.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false),
        meta = super::orchestrator::always_load()
    )]
    pub(crate) async fn git_diff(
        &self,
        Parameters(GitDiffRequest { paths, base }): Parameters<GitDiffRequest>,
    ) -> Result<CallToolResult, McpError> {
        let Some(cwd) = self.shared_cwd() else {
            return Ok(tool_error("this session has no working directory"));
        };
        let base = base.unwrap_or_else(|| "HEAD".to_owned());
        let result = tokio::task::spawn_blocking(move || {
            let repo = nori_config::GitRepo::discover(&cwd).ok_or_else(|| {
                "the working directory is not inside a git repository".to_string()
            })?;
            let diff = repo
                .diff_text(&base, false)
                .ok_or_else(|| format!("`{base}` is not a commit"))?;
            Ok::<_, String>(match paths {
                Some(paths) => only_paths(&diff, &paths),
                None => diff,
            })
        })
        .await;
        Ok(match result {
            Ok(Ok(diff)) if diff.is_empty() => {
                CallToolResult::success(vec![Content::text("no changes")])
            }
            Ok(Ok(diff)) => CallToolResult::success(vec![Content::text(truncated(diff))]),
            Ok(Err(message)) => tool_error(message),
            Err(error) => tool_error(format!("git_diff was interrupted: {error}")),
        })
    }

    #[tool(
        description = "The latest commits on this branch, newest first: id, summary, author, time. Default 10.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false),
        meta = super::orchestrator::always_load()
    )]
    pub(crate) async fn git_log(
        &self,
        Parameters(GitLogRequest { limit }): Parameters<GitLogRequest>,
    ) -> Result<CallToolResult, McpError> {
        let Some(cwd) = self.shared_cwd() else {
            return Ok(tool_error("this session has no working directory"));
        };
        let limit = limit.unwrap_or(DEFAULT_LOG_LIMIT).clamp(1, MAX_LOG_LIMIT);
        let result = tokio::task::spawn_blocking(move || {
            let repo = nori_config::GitRepo::discover(&cwd)?;
            let commits: Vec<serde_json::Value> = repo
                .recent_commits(limit)
                .into_iter()
                .map(|commit| {
                    serde_json::json!({
                        "id": commit.id,
                        "summary": commit.summary,
                        "author": commit.author,
                        "time": chrono::DateTime::from_timestamp(commit.time, 0)
                            .map(|time| time.to_rfc3339())
                            .unwrap_or_default(),
                    })
                })
                .collect();
            Some(serde_json::json!({ "branch": repo.branch(), "commits": commits }))
        })
        .await;
        Ok(match result {
            Ok(Some(log)) => tool_success(log),
            Ok(None) => tool_error("the working directory is not inside a git repository"),
            Err(error) => tool_error(format!("git_log was interrupted: {error}")),
        })
    }

    #[tool(
        description = "Commit exactly these files as one commit with this message; everything else stays uncommitted. Returns the commit id and how many changed files remain.",
        annotations(destructive_hint = false, idempotent_hint = false, open_world_hint = false),
        meta = super::orchestrator::always_load()
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

/// The sections of a unified `diff` that are about one of `paths` (a file, or a directory and
/// everything under it).
fn only_paths(diff: &str, paths: &[String]) -> String {
    let wanted = |file: &str| {
        paths.iter().any(|path| {
            let path = path.trim_end_matches('/');
            file == path
                || file
                    .strip_prefix(path)
                    .is_some_and(|rest| rest.starts_with('/'))
        })
    };
    let mut out = String::new();
    let mut keep = false;
    for line in diff.split_inclusive('\n') {
        if let Some(header) = line.strip_prefix("diff --git a/") {
            let file = header.split(" b/").next().unwrap_or_default();
            keep = wanted(file);
        }
        if keep {
            out.push_str(line);
        }
    }
    out
}

/// `diff`, cut at a line boundary once it passes the size an answer may have.
fn truncated(diff: String) -> String {
    if diff.len() <= MAX_DIFF_BYTES {
        return diff;
    }
    let cut = diff[..MAX_DIFF_BYTES].rfind('\n').map_or(0, |at| at + 1);
    let omitted = diff[cut..].lines().count();
    format!(
        "{}[{omitted} more lines not shown: pass `paths` to see the rest]\n",
        &diff[..cut]
    )
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    const DIFF: &str = "diff --git a/src/a.rs b/src/a.rs\n-a\n+b\ndiff --git a/src/sub/c.rs b/src/sub/c.rs\n+c\ndiff --git a/srcx.rs b/srcx.rs\n+x\n";

    #[test]
    fn a_diff_narrows_to_files_and_directories() {
        assert_eq!(
            only_paths(DIFF, &["src/a.rs".to_owned()]),
            "diff --git a/src/a.rs b/src/a.rs\n-a\n+b\n"
        );
        assert_eq!(
            only_paths(DIFF, &["src/".to_owned()]),
            "diff --git a/src/a.rs b/src/a.rs\n-a\n+b\ndiff --git a/src/sub/c.rs b/src/sub/c.rs\n+c\n"
        );
        assert_eq!(only_paths(DIFF, &["nothing".to_owned()]), "");
    }

    #[test]
    fn a_long_diff_is_cut_at_a_line_and_says_so() {
        let line = "+".repeat(99) + "\n";
        let diff = line.repeat(MAX_DIFF_BYTES / 100 + 10);
        let cut = truncated(diff);
        assert!(cut.len() < MAX_DIFF_BYTES + 100);
        assert!(
            cut.ends_with("[10 more lines not shown: pass `paths` to see the rest]\n"),
            "{}",
            &cut[cut.len() - 80..]
        );
        assert_eq!(truncated("+a\n".to_owned()), "+a\n");
    }
}
