//! Nori's own tools, shown as what they did rather than as a tool call.
//!
//! Git and goal tools come from the `nori-client` MCP server, so an agent names them its own way
//! (`mcp__nori-client__git_commit`, `nori-client.git_commit`, ...). A call is recognized the way
//! delegation calls are: by the server's name and a tool name in the title. The view built here
//! is plain data (a header and detail lines with a tone each); the cell turns tones into colors.

use serde::Deserialize;
use serde::Serialize;

/// One call to a git or goal tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "call", rename_all = "snake_case")]
pub enum NativeCall {
    GitStatus,
    GitCommit { paths: Vec<String>, message: String },
    GetGoal,
    CreateGoal { objective: String },
    UpdateGoal { status: String },
}

impl NativeCall {
    /// The call `title` names, with its arguments from `input`, when it is one.
    pub(crate) fn recognize(title: &str, input: Option<&serde_json::Value>) -> Option<Self> {
        let lowered = title.to_ascii_lowercase();
        if !lowered.contains("nori-client") && !lowered.contains("nori_client") {
            return None;
        }
        let words = lowered.replace("__", " ");
        let tool = words
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .find(|word| TOOLS.contains(word))?;
        let text = |key: &str| {
            input
                .and_then(|input| input.get(key))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .unwrap_or_default()
        };
        Some(match tool {
            "git_status" => Self::GitStatus,
            "git_commit" => Self::GitCommit {
                paths: input
                    .and_then(|input| input.get("paths"))
                    .and_then(serde_json::Value::as_array)
                    .map(|paths| {
                        paths
                            .iter()
                            .filter_map(serde_json::Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default(),
                message: text("message"),
            },
            "create_goal" => Self::CreateGoal {
                objective: text("objective"),
            },
            "update_goal" => Self::UpdateGoal {
                status: text("status"),
            },
            _ => Self::GetGoal,
        })
    }
}

const TOOLS: [&str; 5] = [
    "git_status",
    "git_commit",
    "get_goal",
    "create_goal",
    "update_goal",
];

/// How a detail line should look.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tone {
    Plain,
    Dim,
    Added,
    Modified,
    Deleted,
    Error,
}

/// One detail line: an optional one-letter marker, then text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DetailLine {
    pub marker: Option<(char, Tone)>,
    pub text: String,
    pub tone: Tone,
}

impl DetailLine {
    fn text(text: impl Into<String>, tone: Tone) -> Self {
        Self {
            marker: None,
            text: text.into(),
            tone,
        }
    }
}

/// What a native call looks like: a header and detail lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeView {
    pub header: String,
    pub detail: Vec<DetailLine>,
}

/// Files a status lists before it says how many more there are.
const FILES_SHOWN: usize = 8;

/// The view of `call`. `result` is the text the tool returned, once it has; `failed` is the
/// tool's own report that it did not work.
pub(crate) fn view(
    call: &NativeCall,
    active: bool,
    failed: bool,
    result: Option<&str>,
) -> NativeView {
    if failed {
        return NativeView {
            header: match call {
                NativeCall::GitStatus => "Could not read git status",
                NativeCall::GitCommit { .. } => "Commit failed",
                _ => "Goal tool failed",
            }
            .to_owned(),
            detail: result
                .and_then(first_line)
                .map(|line| vec![DetailLine::text(line, Tone::Error)])
                .unwrap_or_default(),
        };
    }
    let json = result.and_then(|text| serde_json::from_str::<serde_json::Value>(text.trim()).ok());
    match call {
        NativeCall::GitStatus => status_view(active, json.as_ref()),
        NativeCall::GitCommit { paths, message } => {
            commit_view(active, paths, message, json.as_ref())
        }
        NativeCall::GetGoal => goal_view(
            if active { "Reading the goal" } else { "Goal" },
            json.as_ref()
                .and_then(|json| json.get("goal"))
                .filter(|goal| !goal.is_null()),
        ),
        NativeCall::CreateGoal { objective } => NativeView {
            header: if active { "Setting a goal" } else { "Goal set" }.to_owned(),
            detail: vec![DetailLine::text(objective.clone(), Tone::Dim)],
        },
        NativeCall::UpdateGoal { status } => NativeView {
            header: format!(
                "{} {status}",
                if active {
                    "Marking the goal"
                } else {
                    "Goal marked"
                }
            ),
            detail: Vec::new(),
        },
    }
}

fn first_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

fn status_view(active: bool, json: Option<&serde_json::Value>) -> NativeView {
    let Some(json) = json else {
        return NativeView {
            header: if active {
                "Reading git status"
            } else {
                "Git status"
            }
            .to_owned(),
            detail: Vec::new(),
        };
    };
    let branch = json.get("branch").and_then(serde_json::Value::as_str);
    let changes = json
        .get("changes")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let header = match branch {
        Some(branch) => format!("Git status · {branch}"),
        None => "Git status · detached HEAD".to_owned(),
    };
    if changes.is_empty() {
        return NativeView {
            header,
            detail: vec![DetailLine::text("working tree clean", Tone::Dim)],
        };
    }
    let mut detail = vec![DetailLine::text(
        format!(
            "{} {}",
            changes.len(),
            if changes.len() == 1 {
                "change"
            } else {
                "changes"
            }
        ),
        Tone::Dim,
    )];
    for change in changes.iter().take(FILES_SHOWN) {
        let path = change
            .get("path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let marker = match change.get("state").and_then(serde_json::Value::as_str) {
            Some("added") => ('A', Tone::Added),
            Some("deleted") => ('D', Tone::Deleted),
            _ => ('M', Tone::Modified),
        };
        detail.push(DetailLine {
            marker: Some(marker),
            text: path.to_owned(),
            tone: Tone::Plain,
        });
    }
    if changes.len() > FILES_SHOWN {
        detail.push(DetailLine::text(
            format!("… {} more", changes.len() - FILES_SHOWN),
            Tone::Dim,
        ));
    }
    NativeView { header, detail }
}

fn commit_view(
    active: bool,
    paths: &[String],
    message: &str,
    json: Option<&serde_json::Value>,
) -> NativeView {
    let summary = message.lines().next().unwrap_or_default().trim();
    let files = format!(
        "{} {}",
        paths.len(),
        if paths.len() == 1 { "file" } else { "files" }
    );
    if active || json.is_none() {
        return NativeView {
            header: if summary.is_empty() {
                format!("Committing {files}")
            } else {
                format!("Committing {files} · {summary}")
            },
            detail: Vec::new(),
        };
    }
    let commit = json
        .and_then(|json| json.get("commit"))
        .and_then(serde_json::Value::as_str)
        .map(|id| id.chars().take(7).collect::<String>());
    let remaining = json
        .and_then(|json| json.get("remaining_changes"))
        .and_then(serde_json::Value::as_u64);
    let header = match (&commit, summary.is_empty()) {
        (Some(id), false) => format!("Committed {id} · {summary}"),
        (Some(id), true) => format!("Committed {id}"),
        (None, _) => format!("Committed {files}"),
    };
    let mut detail: Vec<DetailLine> = paths
        .iter()
        .take(FILES_SHOWN)
        .map(|path| DetailLine {
            marker: None,
            text: path.clone(),
            tone: Tone::Plain,
        })
        .collect();
    if paths.len() > FILES_SHOWN {
        detail.push(DetailLine::text(
            format!("… {} more", paths.len() - FILES_SHOWN),
            Tone::Dim,
        ));
    }
    if let Some(remaining) = remaining {
        detail.push(DetailLine::text(
            if remaining == 0 {
                "nothing left uncommitted".to_owned()
            } else {
                format!("{remaining} still uncommitted")
            },
            Tone::Dim,
        ));
    }
    NativeView { header, detail }
}

fn goal_view(header: &str, goal: Option<&serde_json::Value>) -> NativeView {
    let Some(goal) = goal else {
        return NativeView {
            header: format!("{header} · none set"),
            detail: Vec::new(),
        };
    };
    let objective = goal
        .get("objective")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let status = goal
        .get("status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    NativeView {
        header: if status.is_empty() {
            header.to_owned()
        } else {
            format!("{header} · {status}")
        },
        detail: vec![DetailLine::text(objective, Tone::Dim)],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn recognizes_the_tools_however_an_agent_names_them() {
        for title in [
            "mcp__nori-client__git_status",
            "nori-client.git_status",
            "git_status (nori-client MCP Server)",
        ] {
            assert_eq!(
                NativeCall::recognize(title, None),
                Some(NativeCall::GitStatus)
            );
        }
        assert_eq!(
            NativeCall::recognize("git_status", None),
            None,
            "no server, no match"
        );
        assert_eq!(NativeCall::recognize("mcp__other__git_status", None), None);
    }

    #[test]
    fn a_commit_call_carries_its_paths_and_message() {
        let input = serde_json::json!({"paths": ["a.rs", "b.rs"], "message": "feat: x\n\nbody"});
        assert_eq!(
            NativeCall::recognize("mcp__nori-client__git_commit", Some(&input)),
            Some(NativeCall::GitCommit {
                paths: vec!["a.rs".into(), "b.rs".into()],
                message: "feat: x\n\nbody".into(),
            })
        );
    }

    #[test]
    fn a_status_lists_branch_count_and_files() {
        let result = r#"{"branch":"main","head":"abc","changes":[
            {"path":"a.rs","state":"modified"},{"path":"new.rs","state":"added"},{"path":"old.rs","state":"deleted"}]}"#;
        let view = view(&NativeCall::GitStatus, false, false, Some(result));
        assert_eq!(view.header, "Git status · main");
        assert_eq!(view.detail[0], DetailLine::text("3 changes", Tone::Dim));
        assert_eq!(view.detail[1].marker, Some(('M', Tone::Modified)));
        assert_eq!(view.detail[2].marker, Some(('A', Tone::Added)));
        assert_eq!(view.detail[3].marker, Some(('D', Tone::Deleted)));
        assert_eq!(view.detail[3].text, "old.rs");
    }

    #[test]
    fn a_clean_tree_and_a_long_list_both_read_well() {
        let clean = view(
            &NativeCall::GitStatus,
            false,
            false,
            Some(r#"{"branch":"main","changes":[]}"#),
        );
        assert_eq!(
            clean.detail,
            vec![DetailLine::text("working tree clean", Tone::Dim)]
        );

        let many: Vec<String> = (0..11)
            .map(|n| format!(r#"{{"path":"f{n}","state":"added"}}"#))
            .collect();
        let result = format!(r#"{{"branch":"main","changes":[{}]}}"#, many.join(","));
        let long = view(&NativeCall::GitStatus, false, false, Some(&result));
        assert_eq!(
            long.detail.last().map(|l| l.text.as_str()),
            Some("… 3 more")
        );
    }

    #[test]
    fn a_commit_reads_as_the_commit_it_made() {
        let call = NativeCall::GitCommit {
            paths: vec!["math.py".into()],
            message: "feat(math): add subtraction\n\nwhy".into(),
        };
        let pending = view(&call, true, false, None);
        assert_eq!(
            pending.header,
            "Committing 1 file · feat(math): add subtraction"
        );

        let done = view(
            &call,
            false,
            false,
            Some(r#"{"commit":"07a5d0f9c0ffee","remaining_changes":2}"#),
        );
        assert_eq!(
            done.header,
            "Committed 07a5d0f · feat(math): add subtraction"
        );
        assert_eq!(done.detail[0].text, "math.py");
        assert_eq!(
            done.detail.last().map(|l| l.text.as_str()),
            Some("2 still uncommitted")
        );
    }

    #[test]
    fn a_failure_shows_the_tools_own_words() {
        let call = NativeCall::GitCommit {
            paths: vec!["a".into()],
            message: "x".into(),
        };
        let failed = view(
            &call,
            false,
            true,
            Some("none of those paths differ from HEAD"),
        );
        assert_eq!(failed.header, "Commit failed");
        assert_eq!(
            failed.detail,
            vec![DetailLine::text(
                "none of those paths differ from HEAD",
                Tone::Error
            )]
        );
    }

    #[test]
    fn goal_calls_show_the_objective() {
        let got = view(
            &NativeCall::GetGoal,
            false,
            false,
            Some(r#"{"goal":{"objective":"Ship it","status":"active"}}"#),
        );
        assert_eq!(got.header, "Goal · active");
        assert_eq!(got.detail, vec![DetailLine::text("Ship it", Tone::Dim)]);
        assert_eq!(
            view(&NativeCall::GetGoal, false, false, Some(r#"{"goal":null}"#)).header,
            "Goal · none set"
        );
    }
}
