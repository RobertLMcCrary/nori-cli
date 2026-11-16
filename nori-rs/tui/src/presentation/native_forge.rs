//! Views of Nori's forge tools: how the branch's checks stand (`ci_status`), and the pull request
//! an agent opened or updated (`pull_request`). See [`super::native_tool`] for how calls are
//! recognized.

use serde_json::Value;

use super::native_tool::DetailLine;
use super::native_tool::NativeView;
use super::native_tool::Part;
use super::native_tool::Tone;
use super::native_tool::part;

pub(super) const CI: &str = "◉";
pub(super) const PULL: &str = "⇡";
/// Checks a CI view names before it says how many more there are.
const CHECKS_SHOWN: usize = 6;

/// `ci_status`: the branch and its checks in one header, then its pull request and each check
/// that failed or has not finished.
pub(super) fn ci_view(active: bool, json: Option<&Value>) -> NativeView {
    let mut header = vec![part(
        if active { "Checking CI" } else { "CI" },
        Tone::Strong,
    )];
    let Some(json) = json.filter(|_| !active) else {
        return NativeView::new(CI, header, Vec::new());
    };
    if let Some(branch) = json["branch"].as_str() {
        header.push(part("  ", Tone::Plain));
        header.push(part(branch, Tone::Accent));
    }
    let mut detail = Vec::new();
    if let Some(pull) = json.get("pull_request").filter(|pull| !pull.is_null()) {
        detail.push(pull_line(pull));
    }
    if json["pushed"].is_null() {
        header.push(part("  ", Tone::Plain));
        header.push(part("not on GitHub", Tone::Dim));
        return NativeView::new(CI, header, detail);
    }
    let checks = &json["checks"];
    let count = |key: &str| checks[key].as_array().map_or(0, Vec::len);
    let passed = checks["passed"].as_i64().unwrap_or(0);
    header.push(part("  ", Tone::Plain));
    match checks["state"].as_str() {
        Some("passing") => header.push(part(format!("✓ {passed} passed"), Tone::Added)),
        Some("failing") => {
            header.push(part(format!("✗ {} failed", count("failed")), Tone::Error));
            header.extend(also_passed(passed));
        }
        Some("pending") => {
            header.push(part(
                format!("◌ {} running", count("running")),
                Tone::Accent,
            ));
            header.extend(also_passed(passed));
        }
        Some("cancelled") => header.push(part("cancelled", Tone::Dim)),
        _ => header.push(part("no checks", Tone::Dim)),
    }
    if json["head_pushed"] == Value::Bool(false) {
        detail.push(DetailLine::text(
            "HEAD has commits CI has not seen",
            Tone::Modified,
        ));
    }
    let runs: Vec<(char, Tone, &Value)> = [
        ("failed", '✗', Tone::Error),
        ("running", '◌', Tone::Accent),
        ("cancelled", '–', Tone::Dim),
    ]
    .into_iter()
    .flat_map(|(key, marker, tone)| {
        checks[key]
            .as_array()
            .into_iter()
            .flatten()
            .map(move |run| (marker, tone, run))
    })
    .collect();
    for (marker, tone, run) in runs.iter().take(CHECKS_SHOWN) {
        let mut line = DetailLine::file(
            Some((*marker, *tone)),
            run["name"].as_str().unwrap_or_default(),
        );
        if let Some(cause) = run["log"].as_str().and_then(first_error) {
            line.suffix.push(part("  ", Tone::Plain));
            line.suffix.push(part(cause, Tone::Dim));
        }
        detail.push(line);
    }
    if runs.len() > CHECKS_SHOWN {
        detail.push(DetailLine::text(
            format!("… {} more", runs.len() - CHECKS_SHOWN),
            Tone::Dim,
        ));
    }
    NativeView::new(CI, header, detail)
}

/// `pull_request`: the pull request opened or updated, or the workflow's word before opening.
pub(super) fn pull_view(active: bool, title: &str, json: Option<&Value>) -> NativeView {
    let Some(json) = json.filter(|_| !active) else {
        return NativeView::new(
            PULL,
            vec![
                part("Opening a pull request", Tone::Strong),
                part("  ", Tone::Plain),
                part(title, Tone::Dim),
            ],
            Vec::new(),
        );
    };
    if json["opened"] == Value::Bool(false) {
        let mut header = vec![part("Before the pull request", Tone::Strong)];
        if let Some(workflow) = json["workflow"].as_str() {
            header.push(part("  ", Tone::Plain));
            header.push(part(format!("workflow \"{workflow}\""), Tone::Dim));
        }
        return NativeView::new(
            PULL,
            header,
            json["before_opening"]
                .as_str()
                .unwrap_or_default()
                .lines()
                .map(|line| DetailLine::text(line, Tone::Plain))
                .collect(),
        );
    }
    let pull = &json["pull_request"];
    let mut header = vec![part(
        if json["created"] == Value::Bool(true) {
            "Opened"
        } else {
            "Updated"
        },
        Tone::Strong,
    )];
    header.push(part(" ", Tone::Plain));
    let line = pull_line(pull);
    header.push(part(line.text, line.tone));
    header.extend(line.suffix);
    let mut detail = vec![DetailLine::text(
        format!(
            "{} → {}",
            pull["head"].as_str().unwrap_or_default(),
            pull["base"].as_str().unwrap_or_default()
        ),
        Tone::Dim,
    )];
    if let Some(url) = pull["url"].as_str() {
        detail.push(DetailLine::text(url, Tone::Accent));
    }
    NativeView::new(PULL, header, detail)
}

/// `#12 Title`, and the state when it is not simply open.
fn pull_line(pull: &Value) -> DetailLine {
    let mut line = DetailLine::text(format!("#{}", pull["number"]), Tone::Accent);
    line.suffix.push(part(" ", Tone::Plain));
    line.suffix.push(part(
        pull["title"].as_str().unwrap_or_default(),
        Tone::Plain,
    ));
    let state = match (pull["state"].as_str(), pull["draft"].as_bool()) {
        (Some("merged"), _) => Some(("merged", Tone::Added)),
        (Some("closed"), _) => Some(("closed", Tone::Dim)),
        (_, Some(true)) => Some(("draft", Tone::Dim)),
        _ => None,
    };
    if let Some((label, tone)) = state {
        line.suffix.push(part("  ", Tone::Plain));
        line.suffix.push(part(label, tone));
    }
    line
}

fn also_passed(passed: i64) -> Vec<Part> {
    if passed == 0 {
        return Vec::new();
    }
    vec![part(format!(" · {passed} passed"), Tone::Dim)]
}

/// The first line of a log excerpt that reads as an error, without the generic exit line.
fn first_error(log: &str) -> Option<&str> {
    log.lines().map(str::trim).find(|line| {
        line.starts_with("error") && !line.starts_with("error: Process completed with exit code")
    })
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::*;

    fn lines(view: &NativeView) -> Vec<String> {
        view.detail
            .iter()
            .map(|line| {
                let suffix: String = line.suffix.iter().map(|part| part.text.as_str()).collect();
                format!(
                    "{}{}{suffix}",
                    line.marker.map_or(String::new(), |(c, _)| format!("{c} ")),
                    line.text
                )
            })
            .collect()
    }

    #[test]
    fn a_failing_branch_names_what_failed_and_why() {
        let json = json!({"repo": "o/r", "branch": "topic", "pushed": "abc", "head_pushed": false,
            "pull_request": {"number": 12, "title": "Speed up start", "state": "open", "draft": true},
            "checks": {"state": "failing", "passed": 5,
                "failed": [{"name": "test", "state": "failed",
                    "log": "Compiling\nerror[E0308]: mismatched types\nerror: Process completed with exit code 101.\n"}],
                "running": [{"name": "build (macos)", "state": "running"}]}});
        let view = ci_view(false, Some(&json));
        assert_eq!(view.header_text(), "CI  topic  ✗ 1 failed · 5 passed");
        assert_eq!(
            lines(&view),
            vec![
                "#12 Speed up start  draft",
                "HEAD has commits CI has not seen",
                "✗ test  error[E0308]: mismatched types",
                "◌ build (macos)",
            ]
        );
    }

    #[test]
    fn an_unpushed_branch_says_so() {
        let json = json!({"branch": "topic", "pushed": null, "pull_request": null, "checks": null});
        assert_eq!(
            ci_view(false, Some(&json)).header_text(),
            "CI  topic  not on GitHub"
        );
        assert_eq!(ci_view(true, None).header_text(), "Checking CI");
    }

    #[test]
    fn a_held_pull_request_shows_the_workflow_hook() {
        let json = json!({"opened": false, "workflow": "land",
            "before_opening": "Check the description lists every user-visible change."});
        let view = pull_view(false, "t", Some(&json));
        assert_eq!(
            view.header_text(),
            "Before the pull request  workflow \"land\""
        );
        assert_eq!(
            lines(&view),
            vec!["Check the description lists every user-visible change."]
        );
    }
}
