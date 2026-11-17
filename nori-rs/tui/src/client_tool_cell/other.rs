use ratatui::prelude::*;
use ratatui::style::Stylize;
use serde_json::Value;

use crate::presentation::Artifact;
use crate::presentation::TextFormat;
use crate::presentation::ToolPhase;
use crate::presentation::ToolSnapshot;
use crate::render::highlight::highlight_code_to_lines;
use crate::render::highlight::highlight_code_to_lines_for_path;
use crate::wrapping::RtOptions;
use crate::wrapping::word_wrap_lines;

/// Display-only projection; the snapshot and its complete raw evidence remain unchanged.
pub(super) fn render(snapshot: &ToolSnapshot, width: u16, full: bool) -> Vec<Line<'static>> {
    let marker = match snapshot.phase {
        ToolPhase::Failed => "●".red(),
        ToolPhase::Completed => "●".green(),
        ToolPhase::Pending | ToolPhase::PendingApproval | ToolPhase::InProgress => "◌".cyan(),
    };
    let command = match &snapshot.invocation {
        Some(crate::presentation::Invocation::Command { command }) => Some(command.as_str()),
        _ => snapshot
            .raw_input
            .as_ref()
            .and_then(|input| input.get("command"))
            .and_then(Value::as_str),
    };
    let title = if is_goal_lookup(snapshot) {
        "Goal status"
    } else if snapshot.kind == crate::presentation::ToolKind::Execute {
        command.unwrap_or(&snapshot.title)
    } else {
        &snapshot.title
    }
    .lines()
    .next()
    .unwrap_or("Tool")
    .trim();
    let title = if title.is_empty() { "Tool" } else { title };
    let title = if snapshot.phase == ToolPhase::Completed && title.starts_with("Running ") {
        format!("Ran {}", &title[8..])
    } else if snapshot.kind == crate::presentation::ToolKind::Execute
        && !is_goal_lookup(snapshot)
        && !title.starts_with("Ran ")
        && !title.starts_with("Running ")
    {
        let verb = if matches!(
            snapshot.phase,
            ToolPhase::Pending | ToolPhase::PendingApproval | ToolPhase::InProgress
        ) {
            "Running"
        } else {
            "Ran"
        };
        format!("{verb} {title}")
    } else {
        title.to_owned()
    };
    let title = clean(&title);
    let mut lines = vec![Line::from(vec![marker, " ".into(), title.bold()])];
    if full {
        append_evidence(&mut lines, snapshot, width);
    } else {
        let summary = snapshot
            .raw_output
            .as_ref()
            .and_then(|value| {
                if let Some(text) = value.as_str() {
                    let text = clean(text);
                    match serde_json::from_str::<Value>(&text) {
                        Ok(value) => structured_summary(&value),
                        Err(_) => Some(text),
                    }
                } else {
                    structured_summary(value)
                }
            })
            .or_else(|| {
                if matches!(
                    snapshot.phase,
                    ToolPhase::Pending | ToolPhase::PendingApproval | ToolPhase::InProgress
                ) {
                    return None;
                }
                snapshot
                    .artifacts
                    .iter()
                    .find_map(|artifact| match artifact {
                        Artifact::Text { text, .. } => {
                            let text = clean(text);
                            match serde_json::from_str::<Value>(&text) {
                                Ok(value) => structured_summary(&value),
                                Err(_) => text
                                    .lines()
                                    .find(|line| {
                                        !line.trim().is_empty()
                                            && !line.trim_start().starts_with("```")
                                    })
                                    .map(str::to_owned),
                            }
                        }
                        Artifact::Diff(_) => None,
                    })
            })
            .or_else(|| {
                snapshot
                    .locations
                    .first()
                    .map(|location| location.path.display().to_string())
            });
        if let Some(summary) = summary.filter(|text| serde_json::from_str::<Value>(text).is_err())
            && let Some(first) = summary.lines().find(|line| !line.trim().is_empty())
        {
            let shortened: String = clean(first).chars().take(100).collect();
            lines.push(Line::from(vec![
                "  └ ".dim(),
                if snapshot.phase == ToolPhase::Failed {
                    shortened.red()
                } else {
                    shortened.dim()
                },
            ]));
        }
    }
    lines
}

/// Recognize only a complete, canonical single-call wrapper; never infer from a substring.
fn is_goal_lookup(snapshot: &ToolSnapshot) -> bool {
    if !snapshot.title.eq_ignore_ascii_case("execute") {
        return false;
    }
    let Some(code) = snapshot
        .raw_input
        .as_ref()
        .and_then(|value| value.get("code"))
        .and_then(Value::as_str)
    else {
        return false;
    };
    let mut compact = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut whitespace = false;
    for character in code.chars() {
        if let Some(delimiter) = quote {
            compact.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == delimiter {
                quote = None;
            }
        } else if character.is_whitespace() {
            whitespace = true;
        } else {
            if whitespace
                && compact
                    .chars()
                    .last()
                    .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
                && (character.is_ascii_alphanumeric() || character == '_')
            {
                compact.push(' ');
            }
            whitespace = false;
            if matches!(character, '\"' | '\'') {
                quote = Some(character);
            }
            compact.push(character);
        }
    }
    let compact = compact.strip_suffix(';').unwrap_or(&compact);
    matches!(
        compact,
        "const r=await tools[\"nori-client\"].get_goal();return r"
            | "const r=await tools['nori-client'].get_goal();return r"
    )
}

fn structured_summary(value: &Value) -> Option<String> {
    value
        .get("error")
        .and_then(|error| {
            error
                .as_str()
                .or_else(|| error.get("message").and_then(Value::as_str))
        })
        .or_else(|| value.get("message").and_then(Value::as_str))
        .map(str::to_owned)
        .or_else(|| {
            value.get("goal").and_then(|goal| {
                if goal.is_null() {
                    Some("No active goal".to_owned())
                } else {
                    goal.get("objective")
                        .and_then(Value::as_str)
                        .map(|objective| {
                            goal.get("status").and_then(Value::as_str).map_or_else(
                                || objective.to_owned(),
                                |status| format!("{objective} · {status}"),
                            )
                        })
                }
            })
        })
        .or_else(|| {
            value
                .get("success")
                .and_then(Value::as_bool)
                .map(|ok| if ok { "Succeeded" } else { "Failed" }.to_owned())
        })
}

pub(super) fn append_evidence(lines: &mut Vec<Line<'static>>, snapshot: &ToolSnapshot, width: u16) {
    let source_json = (snapshot.kind == crate::presentation::ToolKind::Read
        || snapshot.title.eq_ignore_ascii_case("read"))
        && snapshot.raw_output.is_none()
        && snapshot
            .raw_input
            .as_ref()
            .and_then(|v| v.get("path").or_else(|| v.get("file_path")))
            .and_then(Value::as_str)
            .or_else(|| {
                snapshot
                    .locations
                    .first()
                    .and_then(|location| location.path.to_str())
            })
            .or_else(|| match &snapshot.invocation {
                Some(crate::presentation::Invocation::Read { path }) => path.to_str(),
                _ => None,
            })
            .is_some_and(|path| path.ends_with(".json"));
    let parsed_output = snapshot
        .artifacts
        .iter()
        .find_map(|artifact| match artifact {
            Artifact::Text { text, .. } if !source_json => {
                serde_json::from_str::<Value>(&clean(text)).ok()
            }
            Artifact::Text { .. } => None,
            Artifact::Diff(_) => None,
        });
    let parsed_raw = snapshot
        .raw_output
        .as_ref()
        .and_then(Value::as_str)
        .and_then(|text| serde_json::from_str::<Value>(&clean(text)).ok())
        .filter(|value| value.is_object() || value.is_array());
    let output = parsed_raw
        .as_ref()
        .or(snapshot.raw_output.as_ref())
        .or(parsed_output.as_ref());
    let path = snapshot
        .raw_input
        .as_ref()
        .and_then(|value| value.get("path").or_else(|| value.get("file_path")))
        .and_then(Value::as_str)
        .or_else(|| {
            output
                .and_then(|value| value.get("path").or_else(|| value.get("file_path")))
                .and_then(Value::as_str)
        })
        .or_else(|| match &snapshot.invocation {
            Some(crate::presentation::Invocation::Read { path }) => path.to_str(),
            _ => None,
        })
        .or_else(|| {
            snapshot
                .locations
                .first()
                .and_then(|location| location.path.to_str())
        });
    let is_read = snapshot.kind == crate::presentation::ToolKind::Read
        || snapshot.title.eq_ignore_ascii_case("read");
    if snapshot.raw_input.is_none() && !is_read {
        match &snapshot.invocation {
            Some(crate::presentation::Invocation::Read { path }) => {
                property(lines, "Path", &path.display().to_string(), 0, width)
            }
            Some(crate::presentation::Invocation::Command { command }) => append_wrapped(
                lines,
                highlight_code_to_lines(&clean(command), "bash"),
                0,
                width,
            ),
            Some(invocation) => {
                if let Ok(mut value) = serde_json::to_value(invocation) {
                    if let Some(object) = value.as_object_mut() {
                        object.remove("invocation_type");
                    }
                    tree(lines, None, &value, 0, width, path);
                }
            }
            None => {}
        }
    }
    if is_read && path.is_some() {
        let path = path.unwrap_or_default();
        lines.push(Line::from(vec![
            "  ◇ ".dim(),
            clean(path).cyan().underlined(),
        ]));
        if let Some(input) = &snapshot.raw_input {
            let mut remaining = input.clone();
            if let Some(object) = remaining.as_object_mut() {
                object.remove("path");
                object.remove("file_path");
            }
            if remaining
                .as_object()
                .is_none_or(|object| !object.is_empty())
            {
                tree(lines, None, &remaining, 0, width, Some(path));
            }
        }
        if let Some(output) = output {
            let mut remaining = output.clone();
            if let Some(object) = remaining.as_object_mut() {
                let success = object.remove("success").and_then(|v| v.as_bool());
                let content = object.remove("lines");
                let count = content.as_ref().and_then(|v| {
                    v.as_i64()
                        .map(|n| n.to_string())
                        .or_else(|| v.as_array().map(|a| a.len().to_string()))
                });
                if let Some(count) = count {
                    let outcome = if success == Some(false) {
                        "read failed"
                    } else {
                        "lines read"
                    };
                    property(lines, "", &format!("{count} {outcome}"), 0, width);
                } else if let Some(success) = success {
                    property(
                        lines,
                        "",
                        if success {
                            "Read successfully"
                        } else {
                            "Read failed"
                        },
                        0,
                        width,
                    );
                }
                match content {
                    Some(Value::Array(items)) if items.iter().all(Value::is_string) => {
                        let text = items
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join("\n");
                        append_wrapped(
                            lines,
                            highlight_code_to_lines_for_path(
                                &clean(&text),
                                std::path::Path::new(path),
                            ),
                            0,
                            width,
                        );
                    }
                    Some(Value::Number(_)) | None => {}
                    Some(value) => tree(lines, Some("lines"), &value, 0, width, Some(path)),
                }
                if let Some(content) = object.remove("content") {
                    if let Some(text) = content.as_str() {
                        append_wrapped(
                            lines,
                            highlight_code_to_lines_for_path(
                                &clean(text),
                                std::path::Path::new(path),
                            ),
                            0,
                            width,
                        );
                    } else {
                        tree(lines, Some("content"), &content, 0, width, Some(path));
                    }
                }
                if object.get("path").and_then(Value::as_str) == Some(path) {
                    object.remove("path");
                }
                if object.get("file_path").and_then(Value::as_str) == Some(path) {
                    object.remove("file_path");
                }
            }
            if remaining
                .as_object()
                .is_none_or(|object| !object.is_empty())
            {
                tree(lines, None, &remaining, 0, width, Some(path));
            }
        }
    } else {
        if let Some(value) = &snapshot.raw_input {
            tree(lines, None, value, 0, width, path);
        }
        if let Some(value) = output {
            tree(lines, None, value, 0, width, path);
        }
    }
    for artifact in &snapshot.artifacts {
        match artifact {
            Artifact::Text { text, format } => {
                if source_json {
                    append_wrapped(
                        lines,
                        highlight_code_to_lines(&clean(text), "json"),
                        0,
                        width,
                    );
                    continue;
                }
                if output.is_some_and(|value| {
                    value.as_str() == Some(text.as_str())
                        || serde_json::from_str::<Value>(&clean(text)).ok().as_ref() == Some(value)
                }) {
                    continue;
                }
                if let Ok(value) = serde_json::from_str::<Value>(&clean(text)) {
                    tree(lines, None, &value, 0, width, path);
                } else {
                    append_text(lines, text, width, *format == TextFormat::Markdown);
                }
            }
            Artifact::Diff(_) => {} // The existing diff renderer owns the visual diff.
        }
    }
}

fn tree(
    lines: &mut Vec<Line<'static>>,
    key: Option<&str>,
    value: &Value,
    depth: usize,
    width: u16,
    path: Option<&str>,
) {
    match value {
        Value::Object(map) => {
            if let Some(key) = key {
                property(lines, &humanize(key), "", depth, width);
            }
            let next = depth + usize::from(key.is_some());
            if map.is_empty() {
                property(lines, "Empty", "", next, width);
            }
            for (name, child) in map {
                if (name == "blob"
                    || (matches!(name.as_str(), "data" | "base64")
                        && (map
                            .get("type")
                            .and_then(Value::as_str)
                            .is_some_and(|kind| matches!(kind, "image" | "audio" | "resource"))
                            || map.get("mimeType").and_then(Value::as_str).is_some())))
                    && child.as_str().is_some()
                {
                    let bytes = child.as_str().map_or(0, str::len);
                    property(
                        lines,
                        &humanize(name),
                        &format!("▧ attachment · {bytes} encoded characters"),
                        next,
                        width,
                    );
                } else {
                    tree(lines, Some(name), child, next, width, path);
                }
            }
        }
        Value::Array(items) => {
            if matches!(key, Some("lines")) && items.iter().all(Value::is_string) {
                property(lines, "Lines", &items.len().to_string(), depth, width);
                let text = items
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("\n");
                let rendered = path
                    .map(|p| {
                        highlight_code_to_lines_for_path(&clean(&text), std::path::Path::new(p))
                    })
                    .unwrap_or_else(|| {
                        text.lines()
                            .map(|line| Line::from(line.to_owned()))
                            .collect()
                    });
                append_wrapped(lines, rendered, depth, width);
            } else {
                if let Some(key) = key {
                    property(
                        lines,
                        &humanize(key),
                        &format!("{} items", items.len()),
                        depth,
                        width,
                    );
                }
                for (index, item) in items.iter().enumerate() {
                    tree(
                        lines,
                        Some(&format!("{}", index + 1)),
                        item,
                        depth + usize::from(key.is_some()),
                        width,
                        path,
                    );
                }
            }
        }
        Value::String(text) if depth == 0 && matches!(key, Some("code" | "command")) => {
            let language = if key == Some("code") {
                "javascript"
            } else {
                "bash"
            };
            append_wrapped(
                lines,
                highlight_code_to_lines(&clean(text), language),
                depth,
                width,
            );
        }
        Value::String(text) => {
            if !matches!(key, Some("content" | "source"))
                && let Ok(nested) = serde_json::from_str::<Value>(&clean(text))
                && (nested.is_object() || nested.is_array())
            {
                tree(lines, key, &nested, depth, width, path);
            } else if text.contains('\n') || text.contains("```") {
                if let Some(key) = key {
                    property(lines, &humanize(key), "", depth, width);
                }
                append_text(lines, text, width, text.contains("```"));
            } else {
                property(
                    lines,
                    &key.map(humanize).unwrap_or_default(),
                    text,
                    depth,
                    width,
                );
            }
        }
        Value::Bool(value) => property(
            lines,
            &key.map(humanize).unwrap_or_default(),
            if *value { "Yes" } else { "No" },
            depth,
            width,
        ),
        Value::Null if key == Some("goal") => property(lines, "", "No active goal", depth, width),
        Value::Null => property(
            lines,
            &key.map(humanize).unwrap_or_default(),
            "None",
            depth,
            width,
        ),
        Value::Number(number) => property(
            lines,
            &key.map(humanize).unwrap_or_default(),
            &number.to_string(),
            depth,
            width,
        ),
    }
}

fn clean(text: &str) -> String {
    codex_ansi_escape::ansi_escape(text)
        .lines
        .into_iter()
        .map(|line| {
            line.spans
                .into_iter()
                .map(|span| span.content.into_owned())
                .collect::<String>()
                .chars()
                .filter(|c| !c.is_control() || *c == '\t')
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn humanize(key: &str) -> String {
    let mut text = key.replace(['_', '-'], " ");
    if let Some(first) = text.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    text
}

fn property(lines: &mut Vec<Line<'static>>, name: &str, value: &str, depth: usize, width: u16) {
    let indent = format!("  {}", "  ".repeat(depth));
    let rendered = Line::from(vec![
        indent.into(),
        clean(name).cyan(),
        if name.is_empty() || value.is_empty() {
            "".into()
        } else {
            "  ".into()
        },
        clean(value).into(),
    ]);
    lines.extend(word_wrap_lines(
        [rendered],
        RtOptions::new(usize::from(width).max(1)),
    ));
}

fn append_text(lines: &mut Vec<Line<'static>>, text: &str, width: u16, markdown: bool) {
    let cleaned = clean(text);
    let text = cleaned.as_str();
    let rendered = if markdown {
        crate::markdown_render::render_markdown_text_with_width(
            text,
            Some(usize::from(width.saturating_sub(4)).max(1)),
        )
        .lines
    } else {
        text.split('\n')
            .map(|line| Line::from(line.to_owned()))
            .collect()
    };
    append_wrapped(lines, rendered, 0, width);
}

fn append_wrapped(
    lines: &mut Vec<Line<'static>>,
    rendered: Vec<Line<'static>>,
    depth: usize,
    width: u16,
) {
    let prefix = format!("  {}│ ", "  ".repeat(depth));
    for line in word_wrap_lines(
        &rendered,
        RtOptions::new(
            usize::from(width)
                .saturating_sub(depth.saturating_mul(2) + 4)
                .max(1),
        ),
    ) {
        let mut spans = vec![prefix.clone().dim()];
        spans.extend(line.spans);
        lines.push(Line::from(spans));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::ToolKind;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn fixture() -> ToolSnapshot {
        ToolSnapshot {
            call_id: "call".into(),
            title: "execute".into(),
            kind: ToolKind::Other("other".into()),
            phase: ToolPhase::Completed,
            locations: vec![],
            invocation: None,
            artifacts: vec![],
            raw_input: Some(json!({"code": "const r = await tools.get_goal();\nreturn r;"})),
            raw_output: Some(json!({"goal": null})),
            owner_request_id: None,
        }
    }

    fn text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn compact_and_expanded_preserve_evidence_at_both_widths() {
        let snapshot = fixture();
        for width in [30, 80] {
            let compact = render(&snapshot, width, false);
            assert_eq!(text(&compact), "● execute\n  └ No active goal");
            let expanded = render(&snapshot, width, true);
            let display = text(&expanded);
            assert!(display.contains("get_goal"));
            assert!(display.contains("No active goal"));
            assert!(
                expanded
                    .iter()
                    .flat_map(|line| &line.spans)
                    .any(|span| span.style.fg.is_some())
            );
        }
        assert_eq!(snapshot.raw_output, Some(json!({"goal": null})));
        assert_eq!(snapshot.raw_input, fixture().raw_input);
    }

    #[test]
    fn structured_tool_snapshots() {
        let snapshot = fixture();
        let cell = super::super::ClientToolCell::new(
            snapshot.clone(),
            std::path::PathBuf::from("/"),
            false,
        );
        let project = |cell: &super::super::ClientToolCell, width, expanded| {
            let mut projection = crate::chatwidget::tool_projection::ToolProjection::default();
            let ids = if expanded {
                std::collections::HashSet::from(["call".to_owned()])
            } else {
                std::collections::HashSet::new()
            };
            projection.push(cell, width, &ids);
            text(&projection.lines)
        };
        insta::assert_snapshot!("tool_compact_narrow", project(&cell, 30, false));
        insta::assert_snapshot!("tool_compact_wide", project(&cell, 80, false));
        insta::assert_snapshot!("tool_expanded_narrow", project(&cell, 30, true));
        insta::assert_snapshot!("tool_expanded_wide", project(&cell, 80, true));
        let mut failure = snapshot;
        failure.phase = ToolPhase::Failed;
        failure.raw_output = Some(json!({"error": "permission denied\nmore details"}));
        let failure_cell =
            super::super::ClientToolCell::new(failure, std::path::PathBuf::from("/"), false);
        insta::assert_snapshot!("tool_error_compact", project(&failure_cell, 30, false));
        insta::assert_snapshot!("tool_error_expanded", project(&failure_cell, 80, true));
        let mut diff = fixture();
        diff.artifacts
            .push(Artifact::Diff(crate::presentation::FileChange {
                path: "example.rs".into(),
                old_text: Some("let x = 1;\n".into()),
                new_text: "let x = 2;\n".into(),
            }));
        let diff_cell =
            super::super::ClientToolCell::new(diff, std::path::PathBuf::from("/"), false);
        let diff_display = project(&diff_cell, 80, true);
        assert!(diff_display.contains("-let x = 1;") && diff_display.contains("+let x = 2;"));
        insta::assert_snapshot!("tool_diff_expanded", diff_display);
        let mut read = fixture();
        read.title = "Read".into();
        read.kind = ToolKind::Read;
        read.raw_input = Some(json!({"path": "/project/example.rs"}));
        read.raw_output = Some(
            json!({"success": true, "lines": ["fn main() {", "    println!(\"hello\");", "}"]}),
        );
        let read_cell =
            super::super::ClientToolCell::new(read, std::path::PathBuf::from("/"), false);
        insta::assert_snapshot!("tool_read_expanded", project(&read_cell, 80, true));
        let mut nested = fixture();
        nested.title = "lookup".into();
        nested.raw_input = Some(json!({"filter": {"owner": "Ada", "tags": ["open", "urgent"]}}));
        nested.raw_output = Some(json!({"records": [{"name": "first", "count": 2}]}));
        let nested_cell =
            super::super::ClientToolCell::new(nested, std::path::PathBuf::from("/"), false);
        let nested_display = project(&nested_cell, 80, true);
        assert!(!nested_display.contains('{') && !nested_display.contains('}'));
        assert!(
            nested_display.contains("Ada")
                && nested_display.contains("urgent")
                && nested_display.contains("first")
        );
        insta::assert_snapshot!("tool_nested_expanded", nested_display);
    }

    #[test]
    fn code_mode_permission_error_is_semantic_and_preserves_input() {
        let mut snapshot = fixture();
        snapshot.raw_input =
            Some(json!({"code": "const r=await tools[\"nori-client\"].get_goal();return r;"}));
        snapshot.raw_output = Some(
            json!({"error": {"code": "permission_denied", "message": "Permission denied for this tool"}}),
        );
        snapshot.phase = ToolPhase::Failed;
        let cell = super::super::ClientToolCell::new(
            snapshot.clone(),
            std::path::PathBuf::from("/"),
            false,
        );
        let mut projection = crate::chatwidget::tool_projection::ToolProjection::default();
        projection.push(
            &cell,
            80,
            &std::collections::HashSet::from(["call".to_owned()]),
        );
        let rendered = text(&projection.lines);
        assert!(rendered.contains("nori-client") && rendered.contains("Permission denied"));
        assert!(
            !rendered.contains("[completed]")
                && !rendered.contains("\"error\"")
                && !rendered.contains("(Other)")
        );
        assert_eq!(
            snapshot.raw_input,
            Some(json!({"code": "const r=await tools[\"nori-client\"].get_goal();return r;"}))
        );
    }

    #[test]
    fn only_canonical_single_goal_wrapper_gets_goal_label() {
        let mut snapshot = fixture();
        let code = "const r=await tools[\"nori-client\"].get_goal();return r;";
        snapshot.raw_input = Some(json!({"code": code}));
        let original = snapshot.clone();
        let cell = super::super::ClientToolCell::new(
            snapshot.clone(),
            std::path::PathBuf::from("/"),
            false,
        );
        let mut compact = crate::chatwidget::tool_projection::ToolProjection::default();
        compact.push(&cell, 80, &std::collections::HashSet::new());
        let mut expanded = crate::chatwidget::tool_projection::ToolProjection::default();
        expanded.push(
            &cell,
            80,
            &std::collections::HashSet::from(["call".to_owned()]),
        );
        let compact = text(&compact.lines);
        let expanded = text(&expanded.lines);
        assert!(compact.contains("Goal status") && compact.contains("No active goal"));
        assert!(expanded.contains("get_goal") && expanded.contains("No active goal"));
        assert_eq!(snapshot, original);
        insta::assert_snapshot!("goal_wrapper_compact", compact);
        insta::assert_snapshot!("goal_wrapper_expanded", expanded);

        snapshot.raw_input =
            Some(json!({"code": "const r = await tools['nori-client'].get_goal(); return r"}));
        assert!(is_goal_lookup(&snapshot));
        for code in [
            "const r=await tools[\"nori-client\"].get_goal();await tools[\"nori-client\"].update_goal({});return r;",
            "const r=await tools[\"nori - client\"].get_goal();return r;",
            "const r=await tools[\"nori-client\"].get_goal();return r;danger();",
            "con st r=await tools[\"nori-client\"].get_goal();return r;",
        ] {
            snapshot.raw_input = Some(json!({"code": code}));
            assert!(!is_goal_lookup(&snapshot), "{code}");
            assert!(!text(&render(&snapshot, 80, false)).contains("Goal status"));
        }
    }

    #[test]
    fn ansi_shell_output_is_readable_without_control_sequences() {
        let mut snapshot = fixture();
        snapshot.raw_output = Some(json!({"message": "\u{1b}[31mfailed\u{1b}[0m\nnext"}));
        let display = text(&render(&snapshot, 80, true));
        assert!(display.contains("failed") && display.contains("next"));
        assert!(!display.contains('\u{1b}'));
    }

    #[test]
    fn ansi_wrapped_structured_output_and_resources_are_semantic() {
        let mut snapshot = fixture();
        let wrapped = "\u{1b}[32m{\"goal\":null}\u{1b}[0m";
        snapshot.raw_output = Some(json!(wrapped));
        snapshot.artifacts = vec![Artifact::Text {
            text: wrapped.into(),
            format: TextFormat::Plain,
        }];
        let original = snapshot.clone();
        let cell = super::super::ClientToolCell::new(
            snapshot.clone(),
            std::path::PathBuf::from("/"),
            false,
        );
        let mut compact = crate::chatwidget::tool_projection::ToolProjection::default();
        compact.push(&cell, 80, &std::collections::HashSet::new());
        let mut expanded = crate::chatwidget::tool_projection::ToolProjection::default();
        expanded.push(
            &cell,
            80,
            &std::collections::HashSet::from(["call".to_owned()]),
        );
        let compact_text = text(&compact.lines);
        let expanded_text = text(&expanded.lines);
        assert!(compact_text.contains("No active goal"));
        assert_eq!(expanded_text.matches("No active goal").count(), 1);
        assert!(!expanded_text.contains('\u{1b}') && !expanded_text.contains("{\"goal\""));
        assert_eq!(snapshot, original);
        insta::assert_snapshot!("tool_ansi_goal_compact", compact_text);
        insta::assert_snapshot!("tool_ansi_goal_expanded", expanded_text);

        snapshot.phase = ToolPhase::Failed;
        let error = "\u{1b}[31m{\"error\":{\"code\":\"permission_denied\",\"message\":\"Permission denied\\nTry again\"}}\u{1b}[0m";
        snapshot.raw_output = Some(json!(error));
        snapshot.artifacts.clear();
        let original = snapshot.clone();
        let cell = super::super::ClientToolCell::new(
            snapshot.clone(),
            std::path::PathBuf::from("/"),
            false,
        );
        let mut projection = crate::chatwidget::tool_projection::ToolProjection::default();
        projection.push(
            &cell,
            80,
            &std::collections::HashSet::from(["call".to_owned()]),
        );
        let display = text(&projection.lines);
        assert!(display.contains("Permission denied") && display.contains("Try again"));
        assert!(!display.contains('\u{1b}') && !display.contains("{\"error\""));
        assert_eq!(snapshot, original);
        insta::assert_snapshot!("tool_ansi_nested_error", display);

        snapshot.phase = ToolPhase::Completed;
        snapshot.raw_output = Some(
            json!({"resource": {"uri": "file:///report.pdf", "mimeType": "application/pdf", "blob": "YWJjZGVm"}}),
        );
        let original = snapshot.clone();
        let cell = super::super::ClientToolCell::new(
            snapshot.clone(),
            std::path::PathBuf::from("/"),
            false,
        );
        let mut projection = crate::chatwidget::tool_projection::ToolProjection::default();
        projection.push(
            &cell,
            80,
            &std::collections::HashSet::from(["call".to_owned()]),
        );
        let display = text(&projection.lines);
        assert!(
            display.contains("application/pdf")
                && display.contains("file:///report.pdf")
                && display.contains("8 encoded characters")
        );
        assert!(!display.contains("YWJjZGVm"));
        assert_eq!(snapshot, original);
        insta::assert_snapshot!("tool_pdf_resource", display);
    }

    #[test]
    fn json_source_is_rendered_as_code() {
        let mut snapshot = fixture();
        snapshot.kind = ToolKind::Read;
        snapshot.title = "Read".into();
        snapshot.raw_input = Some(json!({"path": "config.json"}));
        snapshot.raw_output = None;
        snapshot.artifacts = vec![Artifact::Text {
            text: "{\n  \"goal\": null\n}".into(),
            format: TextFormat::Plain,
        }];
        let display = text(&render(&snapshot, 80, true));
        assert!(display.contains("\"goal\": null") && display.contains('{'));
    }

    #[test]
    fn failure_stays_compact_and_full_error_is_available() {
        let mut snapshot = fixture();
        snapshot.phase = ToolPhase::Failed;
        let error = format!("permission denied\n{}", "details ".repeat(100));
        snapshot.raw_output = Some(json!({"error": error}));
        assert_eq!(
            text(&render(&snapshot, 30, false)),
            "● execute\n  └ permission denied"
        );
        assert!(text(&render(&snapshot, 80, true)).contains("permission denied"));
    }
}
