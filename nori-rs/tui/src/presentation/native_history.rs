//! Views of Nori's read-only git tools, `git_diff` and `git_log`: what changed, file by file,
//! and the recent commits. See [`super::native_tool`] for how calls are recognized.

use super::native_tool::DetailLine;
use super::native_tool::FILES_SHOWN;
use super::native_tool::GIT;
use super::native_tool::NativeView;
use super::native_tool::Part;
use super::native_tool::Tone;
use super::native_tool::part;

const DIFF: &str = "±";
/// Commits a log lists before it says how many more there are.
const COMMITS_SHOWN: usize = 6;

/// `git_diff`: each file with its added and removed line counts. `result` is the diff text.
pub(super) fn diff_view(active: bool, paths: &[String], result: Option<&str>) -> NativeView {
    let scope = match paths {
        [] => String::new(),
        [path] => path.clone(),
        [first, rest @ ..] => format!("{first} +{}", rest.len()),
    };
    let mut header = vec![part(
        if active { "Reading the diff" } else { "Diff" },
        Tone::Strong,
    )];
    if !scope.is_empty() {
        header.push(part("  ", Tone::Plain));
        header.push(part(scope, Tone::Dim));
    }
    let Some(result) = result.filter(|_| !active) else {
        return NativeView::new(DIFF, header, Vec::new());
    };
    let files = file_stats(result);
    if files.is_empty() {
        return NativeView::new(
            DIFF,
            header,
            vec![DetailLine::text("✓ no changes", Tone::Added)],
        );
    }
    let (added, removed) = files
        .iter()
        .fold((0, 0), |(a, r), file| (a + file.added, r + file.removed));
    header.push(part("  ", Tone::Plain));
    header.extend(counts(added, removed));
    let mut detail: Vec<DetailLine> = files
        .iter()
        .take(FILES_SHOWN)
        .map(|file| {
            let mut line = DetailLine::file(None, file.path.clone());
            line.suffix.push(part("  ", Tone::Plain));
            line.suffix.extend(counts(file.added, file.removed));
            line
        })
        .collect();
    if files.len() > FILES_SHOWN {
        detail.push(DetailLine::text(
            format!("… {} more files", files.len() - FILES_SHOWN),
            Tone::Dim,
        ));
    }
    NativeView::new(DIFF, header, detail)
}

/// `git_log`: the branch, then one line per commit with its short id.
pub(super) fn log_view(active: bool, json: Option<&serde_json::Value>) -> NativeView {
    let mut header = vec![part(
        if active {
            "Reading the history"
        } else {
            "History"
        },
        Tone::Strong,
    )];
    let Some(json) = json.filter(|_| !active) else {
        return NativeView::new(GIT, header, Vec::new());
    };
    if let Some(branch) = json.get("branch").and_then(serde_json::Value::as_str) {
        header.push(part("  ", Tone::Plain));
        header.push(part(branch, Tone::Accent));
    }
    let commits = json
        .get("commits")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    if commits.is_empty() {
        return NativeView::new(
            GIT,
            header,
            vec![DetailLine::text("no commits yet", Tone::Dim)],
        );
    }
    let mut detail: Vec<DetailLine> = commits
        .iter()
        .take(COMMITS_SHOWN)
        .map(|commit| {
            let text = |key: &str| {
                commit
                    .get(key)
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
            };
            let mut line =
                DetailLine::text(text("id").chars().take(7).collect::<String>(), Tone::Accent);
            line.suffix.push(part(" ", Tone::Plain));
            line.suffix.push(part(text("summary"), Tone::Plain));
            line
        })
        .collect();
    if commits.len() > COMMITS_SHOWN {
        detail.push(DetailLine::text(
            format!("… {} more", commits.len() - COMMITS_SHOWN),
            Tone::Dim,
        ));
    }
    NativeView::new(GIT, header, detail)
}

fn counts(added: i64, removed: i64) -> Vec<Part> {
    vec![
        part(format!("+{added}"), Tone::Added),
        part(" ", Tone::Plain),
        part(format!("−{removed}"), Tone::Deleted),
    ]
}

#[derive(Debug, PartialEq, Eq)]
struct FileStat {
    path: String,
    added: i64,
    removed: i64,
}

/// Per-file line counts of a unified diff, in the order the files appear.
fn file_stats(diff: &str) -> Vec<FileStat> {
    let mut files: Vec<FileStat> = Vec::new();
    for line in diff.lines() {
        if let Some(header) = line.strip_prefix("diff --git a/") {
            files.push(FileStat {
                path: header.split(" b/").next().unwrap_or(header).to_owned(),
                added: 0,
                removed: 0,
            });
        } else if line.starts_with("+++") || line.starts_with("---") {
            // File headers, not content.
        } else if let Some(file) = files.last_mut() {
            if line.starts_with('+') {
                file.added += 1;
            } else if line.starts_with('-') {
                file.removed += 1;
            }
        }
    }
    files
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::*;

    const DIFF_TEXT: &str = "diff --git a/src/a.rs b/src/a.rs\nindex 1..2 100644\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,2 +1,3 @@\n-old\n+new\n+more\n ctx\ndiff --git a/b.txt b/b.txt\nnew file mode 100644\n--- /dev/null\n+++ b/b.txt\n@@ -0,0 +1 @@\n+b\n";

    #[test]
    fn a_diff_counts_lines_per_file() {
        assert_eq!(
            file_stats(DIFF_TEXT),
            vec![
                FileStat {
                    path: "src/a.rs".to_owned(),
                    added: 2,
                    removed: 1
                },
                FileStat {
                    path: "b.txt".to_owned(),
                    added: 1,
                    removed: 0
                },
            ]
        );
    }

    #[test]
    fn a_finished_diff_shows_totals_and_files() {
        let view = diff_view(false, &[], Some(DIFF_TEXT));
        assert_eq!(view.header_text(), "Diff  +3 −1");
        let lines: Vec<String> = view
            .detail
            .iter()
            .map(|line| {
                let suffix: String = line.suffix.iter().map(|part| part.text.as_str()).collect();
                format!("{}{suffix}", line.text)
            })
            .collect();
        assert_eq!(lines, vec!["src/a.rs  +2 −1", "b.txt  +1 −0"]);
        assert_eq!(
            diff_view(false, &["x".to_owned()], Some("no changes")).header_text(),
            "Diff  x"
        );
    }

    #[test]
    fn a_log_lists_short_ids_and_summaries() {
        let json = json!({"branch": "main", "commits": [
            {"id": "0123456789abcdef", "summary": "feat: one"},
            {"id": "fedcba9876543210", "summary": "fix: two"},
        ]});
        let view = log_view(false, Some(&json));
        assert_eq!(view.header_text(), "History  main");
        assert_eq!(
            view.detail
                .iter()
                .map(|line| format!(
                    "{}{}",
                    line.text,
                    line.suffix
                        .iter()
                        .map(|p| p.text.as_str())
                        .collect::<String>()
                ))
                .collect::<Vec<_>>(),
            vec!["0123456 feat: one", "fedcba9 fix: two"]
        );
        assert_eq!(log_view(true, None).header_text(), "Reading the history");
    }
}
