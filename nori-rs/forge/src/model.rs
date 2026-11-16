//! What the forge answers, reduced to what an agent or a workflow acts on.

use serde::Serialize;

/// A pull request, as `ci_status` and `pull_request` report it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PullRequest {
    pub number: i64,
    pub title: String,
    pub url: String,
    pub state: PullState,
    pub draft: bool,
    /// The branch it merges into.
    pub base: String,
    /// The branch it merges from.
    pub head: String,
    /// The commit at the tip of `head`, as GitHub last saw it.
    pub head_sha: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PullState {
    Open,
    Closed,
    Merged,
}

impl PullRequest {
    pub(crate) fn from_json(json: &serde_json::Value) -> Option<Self> {
        let text = |value: &serde_json::Value| value.as_str().unwrap_or_default().to_owned();
        let state = match (json["state"].as_str(), json["merged_at"].is_string()) {
            (_, true) => PullState::Merged,
            (Some("open"), false) => PullState::Open,
            (_, false) => PullState::Closed,
        };
        Some(Self {
            number: json["number"].as_i64()?,
            title: text(&json["title"]),
            url: text(&json["html_url"]),
            state,
            draft: json["draft"].as_bool().unwrap_or(false),
            base: text(&json["base"]["ref"]),
            head: text(&json["head"]["ref"]),
            head_sha: text(&json["head"]["sha"]),
        })
    }
}

/// A pull request to open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPull {
    pub title: String,
    pub body: String,
    /// The branch to merge from, already pushed.
    pub head: String,
    /// The branch to merge into.
    pub base: String,
    pub draft: bool,
}

/// The checks on one commit: GitHub check runs and the older commit statuses together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Checks {
    pub sha: String,
    pub runs: Vec<CheckRun>,
}

/// One check, a check run or a commit status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckRun {
    pub name: String,
    pub state: RunState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The GitHub Actions job behind the run, whose log `Forge::job_log` reads.
    #[serde(skip)]
    pub job: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    /// Queued or waiting to start.
    Pending,
    Running,
    Passed,
    Failed,
    /// Stopped before it finished, usually because a newer push replaced it.
    Cancelled,
    /// Neutral or skipped: it finished without a verdict.
    Skipped,
}

/// Where all the checks on a commit stand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckState {
    /// The commit has no checks, or none have started.
    None,
    Pending,
    Passing,
    Failing,
    /// Nothing failed, but a check was cancelled.
    Cancelled,
}

impl CheckRun {
    /// A GitHub check run.
    pub(crate) fn from_check_run(json: &serde_json::Value) -> Option<Self> {
        let state = match (json["status"].as_str()?, json["conclusion"].as_str()) {
            ("in_progress", _) => RunState::Running,
            ("completed", Some("success")) => RunState::Passed,
            ("completed", Some("neutral" | "skipped")) => RunState::Skipped,
            ("completed", Some("cancelled" | "stale")) => RunState::Cancelled,
            ("completed", _) => RunState::Failed,
            _ => RunState::Pending,
        };
        let url = json["html_url"]
            .as_str()
            .or_else(|| json["details_url"].as_str())
            .map(str::to_owned);
        Some(Self {
            name: json["name"].as_str()?.to_owned(),
            state,
            url,
            job: (json["app"]["slug"].as_str() == Some("github-actions"))
                .then(|| json["id"].as_i64())
                .flatten(),
        })
    }

    /// A commit status, from CI that reports the older way.
    pub(crate) fn from_status(json: &serde_json::Value) -> Option<Self> {
        let state = match json["state"].as_str()? {
            "success" => RunState::Passed,
            "pending" => RunState::Pending,
            _ => RunState::Failed,
        };
        Some(Self {
            name: json["context"].as_str()?.to_owned(),
            state,
            url: json["target_url"]
                .as_str()
                .filter(|url| !url.is_empty())
                .map(str::to_owned),
            job: None,
        })
    }
}

impl Checks {
    /// A failure outranks anything still running: the commit cannot pass any more.
    pub fn state(&self) -> CheckState {
        let any = |states: &[RunState]| self.runs.iter().any(|run| states.contains(&run.state));
        if self.runs.is_empty() {
            CheckState::None
        } else if any(&[RunState::Failed]) {
            CheckState::Failing
        } else if any(&[RunState::Pending, RunState::Running]) {
            CheckState::Pending
        } else if any(&[RunState::Cancelled]) {
            CheckState::Cancelled
        } else {
            CheckState::Passing
        }
    }

    /// The runs in `states`.
    pub fn in_states<'a>(&'a self, states: &'a [RunState]) -> impl Iterator<Item = &'a CheckRun> {
        self.runs.iter().filter(|run| states.contains(&run.state))
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::*;

    fn run(state: RunState) -> CheckRun {
        CheckRun {
            name: "test".to_owned(),
            state,
            url: None,
            job: None,
        }
    }

    #[test]
    fn check_runs_read_their_state_and_job() {
        let actions = json!({"id": 7, "name": "test (ubuntu)", "status": "completed",
            "conclusion": "failure", "html_url": "https://github.com/o/r/runs/7",
            "app": {"slug": "github-actions"}});
        assert_eq!(
            CheckRun::from_check_run(&actions),
            Some(CheckRun {
                name: "test (ubuntu)".to_owned(),
                state: RunState::Failed,
                url: Some("https://github.com/o/r/runs/7".to_owned()),
                job: Some(7),
            })
        );
        let other = json!({"id": 8, "name": "lint", "status": "queued", "conclusion": null,
            "details_url": "https://ci.example/8", "app": {"slug": "circleci"}});
        assert_eq!(
            CheckRun::from_check_run(&other),
            Some(CheckRun {
                name: "lint".to_owned(),
                state: RunState::Pending,
                url: Some("https://ci.example/8".to_owned()),
                job: None,
            })
        );
        let status = json!({"context": "deploy", "state": "error", "target_url": ""});
        assert_eq!(
            CheckRun::from_status(&status).map(|run| (run.state, run.url)),
            Some((RunState::Failed, None))
        );
    }

    #[test]
    fn a_failure_decides_the_state_before_running_checks_finish() {
        let checks = |states: &[RunState]| Checks {
            sha: "abc".to_owned(),
            runs: states.iter().copied().map(run).collect(),
        };
        assert_eq!(checks(&[]).state(), CheckState::None);
        assert_eq!(
            checks(&[RunState::Passed, RunState::Running, RunState::Failed]).state(),
            CheckState::Failing
        );
        assert_eq!(
            checks(&[RunState::Passed, RunState::Pending]).state(),
            CheckState::Pending
        );
        assert_eq!(
            checks(&[RunState::Passed, RunState::Cancelled]).state(),
            CheckState::Cancelled
        );
        assert_eq!(
            checks(&[RunState::Passed, RunState::Skipped]).state(),
            CheckState::Passing
        );
    }

    #[test]
    fn a_merged_pull_request_is_merged_whatever_its_state() {
        let pull = |state: &str, merged_at: serde_json::Value| {
            PullRequest::from_json(&json!({"number": 3, "title": "t", "html_url": "u",
                "state": state, "merged_at": merged_at, "draft": false,
                "base": {"ref": "main"}, "head": {"ref": "topic", "sha": "abc"}}))
            .map(|pull| pull.state)
        };
        assert_eq!(pull("open", json!(null)), Some(PullState::Open));
        assert_eq!(pull("closed", json!(null)), Some(PullState::Closed));
        assert_eq!(
            pull("closed", json!("2026-01-01T00:00:00Z")),
            Some(PullState::Merged)
        );
    }
}
