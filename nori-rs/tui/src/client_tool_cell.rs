mod other;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use ratatui::prelude::*;
use ratatui::style::Stylize;
use textwrap::WordSplitter;

use crate::client_event_format::format_edit_tool_header;
use crate::client_event_format::format_tool_kind;
use crate::client_event_format::is_exploring_snapshot;
use crate::client_event_format::relativize_paths_in_text;
use crate::diff_render::create_diff_summary;
use crate::exec_cell::spinner;
use crate::history_cell::HistoryCell;
use crate::render::line_utils::prefix_lines;
use crate::render::line_utils::push_owned_lines;
use crate::wrapping::RtOptions;
use crate::wrapping::word_wrap_line;
use nori_harness::patch::first_line_in_file;

#[derive(Debug, Clone)]
pub(crate) struct ClientToolCell {
    snapshot: crate::presentation::ToolSnapshot,
    exploring_snapshots: Vec<crate::presentation::ToolSnapshot>,
    cwd: PathBuf,
    edit_changes: HashMap<PathBuf, crate::ui_types::FileChange>,
    animations_enabled: bool,
    start_time: Option<Instant>,
}

impl ClientToolCell {
    pub(crate) fn new(
        snapshot: crate::presentation::ToolSnapshot,
        cwd: PathBuf,
        animations_enabled: bool,
    ) -> Self {
        let start_time = if is_active_phase(&snapshot.phase) {
            Some(Instant::now())
        } else {
            None
        };
        let edit_changes = changes_from_snapshot(&snapshot, &cwd);
        Self {
            snapshot,
            exploring_snapshots: Vec::new(),
            cwd,
            edit_changes,
            animations_enabled,
            start_time,
        }
    }

    /// Folded tool projection for the interactive conversation view.
    pub(crate) fn compact_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines = if matches!(
            self.snapshot.kind,
            crate::presentation::ToolKind::Other(_)
                | crate::presentation::ToolKind::Fetch
                | crate::presentation::ToolKind::Think
                | crate::presentation::ToolKind::Execute
        ) && self.snapshot.invocation.as_ref().is_none_or(|invocation| {
            !matches!(
                invocation,
                crate::presentation::Invocation::Native(_)
                    | crate::presentation::Invocation::Delegation(_)
            )
        }) && !self.is_exploring()
        {
            other::render(&self.snapshot, width, false)
        } else {
            self.display_lines(width)
        };
        if !self.is_active() {
            lines.truncate(2);
        }
        if let Some(first) = lines.first_mut() {
            if self.snapshot.kind != crate::presentation::ToolKind::Execute {
                for span in &mut first.spans {
                    span.content = relativize_paths_in_text(&span.content, &self.cwd).into();
                }
            }
            first.spans.insert(0, "▸ ".dim());
        }
        lines
    }

    /// Complete, structured evidence for the shared conversation view.
    pub(crate) fn expanded_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines = self.compact_lines(width);
        lines.truncate(1);
        if let Some(first) = lines.first_mut()
            && let Some(indicator) = first.spans.first_mut()
        {
            *indicator = "▾ ".dim();
        }
        if !self.edit_changes.is_empty() {
            lines.extend(create_diff_summary(
                &self.edit_changes,
                &self.cwd,
                usize::from(width),
            ));
        }
        if self.exploring_snapshots.is_empty() {
            other::append_evidence(&mut lines, &self.snapshot, width);
        } else {
            for snapshot in &self.exploring_snapshots {
                lines.push(Line::from(snapshot.title.clone().bold()));
                other::append_evidence(&mut lines, snapshot, width);
            }
        }
        lines
    }

    pub(crate) fn call_id(&self) -> &str {
        &self.snapshot.call_id
    }

    pub(crate) fn is_active(&self) -> bool {
        is_active_phase(&self.snapshot.phase)
            || self
                .exploring_snapshots
                .iter()
                .any(|snapshot| is_active_phase(&snapshot.phase))
    }

    pub(crate) fn snapshot_kind(&self) -> &crate::presentation::ToolKind {
        &self.snapshot.kind
    }

    /// Whether this is a call to one of Nori's delegated-task tools.
    pub(crate) fn is_delegation(&self) -> bool {
        matches!(
            self.snapshot.invocation,
            Some(crate::presentation::Invocation::Delegation(_))
        )
    }

    /// A call to one of Nori's own tools, which reads as its result once it has one.
    pub(crate) fn is_native(&self) -> bool {
        matches!(
            self.snapshot.invocation,
            Some(crate::presentation::Invocation::Native(_))
        )
    }

    pub(crate) fn is_exploring(&self) -> bool {
        is_exploring_snapshot(&self.snapshot) || !self.exploring_snapshots.is_empty()
    }

    /// Return all call_ids held by this cell's exploring group (for tracking
    /// on flush to prevent re-merging into later cells).
    pub(crate) fn exploring_call_ids(&self) -> Vec<String> {
        self.exploring_snapshots
            .iter()
            .map(|s| s.call_id.clone())
            .collect()
    }

    /// Mark this cell as an exploring cell. The primary snapshot becomes the
    /// first item in the exploring group.
    pub(crate) fn mark_exploring(&mut self) {
        if self.exploring_snapshots.is_empty() {
            self.exploring_snapshots.push(self.snapshot.clone());
        }
    }

    /// Add another exploring snapshot to this cell's group, or update an
    /// existing one if the call_id is already present.
    pub(crate) fn merge_exploring(&mut self, snapshot: crate::presentation::ToolSnapshot) {
        // Update in place if the call_id already exists in the group
        if let Some(existing) = self
            .exploring_snapshots
            .iter_mut()
            .find(|s| s.call_id == snapshot.call_id)
        {
            *existing = snapshot;
            return;
        }
        self.exploring_snapshots.push(snapshot);
    }

    pub(crate) fn apply_snapshot(&mut self, snapshot: crate::presentation::ToolSnapshot) {
        if self.snapshot.call_id != snapshot.call_id {
            return;
        }
        if self.start_time.is_none() && is_active_phase(&snapshot.phase) {
            self.start_time = Some(Instant::now());
        }
        if !is_active_phase(&snapshot.phase) {
            self.start_time = None;
        }
        // Also update the corresponding entry in exploring_snapshots so that
        // tool_call_update events (which carry the real path/query) propagate
        // into the exploring rendering.
        if let Some(existing) = self
            .exploring_snapshots
            .iter_mut()
            .find(|s| s.call_id == snapshot.call_id)
        {
            *existing = snapshot.clone();
        }
        self.edit_changes = changes_from_snapshot(&snapshot, &self.cwd);
        self.snapshot = snapshot;
    }

    pub(crate) fn mark_failed(&mut self) {
        if is_active_phase(&self.snapshot.phase) {
            self.snapshot.phase = crate::presentation::ToolPhase::Failed;
        }
        for snapshot in &mut self.exploring_snapshots {
            if is_active_phase(&snapshot.phase) {
                snapshot.phase = crate::presentation::ToolPhase::Failed;
            }
        }
        self.start_time = None;
    }

    fn render_exploring_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut out: Vec<Line<'static>> = Vec::new();

        // Header: Exploring/Explored with bullet
        out.push(Line::from(vec![
            if self.is_active() {
                spinner(self.start_time, self.animations_enabled)
            } else {
                "\u{2022}".dim() // bullet
            },
            " ".into(),
            if self.is_active() {
                "Exploring".bold()
            } else {
                "Explored".bold()
            },
        ]));

        // Build sub-items from exploring snapshots. When exploring_snapshots
        // is empty (single-snapshot exploring cell), use the primary snapshot.
        let mut sub_items: Vec<Line<'static>> = Vec::new();
        let mut i = 0;
        let single_fallback;
        let snapshots = if self.exploring_snapshots.is_empty() {
            single_fallback = [self.snapshot.clone()];
            &single_fallback[..]
        } else {
            &self.exploring_snapshots[..]
        };
        while i < snapshots.len() {
            let snap = &snapshots[i];

            // Group consecutive reads by filename
            if snap.kind == crate::presentation::ToolKind::Read {
                let mut names: Vec<String> = Vec::new();
                if let Some(crate::presentation::Invocation::Read { path }) = &snap.invocation {
                    names.push(read_display_name(path));
                } else {
                    names.push(relativize_paths_in_text(&snap.title, &self.cwd));
                }
                let mut j = i + 1;
                while j < snapshots.len()
                    && snapshots[j].kind == crate::presentation::ToolKind::Read
                {
                    let next = &snapshots[j];
                    if let Some(crate::presentation::Invocation::Read { path }) = &next.invocation {
                        names.push(read_display_name(path));
                    } else {
                        names.push(relativize_paths_in_text(&next.title, &self.cwd));
                    }
                    j += 1;
                }
                i = j;

                // Build "Read file1.rs, file2.rs" line
                let mut spans: Vec<Span<'static>> = vec!["Read".cyan(), " ".into()];
                for (idx, path) in names.into_iter().enumerate() {
                    if idx > 0 {
                        spans.push(", ".dim());
                    }
                    spans.push(path.into());
                }
                sub_items.push(Line::from(spans));
                continue;
            }

            // Search sub-item
            if snap.kind == crate::presentation::ToolKind::Search {
                if let Some(crate::presentation::Invocation::Search { query, path }) =
                    &snap.invocation
                {
                    let mut spans: Vec<Span<'static>> = vec!["Search".cyan(), " ".into()];
                    if let Some(q) = query {
                        spans.push(q.clone().into());
                    }
                    if let Some(p) = path {
                        spans.push(" in ".dim());
                        spans.push(
                            relativize_paths_in_text(&p.display().to_string(), &self.cwd).into(),
                        );
                    }
                    sub_items.push(Line::from(spans));
                } else if let Some(crate::presentation::Invocation::ListFiles { path }) =
                    &snap.invocation
                {
                    let mut spans: Vec<Span<'static>> = vec!["List".cyan(), " ".into()];
                    if let Some(p) = path {
                        spans.push(
                            relativize_paths_in_text(&p.display().to_string(), &self.cwd).into(),
                        );
                    }
                    sub_items.push(Line::from(spans));
                } else {
                    sub_items.push(Line::from(vec![
                        "Search".cyan(),
                        " ".into(),
                        relativize_paths_in_text(&snap.title, &self.cwd).into(),
                    ]));
                }
                i += 1;
                continue;
            }

            // ListFiles invocation (from Read/Search tools classified as exploring)
            if matches!(
                &snap.invocation,
                Some(crate::presentation::Invocation::ListFiles { .. })
            ) {
                if let Some(crate::presentation::Invocation::ListFiles { path }) = &snap.invocation
                {
                    let mut spans: Vec<Span<'static>> = vec!["List".cyan(), " ".into()];
                    if let Some(p) = path {
                        spans.push(
                            relativize_paths_in_text(&p.display().to_string(), &self.cwd).into(),
                        );
                    }
                    sub_items.push(Line::from(spans));
                }
                i += 1;
                continue;
            }

            // Fallback: generic sub-item. Skip the kind label if the title
            // already starts with it (e.g., kind="List", title="List /path"
            // → show "List /path" not "List List /path").
            let kind_label = format_tool_kind(&snap.kind);
            let title = relativize_paths_in_text(&snap.title, &self.cwd);
            if title.to_lowercase().starts_with(&kind_label.to_lowercase()) {
                sub_items.push(Line::from(vec![title.into()]));
            } else {
                sub_items.push(Line::from(vec![
                    kind_label.to_string().cyan(),
                    " ".into(),
                    title.into(),
                ]));
            }
            i += 1;
        }

        // Apply tree-style prefix (└ for first, spaces for subsequent)
        let wrap_width = width.saturating_sub(4).max(1);
        let wrap_opts =
            RtOptions::new(wrap_width as usize).word_splitter(WordSplitter::NoHyphenation);
        let mut wrapped_items: Vec<Line<'static>> = Vec::new();
        for item in sub_items {
            push_owned_lines(
                &word_wrap_line(&item, wrap_opts.clone()),
                &mut wrapped_items,
            );
        }

        out.extend(prefix_lines(
            wrapped_items,
            "  \u{2514} ".dim(),
            "    ".into(),
        ));

        out
    }

    fn render_edit_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines = Vec::new();

        // Bullet: green for completed, red for failed, spinner for active
        let bullet = if self.snapshot.phase == crate::presentation::ToolPhase::Completed {
            "•".green().bold()
        } else if self.snapshot.phase == crate::presentation::ToolPhase::Failed {
            "•".red().bold()
        } else {
            spinner(self.start_time, self.animations_enabled)
        };

        // For failed edits: show error text or "(failed)" fallback
        if self.snapshot.phase == crate::presentation::ToolPhase::Failed {
            let header =
                relativize_paths_in_text(&format_edit_tool_header(&self.snapshot), &self.cwd);
            lines.push(Line::from(vec![bullet, " ".into(), header.bold()]));
            if let Some(error_text) = extract_error_text(&self.snapshot) {
                lines.push(Line::from(vec!["  └ ".dim(), error_text.dim()]));
            } else {
                lines.push(Line::from(vec!["  └ ".dim(), "(failed)".dim()]));
            }
            return lines;
        }

        if !self.edit_changes.is_empty() {
            let diff_lines = create_diff_summary(&self.edit_changes, &self.cwd, width as usize);

            if let Some((first, rest)) = diff_lines.split_first() {
                // Replace DiffSummary's dim "• " bullet with our phase-aware
                // bullet, and for Move tools swap the "Edited" verb with "Moved".
                let is_move = self.snapshot.kind == crate::presentation::ToolKind::Move;
                let mut header_spans = vec![bullet, " ".into()];
                for span in &first.spans {
                    if span.content.as_ref() == "• " {
                        continue;
                    }
                    if is_move && span.content.as_ref() == "Edited" {
                        header_spans.push("Moved".bold());
                    } else {
                        header_spans.push(span.clone());
                    }
                }
                lines.push(Line::from(header_spans));
                lines.extend(rest.to_vec());
            }
        } else {
            // No diff data: use the format_edit_tool_header as sole header
            let header =
                relativize_paths_in_text(&format_edit_tool_header(&self.snapshot), &self.cwd);
            lines.push(Line::from(vec![bullet, " ".into(), header.bold()]));
        }

        lines
    }

    /// A delegated-task call: what it did and how it came out. The child's own work is shown
    /// as its subagent, not here.
    fn render_delegation_lines(
        &self,
        call: &crate::presentation::DelegationCall,
    ) -> Vec<Line<'static>> {
        let active = self.is_active();
        let bullet = if active {
            spinner(self.start_time, self.animations_enabled)
        } else if self.snapshot.phase == crate::presentation::ToolPhase::Failed {
            "⇢".red().bold()
        } else {
            "⇢".cyan().bold()
        };
        let mut lines = vec![Line::from(vec![
            bullet,
            " ".into(),
            call.header(active).bold(),
        ])];
        let outcome = self
            .snapshot
            .artifacts
            .iter()
            .find_map(|artifact| match artifact {
                crate::presentation::Artifact::Text { text, .. } => {
                    crate::presentation::delegation_outcome(text)
                }
                crate::presentation::Artifact::Diff(_) => None,
            });
        if let Some(outcome) = outcome {
            lines.push(Line::from(vec!["  └ ".dim(), outcome.dim()]));
        }
        lines
    }

    /// One of Nori's own tools: what it did, with a glyph for what it is and a tone for every
    /// piece of text, and its details hung off a small tree. Long lines wrap under their branch.
    fn render_native_lines(
        &self,
        call: &crate::presentation::NativeCall,
        width: u16,
    ) -> Vec<Line<'static>> {
        use crate::presentation::NativeTone as Tone;

        let active = self.is_active();
        let failed = self.snapshot.phase == crate::presentation::ToolPhase::Failed;
        let result = self
            .snapshot
            .artifacts
            .iter()
            .find_map(|artifact| match artifact {
                crate::presentation::Artifact::Text { text, .. } => Some(text.as_str()),
                crate::presentation::Artifact::Diff(_) => None,
            });
        let view = crate::presentation::native_view(call, active, failed, result);
        let style = |text: String, tone: Tone| -> Span<'static> {
            match tone {
                Tone::Plain => text.into(),
                Tone::Strong => text.bold(),
                Tone::Accent => text.cyan().bold(),
                Tone::Dim => text.dim(),
                Tone::Added => text.green(),
                Tone::Modified => text.cyan(),
                Tone::Deleted | Tone::Error => text.red(),
            }
        };
        // The glyph says what kind of call this is; its color says how it went.
        let glyph = if active {
            spinner(self.start_time, self.animations_enabled)
        } else if failed {
            view.glyph.red().bold()
        } else if view.glyph == "◆" {
            view.glyph.green().bold()
        } else {
            view.glyph.cyan().bold()
        };
        let mut header = vec![glyph, " ".into()];
        header.extend(
            view.header
                .into_iter()
                .map(|part| style(part.text, part.tone)),
        );
        let width = usize::from(width.max(20));
        let mut lines = crate::wrapping::word_wrap_lines(
            [Line::from(header)],
            crate::wrapping::RtOptions::new(width).subsequent_indent("  ".into()),
        );
        let count = view.detail.len();
        for (index, detail) in view.detail.into_iter().enumerate() {
            let (connector, continuation) = if index + 1 == count {
                ("  └ ", "    ")
            } else {
                ("  ├ ", "  │ ")
            };
            let mut spans = Vec::new();
            if let Some((marker, tone)) = detail.marker {
                spans.push(style(marker.to_string(), tone));
                spans.push(" ".into());
            }
            spans.push(style(detail.text, detail.tone));
            spans.extend(
                detail
                    .suffix
                    .into_iter()
                    .map(|part| style(part.text, part.tone)),
            );
            lines.extend(crate::wrapping::word_wrap_lines(
                [Line::from(spans)],
                crate::wrapping::RtOptions::new(width)
                    .initial_indent(connector.dim().into())
                    .subsequent_indent(continuation.dim().into()),
            ));
        }
        lines
    }
}

fn extract_error_text(snapshot: &crate::presentation::ToolSnapshot) -> Option<String> {
    let raw = snapshot.raw_output.as_ref()?;
    for key in ["error", "stderr", "output"] {
        if let Some(text) = raw.get(key).and_then(serde_json::Value::as_str)
            && !text.is_empty()
        {
            return Some(text.to_string());
        }
    }
    // Check if raw_output is itself a string
    if let Some(s) = raw.as_str()
        && !s.is_empty()
    {
        return Some(s.to_string());
    }
    None
}

pub(crate) fn diff_changes_from_artifacts(
    artifacts: &[crate::presentation::Artifact],
    cwd: &std::path::Path,
) -> std::collections::HashMap<std::path::PathBuf, crate::ui_types::FileChange> {
    let mut changes = std::collections::HashMap::new();
    for artifact in artifacts {
        if let crate::presentation::Artifact::Diff(change) = artifact {
            let file_change = match &change.old_text {
                None => crate::ui_types::FileChange::Add {
                    content: change.new_text.clone(),
                },
                Some(old_text) => crate::ui_types::FileChange::Update {
                    first_line: first_line_in_file(&change.path, cwd, old_text, &change.new_text),
                    old_text: old_text.clone(),
                    new_text: change.new_text.clone(),
                    move_path: None,
                },
            };
            changes.insert(change.path.clone(), file_change);
        }
    }
    changes
}

pub(crate) fn changes_from_invocation(
    invocation: &Option<crate::presentation::Invocation>,
    cwd: &std::path::Path,
) -> std::collections::HashMap<std::path::PathBuf, crate::ui_types::FileChange> {
    let mut changes = std::collections::HashMap::new();
    match invocation.as_ref() {
        Some(crate::presentation::Invocation::FileChanges { changes: fc }) => {
            for change in fc {
                let file_change = match &change.old_text {
                    None => crate::ui_types::FileChange::Add {
                        content: change.new_text.clone(),
                    },
                    Some(old_text) => crate::ui_types::FileChange::Update {
                        first_line: first_line_in_file(
                            &change.path,
                            cwd,
                            old_text,
                            &change.new_text,
                        ),
                        old_text: old_text.clone(),
                        new_text: change.new_text.clone(),
                        move_path: None,
                    },
                };
                changes.insert(change.path.clone(), file_change);
            }
        }
        Some(crate::presentation::Invocation::FileOperations { operations }) => {
            for op in operations {
                let (path, file_change) = match op {
                    crate::presentation::FileOperation::Create { path, new_text } => (
                        path.clone(),
                        crate::ui_types::FileChange::Add {
                            content: new_text.clone(),
                        },
                    ),
                    crate::presentation::FileOperation::Update {
                        path,
                        old_text,
                        new_text,
                    } => (
                        path.clone(),
                        crate::ui_types::FileChange::Update {
                            first_line: first_line_in_file(path, cwd, old_text, new_text),
                            old_text: old_text.clone(),
                            new_text: new_text.clone(),
                            move_path: None,
                        },
                    ),
                    crate::presentation::FileOperation::Delete { path, old_text } => (
                        path.clone(),
                        crate::ui_types::FileChange::Delete {
                            content: old_text.clone().unwrap_or_default(),
                        },
                    ),
                    crate::presentation::FileOperation::Move {
                        from_path,
                        to_path,
                        old_text,
                        new_text,
                    } => {
                        let old = old_text.clone().unwrap_or_default();
                        let new = new_text.clone().unwrap_or_else(|| old.clone());
                        (
                            from_path.clone(),
                            crate::ui_types::FileChange::Update {
                                first_line: first_line_in_file(from_path, cwd, &old, &new),
                                old_text: old,
                                new_text: new,
                                move_path: Some(to_path.clone()),
                            },
                        )
                    }
                };
                changes.insert(path, file_change);
            }
        }
        _ => {}
    }
    changes
}

fn changes_from_snapshot(
    snapshot: &crate::presentation::ToolSnapshot,
    cwd: &std::path::Path,
) -> HashMap<PathBuf, crate::ui_types::FileChange> {
    let diff_changes = diff_changes_from_artifacts(&snapshot.artifacts, cwd);
    if diff_changes.is_empty() {
        changes_from_invocation(&snapshot.invocation, cwd)
    } else {
        diff_changes
    }
}

impl HistoryCell for ClientToolCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        if let Some(crate::presentation::Invocation::Delegation(call)) = &self.snapshot.invocation {
            self.render_delegation_lines(call)
        } else if let Some(crate::presentation::Invocation::Native(call)) =
            &self.snapshot.invocation
        {
            self.render_native_lines(call, width)
        } else if !self.exploring_snapshots.is_empty() || is_exploring_snapshot(&self.snapshot) {
            self.render_exploring_lines(width)
        } else if self.snapshot.kind == crate::presentation::ToolKind::Execute {
            other::render(&self.snapshot, width, false)
        } else if matches!(
            self.snapshot.kind,
            crate::presentation::ToolKind::Create
                | crate::presentation::ToolKind::Edit
                | crate::presentation::ToolKind::Delete
                | crate::presentation::ToolKind::Move
        ) {
            self.render_edit_lines(width)
        } else {
            let mut lines = other::render(&self.snapshot, width, false);
            if let Some(header) = lines.first_mut() {
                for span in &mut header.spans {
                    span.content = relativize_paths_in_text(&span.content, &self.cwd).into();
                }
            }
            lines
        }
    }

    fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        self.expanded_lines(width)
    }
}

/// Extract a display name for a Read path: use the file name (basename) when
/// available, falling back to the full path display. This matches the Codex
/// ExecCell exploring style where Read items show just the filename.
fn read_display_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string())
}

fn is_active_phase(phase: &crate::presentation::ToolPhase) -> bool {
    matches!(
        phase,
        crate::presentation::ToolPhase::Pending
            | crate::presentation::ToolPhase::PendingApproval
            | crate::presentation::ToolPhase::InProgress
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::Artifact;
    use crate::presentation::Invocation;
    use crate::presentation::ToolKind;
    use crate::presentation::ToolPhase;
    use crate::presentation::ToolSnapshot;
    use pretty_assertions::assert_eq;

    fn render_lines(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    fn write_updated_rust_fixture(
        dir: &std::path::Path,
        relative_path: &str,
        changed_line: i32,
        changed_text: &str,
    ) -> PathBuf {
        let file_path = dir.join(relative_path);
        if let Some(parent) = file_path.parent() {
            std::fs::create_dir_all(parent).expect("create fixture parent dir");
        }
        let file_content = (1..=100)
            .map(|line| {
                if line == changed_line {
                    format!("{changed_text}\n")
                } else {
                    format!("fn line_{line}() {{}}\n")
                }
            })
            .collect::<String>();
        std::fs::write(&file_path, file_content).expect("write fixture file");
        file_path
    }

    fn make_execute_snapshot(
        phase: ToolPhase,
        command: &str,
        artifacts: Vec<Artifact>,
        raw_output: Option<serde_json::Value>,
    ) -> ToolSnapshot {
        ToolSnapshot {
            call_id: "call-1".into(),
            title: command.into(),
            kind: ToolKind::Execute,
            phase,
            locations: vec![],
            invocation: Some(Invocation::Command {
                command: command.into(),
            }),
            artifacts,
            raw_input: Some(serde_json::json!({"command": command})),
            raw_output,
            owner_request_id: None,
        }
    }

    #[test]
    fn execute_completed_with_output_shows_ran_and_output() {
        let snapshot = make_execute_snapshot(
            ToolPhase::Completed,
            "date --utc",
            vec![Artifact::Text {
                text: "2026-03-30 05:45:34 UTC".into(),
                format: crate::presentation::TextFormat::Plain,
            }],
            Some(serde_json::json!({"exit_code": 0, "stdout": "2026-03-30 05:45:34 UTC"})),
        );
        let cell = ClientToolCell::new(snapshot, PathBuf::from("/tmp/test-cwd"), false);
        let lines = render_lines(&cell.display_lines(80));

        // First line: green bullet + "Ran" + command
        assert!(lines[0].contains("Ran"));
        assert!(lines[0].contains("date --utc"));
        // Should NOT have the generic "Tool [completed]" format
        assert!(!lines[0].contains("Tool ["));

        // Output line under └ prefix
        assert!(lines.iter().any(|l| l.contains("2026-03-30 05:45:34 UTC")));
    }

    #[test]
    fn execute_completed_no_output_shows_no_output() {
        let snapshot = make_execute_snapshot(
            ToolPhase::Completed,
            "rm /tmp/foo.txt",
            vec![],
            Some(serde_json::json!({"exit_code": 0, "stdout": ""})),
        );
        let cell = ClientToolCell::new(snapshot, PathBuf::from("/tmp/test-cwd"), false);
        let lines = render_lines(&cell.display_lines(80));

        assert!(lines[0].contains("Ran"));
        assert!(lines[0].contains("rm /tmp/foo.txt"));
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn execute_failed_shows_red_bullet_text() {
        let snapshot = make_execute_snapshot(
            ToolPhase::Failed,
            "cargo test",
            vec![Artifact::Text {
                text: "error[E0308]: mismatched types".into(),
                format: crate::presentation::TextFormat::Plain,
            }],
            Some(serde_json::json!({"exit_code": 1, "stdout": "error[E0308]: mismatched types"})),
        );
        let cell = ClientToolCell::new(snapshot, PathBuf::from("/tmp/test-cwd"), false);
        let lines = cell.display_lines(80);

        // Verify the bullet span is red+bold (not dim)
        let bullet_span = &lines[0].spans[0];
        assert!(
            bullet_span.style.fg == Some(ratatui::style::Color::Red),
            "Failed execute should have red bullet, got {:?}",
            bullet_span.style
        );

        let text_lines = render_lines(&lines);
        assert!(text_lines[0].contains("Ran"));
    }

    #[test]
    fn execute_in_progress_shows_running() {
        let snapshot = make_execute_snapshot(ToolPhase::InProgress, "cargo build", vec![], None);
        let cell = ClientToolCell::new(snapshot, PathBuf::from("/tmp/test-cwd"), false);
        let lines = render_lines(&cell.display_lines(80));

        assert!(lines[0].contains("Running"));
        assert!(lines[0].contains("cargo build"));
        assert!(!lines[0].contains("Tool ["));
    }

    #[test]
    fn execute_output_truncation_with_many_lines() {
        let long_output = (1..=20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let snapshot = make_execute_snapshot(
            ToolPhase::Completed,
            "cat big_file.txt",
            vec![Artifact::Text {
                text: long_output.clone(),
                format: crate::presentation::TextFormat::Plain,
            }],
            Some(serde_json::json!({"exit_code": 0, "stdout": long_output})),
        );
        let cell = ClientToolCell::new(snapshot, PathBuf::from("/tmp/test-cwd"), false);
        let lines = render_lines(&cell.display_lines(80));

        assert!(lines.len() <= 2, "{lines:?}");
        let expanded = render_lines(&cell.expanded_lines(80)).join("\n");
        assert!(expanded.contains("line 1") && expanded.contains("line 20"));
    }

    /// Output a tool declares as markdown is rendered as markdown: no raw syntax, and only its
    /// first rows until the transcript shows it all.
    #[test]
    fn declared_markdown_output_is_rendered() {
        let body = std::iter::once("# Findings\n\n- **one** `a.rs`\n- two".to_owned())
            .chain((0..20).map(|n| format!("\nline {n}")))
            .collect::<String>();
        let snapshot = ToolSnapshot {
            call_id: "call-md".into(),
            title: "Read report".into(),
            kind: ToolKind::Fetch,
            phase: ToolPhase::Completed,
            locations: vec![],
            invocation: None,
            artifacts: vec![Artifact::Text {
                text: body,
                format: crate::presentation::TextFormat::Markdown,
            }],
            raw_input: None,
            raw_output: None,
            owner_request_id: None,
        };
        let cell = ClientToolCell::new(snapshot, PathBuf::from("/tmp/test-cwd"), false);
        let collapsed = render_lines(&cell.display_lines(80)).join("\n");
        assert!(collapsed.contains("Findings"), "{collapsed}");
        assert!(!collapsed.contains("line 19"), "{collapsed}");
        let full = render_lines(&cell.transcript_lines(80)).join("\n");
        assert!(
            full.contains("one a.rs") && full.contains("line 19") && !full.contains("**"),
            "{full}"
        );
    }

    #[test]
    fn non_execute_non_exploring_tool_uses_generic_rendering() {
        // Fetch is not an exploring tool kind, so it should use generic rendering
        let snapshot = ToolSnapshot {
            call_id: "call-2".into(),
            title: "Fetch https://example.com".into(),
            kind: ToolKind::Fetch,
            phase: ToolPhase::Completed,
            locations: vec![],
            invocation: Some(Invocation::Tool {
                tool_name: "fetch".into(),
                input: None,
            }),
            artifacts: vec![Artifact::Text {
                text: "200 OK".into(),
                format: crate::presentation::TextFormat::Plain,
            }],
            raw_input: None,
            raw_output: None,
            owner_request_id: None,
        };
        let cell = ClientToolCell::new(snapshot, PathBuf::from("/tmp/test-cwd"), false);
        let lines = render_lines(&cell.display_lines(80));

        assert!(lines[0].contains("Fetch https://example.com"));
        assert!(!lines[0].contains("Tool ["));
    }

    #[test]
    fn execute_strips_code_fences_from_artifact_text() {
        let snapshot = make_execute_snapshot(
            ToolPhase::Completed,
            "uptime -p",
            vec![Artifact::Text {
                text: "```console\nup 1 day, 20 hours\n```".into(),
                format: crate::presentation::TextFormat::Plain,
            }],
            Some(serde_json::json!({"exit_code": 0, "stdout": "up 1 day, 20 hours"})),
        );
        let cell = ClientToolCell::new(snapshot, PathBuf::from("/tmp/test-cwd"), false);
        let lines = render_lines(&cell.display_lines(80));

        // Should NOT show ```console or ``` literals
        assert!(
            !lines.iter().any(|l| l.contains("```")),
            "Code fences should be stripped, got: {lines:?}"
        );
        // Should show the actual output
        assert!(lines.iter().any(|l| l.contains("up 1 day, 20 hours")));
    }

    #[test]
    fn execute_success_has_green_bullet() {
        let snapshot = make_execute_snapshot(
            ToolPhase::Completed,
            "echo ok",
            vec![Artifact::Text {
                text: "ok".into(),
                format: crate::presentation::TextFormat::Plain,
            }],
            Some(serde_json::json!({"exit_code": 0, "stdout": "ok"})),
        );
        let cell = ClientToolCell::new(snapshot, PathBuf::from("/tmp/test-cwd"), false);
        let lines = cell.display_lines(80);

        let bullet_span = &lines[0].spans[0];
        assert!(
            bullet_span.style.fg == Some(ratatui::style::Color::Green),
            "Successful execute should have green bullet, got {:?}",
            bullet_span.style
        );
    }

    #[test]
    fn execute_fallback_to_title_when_no_invocation() {
        let snapshot = ToolSnapshot {
            call_id: "call-1".into(),
            title: "ls -la".into(),
            kind: ToolKind::Execute,
            phase: ToolPhase::Completed,
            locations: vec![],
            invocation: None,
            artifacts: vec![],
            raw_input: None,
            raw_output: Some(serde_json::json!({"exit_code": 0, "stdout": ""})),
            owner_request_id: None,
        };
        let cell = ClientToolCell::new(snapshot, PathBuf::from("/tmp/test-cwd"), false);
        let lines = render_lines(&cell.display_lines(80));

        assert!(lines[0].contains("Ran"));
        assert!(lines[0].contains("ls -la"));
    }

    // --- Spec 08: Gemini Empty Content Fallback ---

    #[test]
    fn generic_completed_with_no_details_but_locations_shows_location_paths() {
        let snapshot = ToolSnapshot {
            call_id: "call-3".into(),
            title: "Fetch resource".into(),
            kind: ToolKind::Fetch,
            phase: ToolPhase::Completed,
            locations: vec![crate::presentation::ToolLocation {
                path: "/repo/README.md".into(),
                line: None,
            }],
            invocation: None,
            artifacts: vec![],
            raw_input: None,
            raw_output: None,
            owner_request_id: None,
        };
        let cell = ClientToolCell::new(snapshot, PathBuf::from("/tmp/test-cwd"), false);
        let lines = render_lines(&cell.display_lines(80));

        // Should have more than just the header line
        assert!(
            lines.len() > 1,
            "Expected location detail lines, got only: {lines:?}"
        );
        // Location path should appear in the detail lines
        assert!(
            lines.iter().any(|l| l.contains("/repo/README.md")),
            "Expected location path in output, got: {lines:?}"
        );
    }

    #[test]
    fn generic_completed_with_no_details_no_locations_still_renders_header() {
        let snapshot = ToolSnapshot {
            call_id: "call-4".into(),
            title: "Some tool".into(),
            kind: ToolKind::Fetch,
            phase: ToolPhase::Completed,
            locations: vec![],
            invocation: None,
            artifacts: vec![],
            raw_input: None,
            raw_output: None,
            owner_request_id: None,
        };
        let cell = ClientToolCell::new(snapshot, PathBuf::from("/tmp/test-cwd"), false);
        let lines = render_lines(&cell.display_lines(80));

        // Should still render at least a header line
        assert!(!lines.is_empty(), "Expected at least a header line");
        assert!(
            lines[0].contains("Some tool"),
            "Header should contain the title, got: {}",
            lines[0]
        );
    }

    // --- Spec 06: Artifact Text Output Cleanup ---

    #[test]
    fn generic_tool_strips_code_fences_from_text_artifact() {
        let snapshot = ToolSnapshot {
            call_id: "call-fence".into(),
            title: "Fetch https://example.com/main.rs".into(),
            kind: ToolKind::Fetch,
            phase: ToolPhase::Completed,
            locations: vec![],
            invocation: Some(Invocation::Tool {
                tool_name: "fetch".into(),
                input: None,
            }),
            artifacts: vec![Artifact::Text {
                text: "```rust\nfn main() {}\n```".into(),
                format: crate::presentation::TextFormat::Plain,
            }],
            raw_input: None,
            raw_output: None,
            owner_request_id: None,
        };
        let cell = ClientToolCell::new(snapshot, PathBuf::from("/tmp/test-cwd"), false);
        let lines = render_lines(&cell.display_lines(80));

        // Code fence markers should NOT appear in the rendered output
        assert!(
            !lines.iter().any(|l| l.contains("```")),
            "Code fences should be stripped from generic tool output, got: {lines:?}"
        );
        // The actual content should still appear
        assert!(
            lines.iter().any(|l| l.contains("fn main() {}")),
            "Content inside fences should still render, got: {lines:?}"
        );
    }

    #[test]
    fn generic_tool_single_line_output_has_no_output_prefix() {
        let snapshot = ToolSnapshot {
            call_id: "call-single".into(),
            title: "Fetch https://example.com".into(),
            kind: ToolKind::Fetch,
            phase: ToolPhase::Completed,
            locations: vec![],
            invocation: None,
            artifacts: vec![Artifact::Text {
                text: "200 OK".into(),
                format: crate::presentation::TextFormat::Plain,
            }],
            raw_input: None,
            raw_output: None,
            owner_request_id: None,
        };
        let cell = ClientToolCell::new(snapshot, PathBuf::from("/tmp/test-cwd"), false);
        let lines = render_lines(&cell.display_lines(80));

        // Should NOT have "Output:" prefix for single-line output
        assert!(
            !lines.iter().any(|l| l.contains("Output:")),
            "Single-line output should not have 'Output:' prefix, got: {lines:?}"
        );
        // The actual text should still appear
        assert!(
            lines.iter().any(|l| l.contains("200 OK")),
            "Output text should still render, got: {lines:?}"
        );
    }

    #[test]
    fn generic_tool_multi_line_output_has_no_output_prefix() {
        let snapshot = ToolSnapshot {
            call_id: "call-multi".into(),
            title: "Think about pattern".into(),
            kind: ToolKind::Think,
            phase: ToolPhase::Completed,
            locations: vec![],
            invocation: Some(Invocation::Tool {
                tool_name: "think".into(),
                input: None,
            }),
            artifacts: vec![Artifact::Text {
                text: "line one\nline two\nline three".into(),
                format: crate::presentation::TextFormat::Plain,
            }],
            raw_input: None,
            raw_output: None,
            owner_request_id: None,
        };
        let cell = ClientToolCell::new(snapshot, PathBuf::from("/tmp/test-cwd"), false);
        let lines = render_lines(&cell.display_lines(80));

        // Should NOT have "Output:" prefix for multi-line output either
        assert!(
            !lines.iter().any(|l| l.contains("Output:")),
            "Multi-line output should not have 'Output:' prefix, got: {lines:?}"
        );
        // Content lines should still appear
        assert!(
            lines.iter().any(|l| l.contains("line one")),
            "First content line should render, got: {lines:?}"
        );
    }

    #[test]
    fn generic_tool_invocation_omitted_when_redundant_with_title() {
        let snapshot = ToolSnapshot {
            call_id: "call-dup".into(),
            title: "Fetch https://example.com".into(),
            kind: ToolKind::Fetch,
            phase: ToolPhase::Completed,
            locations: vec![],
            invocation: Some(Invocation::Tool {
                tool_name: "Fetch https://example.com".into(),
                input: None,
            }),
            artifacts: vec![Artifact::Text {
                text: "# README".into(),
                format: crate::presentation::TextFormat::Plain,
            }],
            raw_input: None,
            raw_output: None,
            owner_request_id: None,
        };
        let cell = ClientToolCell::new(snapshot, PathBuf::from("/tmp/test-cwd"), false);
        let lines = render_lines(&cell.display_lines(80));

        // The invocation detail line should NOT appear because the title
        // already contains the same information
        assert!(
            !lines.iter().any(|l| l.contains("Tool: Fetch")),
            "Redundant invocation should be omitted when title contains same info, got: {lines:?}"
        );
        // But the artifact output should still appear
        assert!(
            lines.iter().any(|l| l.contains("# README")),
            "Output content should still render, got: {lines:?}"
        );
    }

    // --- Spec 04: Path Display Normalization ---

    #[test]
    fn generic_tool_title_relativizes_cwd_paths() {
        let cwd = PathBuf::from("/home/user/project");
        let snapshot = ToolSnapshot {
            call_id: "call-path".into(),
            title: "Fetch /home/user/project/src/main.rs".into(),
            kind: ToolKind::Fetch,
            phase: ToolPhase::Completed,
            locations: vec![],
            invocation: Some(Invocation::Tool {
                tool_name: "fetch".into(),
                input: None,
            }),
            artifacts: vec![Artifact::Text {
                text: "fn main() {}".into(),
                format: crate::presentation::TextFormat::Plain,
            }],
            raw_input: None,
            raw_output: None,
            owner_request_id: None,
        };
        let cell = ClientToolCell::new(snapshot, cwd, false);
        let lines = render_lines(&cell.display_lines(80));

        // The absolute path should NOT appear in the header
        assert!(
            !lines[0].contains("/home/user/project/src/main.rs"),
            "Absolute path should be relativized in title, got: {}",
            lines[0]
        );
        // The relative path should appear instead
        assert!(
            lines[0].contains("src/main.rs"),
            "Relative path should appear in title, got: {}",
            lines[0]
        );
    }

    #[test]
    fn generic_tool_invocation_path_relativized() {
        let cwd = PathBuf::from("/home/user/project");
        let snapshot = ToolSnapshot {
            call_id: "call-inv-path".into(),
            title: "Search in /home/user/project/src".into(),
            kind: ToolKind::Search,
            phase: ToolPhase::Completed,
            locations: vec![],
            invocation: Some(Invocation::Search {
                query: Some("TODO".into()),
                path: Some("/home/user/project/src".into()),
            }),
            artifacts: vec![Artifact::Text {
                text: "found 3 results".into(),
                format: crate::presentation::TextFormat::Plain,
            }],
            raw_input: None,
            raw_output: None,
            owner_request_id: None,
        };
        let cell = ClientToolCell::new(snapshot, cwd, false);
        let lines = render_lines(&cell.display_lines(80));

        // Invocation detail should NOT have absolute path
        let detail_lines: Vec<_> = lines.iter().filter(|l| l.contains("Search")).collect();
        assert!(
            !detail_lines
                .iter()
                .any(|l| l.contains("/home/user/project/src")),
            "Absolute path should be relativized in invocation detail, got: {detail_lines:?}"
        );
    }

    #[test]
    fn execute_tool_command_not_path_mangled() {
        let cwd = PathBuf::from("/home/user/project");
        let snapshot = make_execute_snapshot(
            ToolPhase::Completed,
            "cat /home/user/project/README.md",
            vec![Artifact::Text {
                text: "# Readme".into(),
                format: crate::presentation::TextFormat::Plain,
            }],
            Some(serde_json::json!({"exit_code": 0, "stdout": "# Readme"})),
        );
        // Override the default cwd for this test
        let cell = ClientToolCell::new(snapshot, cwd, false);
        let lines = render_lines(&cell.display_lines(80));

        // Execute commands should keep their original command text
        // (path normalization in the command itself is NOT desired for execute tools)
        assert!(
            lines[0].contains("Ran"),
            "Execute should still show 'Ran', got: {}",
            lines[0]
        );
    }

    // --- Spec 07: Diff Artifact Rendering in ClientToolCell ---

    #[test]
    fn generic_tool_renders_diff_artifacts() {
        let snapshot = ToolSnapshot {
            call_id: "call-diff".into(),
            title: "Edit README.md".into(),
            kind: ToolKind::Edit,
            phase: ToolPhase::InProgress,
            locations: vec![crate::presentation::ToolLocation {
                path: PathBuf::from("README.md"),
                line: None,
            }],
            invocation: None,
            artifacts: vec![crate::presentation::Artifact::Diff(
                crate::presentation::FileChange {
                    path: PathBuf::from("README.md"),
                    old_text: Some("# Old Title\n".into()),
                    new_text: "# New Title\n".into(),
                },
            )],
            raw_input: None,
            raw_output: None,
            owner_request_id: None,
        };
        let cell = ClientToolCell::new(snapshot, PathBuf::from("/tmp/test-cwd"), false);
        let lines = render_lines(&cell.display_lines(80));

        // Should render some diff content -- at minimum the file path
        assert!(
            lines.iter().any(|l| l.contains("README.md")),
            "Diff artifact should render file path in output, got: {lines:?}"
        );
        // Should show some diff indicator (add/remove line counts or content)
        let has_diff_indicator = lines.iter().any(|l| {
            l.contains('+') || l.contains('-') || l.contains("Old Title") || l.contains("New Title")
        });
        assert!(
            has_diff_indicator,
            "Diff artifact should render diff content, got: {lines:?}"
        );
    }

    // --- Spec 02: Exploring Cell Grouping ---

    #[test]
    fn exploring_cell_with_single_read_shows_explored_header() {
        let snapshot = ToolSnapshot {
            call_id: "call-read-1".into(),
            title: "Read README.md".into(),
            kind: ToolKind::Read,
            phase: ToolPhase::Completed,
            locations: vec![crate::presentation::ToolLocation {
                path: PathBuf::from("README.md"),
                line: None,
            }],
            invocation: Some(Invocation::Read {
                path: "README.md".into(),
            }),
            artifacts: vec![Artifact::Text {
                text: "# Title\nSome content".into(),
                format: crate::presentation::TextFormat::Plain,
            }],
            raw_input: None,
            raw_output: None,
            owner_request_id: None,
        };
        let mut cell = ClientToolCell::new(snapshot, PathBuf::from("/tmp/cwd"), false);
        cell.mark_exploring();
        let lines = render_lines(&cell.display_lines(80));

        // Should show "Explored" header, not "Tool [completed]"
        assert!(
            lines[0].contains("Explored"),
            "Exploring cell should show 'Explored' header, got: {}",
            lines[0]
        );

        // Should show the read file in a sub-item
        assert!(
            lines
                .iter()
                .any(|l| l.contains("Read") && l.contains("README.md")),
            "Exploring cell should show Read sub-item, got: {lines:?}"
        );

        // Should NOT show the read content (output is noise in exploring)
        assert!(
            !lines
                .iter()
                .any(|l| l.contains("# Title") || l.contains("Some content")),
            "Exploring cell should omit read output content, got: {lines:?}"
        );
    }

    #[test]
    fn exploring_cell_groups_multiple_reads() {
        let snap1 = ToolSnapshot {
            call_id: "call-r1".into(),
            title: "Read file1.rs".into(),
            kind: ToolKind::Read,
            phase: ToolPhase::Completed,
            locations: vec![],
            invocation: Some(Invocation::Read {
                path: "file1.rs".into(),
            }),
            artifacts: vec![],
            raw_input: None,
            raw_output: None,
            owner_request_id: None,
        };
        let snap2 = ToolSnapshot {
            call_id: "call-r2".into(),
            title: "Read file2.rs".into(),
            kind: ToolKind::Read,
            phase: ToolPhase::Completed,
            locations: vec![],
            invocation: Some(Invocation::Read {
                path: "file2.rs".into(),
            }),
            artifacts: vec![],
            raw_input: None,
            raw_output: None,
            owner_request_id: None,
        };

        let mut cell = ClientToolCell::new(snap1, PathBuf::from("/tmp/cwd"), false);
        cell.mark_exploring();
        cell.merge_exploring(snap2);
        let lines = render_lines(&cell.display_lines(80));

        // Should group reads on a single line: "Read file1.rs, file2.rs"
        assert!(
            lines
                .iter()
                .any(|l| l.contains("file1.rs") && l.contains("file2.rs")),
            "Exploring cell should group reads, got: {lines:?}"
        );
    }

    #[test]
    fn exploring_cell_shows_search_sub_item() {
        let snap = ToolSnapshot {
            call_id: "call-search".into(),
            title: "Search TODO".into(),
            kind: ToolKind::Search,
            phase: ToolPhase::Completed,
            locations: vec![],
            invocation: Some(Invocation::Search {
                query: Some("TODO".into()),
                path: Some("/repo/src".into()),
            }),
            artifacts: vec![],
            raw_input: None,
            raw_output: None,
            owner_request_id: None,
        };

        let mut cell = ClientToolCell::new(snap, PathBuf::from("/tmp/cwd"), false);
        cell.mark_exploring();
        let lines = render_lines(&cell.display_lines(80));

        assert!(
            lines
                .iter()
                .any(|l| l.contains("Search") && l.contains("TODO")),
            "Exploring cell should show Search sub-item, got: {lines:?}"
        );
    }

    #[test]
    fn exploring_cell_in_progress_shows_exploring_header() {
        let snapshot = ToolSnapshot {
            call_id: "call-read-active".into(),
            title: "Read file.rs".into(),
            kind: ToolKind::Read,
            phase: ToolPhase::InProgress,
            locations: vec![],
            invocation: Some(Invocation::Read {
                path: "file.rs".into(),
            }),
            artifacts: vec![],
            raw_input: None,
            raw_output: None,
            owner_request_id: None,
        };
        let mut cell = ClientToolCell::new(snapshot, PathBuf::from("/tmp/cwd"), false);
        cell.mark_exploring();
        let lines = render_lines(&cell.display_lines(80));

        // Should show "Exploring" (active), not "Explored"
        assert!(
            lines[0].contains("Exploring"),
            "Active exploring cell should show 'Exploring', got: {}",
            lines[0]
        );
    }

    // --- Spec 12: Execute Cell Completion Buffering ---

    #[test]
    fn execute_in_progress_with_description_artifact_shows_no_output() {
        // Claude sends the tool description in the content array of in-progress
        // execute updates. This description text should NOT be rendered as stdout.
        let snapshot = make_execute_snapshot(
            ToolPhase::InProgress,
            "date --utc +\"%Y-%m-%d %H:%M:%S %Z\"",
            vec![Artifact::Text {
                text: "Print current UTC date/time with format flags".into(),
                format: crate::presentation::TextFormat::Plain,
            }],
            None, // no raw_output yet (in-progress)
        );
        let cell = ClientToolCell::new(snapshot, PathBuf::from("/tmp/test-cwd"), false);
        let lines = render_lines(&cell.display_lines(80));

        // Should show "Running" header with command
        assert!(
            lines[0].contains("Running"),
            "In-progress execute should show 'Running', got: {}",
            lines[0]
        );
        // Should NOT show the description text as output
        assert!(
            !lines.iter().any(|l| l.contains("Print current UTC")),
            "Description text should not be rendered as execute output, got: {lines:?}"
        );
    }

    #[test]
    fn list_files_title_not_duplicated_in_exploring_fallback() {
        // When the kind maps to a label that already prefixes the title,
        // the exploring renderer should not duplicate it.
        let snapshot = ToolSnapshot {
            call_id: "call-list-dup".into(),
            title: "List /home/user/project/src".into(),
            kind: ToolKind::Other("List".into()),
            phase: ToolPhase::Completed,
            locations: vec![],
            invocation: None, // no ListFiles invocation, so hits generic fallback
            artifacts: vec![],
            raw_input: None,
            raw_output: None,
            owner_request_id: None,
        };
        let mut cell = ClientToolCell::new(snapshot, PathBuf::from("/tmp/cwd"), false);
        cell.mark_exploring();
        let lines = render_lines(&cell.display_lines(80));

        // Should NOT show "List List /home/..."
        let has_double_list = lines.iter().any(|l| l.contains("List List"));
        assert!(
            !has_double_list,
            "Should not duplicate 'List' label, got: {lines:?}"
        );
        // Should still show the path
        assert!(
            lines.iter().any(|l| l.contains("/home/user/project/src")),
            "Should still show the path, got: {lines:?}"
        );
    }

    // --- Spec 10: Failed Edit Tool Visibility ---

    fn make_edit_snapshot(
        phase: ToolPhase,
        path: &str,
        artifacts: Vec<Artifact>,
        raw_output: Option<serde_json::Value>,
    ) -> ToolSnapshot {
        ToolSnapshot {
            call_id: "call-edit-1".into(),
            title: format!("Edit {path}"),
            kind: ToolKind::Edit,
            phase,
            locations: vec![crate::presentation::ToolLocation {
                path: PathBuf::from(path),
                line: None,
            }],
            invocation: None,
            artifacts,
            raw_input: None,
            raw_output,
            owner_request_id: None,
        }
    }

    #[test]
    fn failed_edit_has_red_bullet() {
        let snapshot = make_edit_snapshot(ToolPhase::Failed, "README.md", vec![], None);
        let cell = ClientToolCell::new(snapshot, PathBuf::from("/tmp/test-cwd"), false);
        let lines = cell.display_lines(80);

        let bullet_span = &lines[0].spans[0];
        assert!(
            bullet_span.style.fg == Some(ratatui::style::Color::Red),
            "Failed edit should have red bullet, got {:?}",
            bullet_span.style
        );
    }

    #[test]
    fn failed_edit_has_semantic_header() {
        let snapshot = make_edit_snapshot(ToolPhase::Failed, "README.md", vec![], None);
        let cell = ClientToolCell::new(snapshot, PathBuf::from("/tmp/test-cwd"), false);
        let lines = render_lines(&cell.display_lines(80));

        assert!(
            lines[0].contains("Edit failed:"),
            "Failed edit should show 'Edit failed:' header, got: {}",
            lines[0]
        );
        assert!(
            lines[0].contains("README.md"),
            "Failed edit header should include path, got: {}",
            lines[0]
        );
        assert!(
            !lines[0].contains("Tool ["),
            "Failed edit should NOT use generic 'Tool [failed]' header, got: {}",
            lines[0]
        );
    }

    #[test]
    fn failed_delete_has_semantic_header() {
        let snapshot = ToolSnapshot {
            call_id: "call-del-1".into(),
            title: "Delete temp.txt".into(),
            kind: ToolKind::Delete,
            phase: ToolPhase::Failed,
            locations: vec![crate::presentation::ToolLocation {
                path: PathBuf::from("temp.txt"),
                line: None,
            }],
            invocation: None,
            artifacts: vec![],
            raw_input: None,
            raw_output: None,
            owner_request_id: None,
        };
        let cell = ClientToolCell::new(snapshot, PathBuf::from("/tmp/test-cwd"), false);
        let lines = render_lines(&cell.display_lines(80));

        assert!(
            lines[0].contains("Delete failed:"),
            "Failed delete should show 'Delete failed:' header, got: {}",
            lines[0]
        );
        assert!(
            lines[0].contains("temp.txt"),
            "Failed delete header should include path, got: {}",
            lines[0]
        );
    }

    #[test]
    fn in_progress_edit_has_semantic_header() {
        let snapshot = make_edit_snapshot(ToolPhase::InProgress, "src/main.rs", vec![], None);
        let cell = ClientToolCell::new(snapshot, PathBuf::from("/tmp/test-cwd"), false);
        let lines = render_lines(&cell.display_lines(80));

        assert!(
            lines[0].contains("Editing"),
            "In-progress edit should show 'Editing' header, got: {}",
            lines[0]
        );
        assert!(
            lines[0].contains("src/main.rs"),
            "In-progress edit header should include path, got: {}",
            lines[0]
        );
    }

    #[test]
    fn completed_edit_fallthrough_has_semantic_header() {
        // Completed edit that falls through to generic rendering (no file_changes)
        let snapshot = make_edit_snapshot(ToolPhase::Completed, "config.toml", vec![], None);
        let cell = ClientToolCell::new(snapshot, PathBuf::from("/tmp/test-cwd"), false);
        let lines = render_lines(&cell.display_lines(80));

        assert!(
            lines[0].contains("Edited"),
            "Completed edit fallthrough should show 'Edited' header, got: {}",
            lines[0]
        );
        assert!(
            lines[0].contains("config.toml"),
            "Completed edit header should include path, got: {}",
            lines[0]
        );
    }

    #[test]
    fn failed_edit_with_error_in_raw_output_shows_error_text() {
        let snapshot = make_edit_snapshot(
            ToolPhase::Failed,
            "README.md",
            vec![],
            Some(serde_json::json!({"error": "Permission denied: README.md"})),
        );
        let cell = ClientToolCell::new(snapshot, PathBuf::from("/tmp/test-cwd"), false);
        let lines = render_lines(&cell.display_lines(80));

        assert!(
            lines.iter().any(|l| l.contains("Permission denied")),
            "Failed edit with raw_output error should show error text, got: {lines:?}"
        );
    }

    #[test]
    fn failed_edit_with_no_artifacts_shows_failed_fallback() {
        let snapshot = make_edit_snapshot(ToolPhase::Failed, "README.md", vec![], None);
        let cell = ClientToolCell::new(snapshot, PathBuf::from("/tmp/test-cwd"), false);
        let lines = render_lines(&cell.display_lines(80));

        assert!(
            lines.iter().any(|l| l.contains("(failed)")),
            "Failed edit with no artifacts should show '(failed)' fallback, got: {lines:?}"
        );
    }

    #[test]
    fn failed_edit_with_diff_artifact_renders_diff() {
        let snapshot = make_edit_snapshot(
            ToolPhase::Failed,
            "README.md",
            vec![crate::presentation::Artifact::Diff(
                crate::presentation::FileChange {
                    path: PathBuf::from("README.md"),
                    old_text: Some("# Old\n".into()),
                    new_text: "# New\n".into(),
                },
            )],
            None,
        );
        let cell = ClientToolCell::new(snapshot, PathBuf::from("/tmp/test-cwd"), false);
        let lines = render_lines(&cell.display_lines(80));

        // Should have red bullet (failed)
        let raw_lines = cell.display_lines(80);
        let bullet_span = &raw_lines[0].spans[0];
        assert!(
            bullet_span.style.fg == Some(ratatui::style::Color::Red),
            "Failed edit with diff should still have red bullet"
        );

        // Should render diff content — look for the changed file path
        // and the actual content from old/new text
        assert!(
            lines.iter().any(|l| l.contains("README.md")),
            "Failed edit with diff artifact should render file path, got: {lines:?}"
        );
    }

    #[test]
    fn completed_non_edit_tool_still_has_dim_bullet() {
        let snapshot = ToolSnapshot {
            call_id: "call-fetch-1".into(),
            title: "Fetch resource".into(),
            kind: ToolKind::Fetch,
            phase: ToolPhase::Completed,
            locations: vec![],
            invocation: None,
            artifacts: vec![Artifact::Text {
                text: "200 OK".into(),
                format: crate::presentation::TextFormat::Plain,
            }],
            raw_input: None,
            raw_output: None,
            owner_request_id: None,
        };
        let cell = ClientToolCell::new(snapshot, PathBuf::from("/tmp/test-cwd"), false);
        let lines = cell.display_lines(80);

        let bullet_span = &lines[0].spans[0];
        // Should NOT be red (it's completed, not failed)
        assert!(
            bullet_span.style.fg != Some(ratatui::style::Color::Red),
            "Completed non-edit tool should NOT have red bullet, got {:?}",
            bullet_span.style
        );
    }

    #[test]
    fn failed_move_has_semantic_header() {
        let snapshot = ToolSnapshot {
            call_id: "call-move-1".into(),
            title: "Move old.rs to new.rs".into(),
            kind: ToolKind::Move,
            phase: ToolPhase::Failed,
            locations: vec![crate::presentation::ToolLocation {
                path: PathBuf::from("old.rs"),
                line: None,
            }],
            invocation: None,
            artifacts: vec![],
            raw_input: None,
            raw_output: None,
            owner_request_id: None,
        };
        let cell = ClientToolCell::new(snapshot, PathBuf::from("/tmp/test-cwd"), false);
        let lines = render_lines(&cell.display_lines(80));

        assert!(
            lines[0].contains("Move failed:"),
            "Failed move should show 'Move failed:' header, got: {}",
            lines[0]
        );
    }

    // --- Spec 11: Delete File Operation Bridge ---

    #[test]
    fn completed_delete_has_green_bullet_and_semantic_header() {
        let snapshot = ToolSnapshot {
            call_id: "call-del-1".into(),
            title: "Delete temp.txt".into(),
            kind: ToolKind::Delete,
            phase: ToolPhase::Completed,
            locations: vec![crate::presentation::ToolLocation {
                path: PathBuf::from("temp.txt"),
                line: None,
            }],
            invocation: Some(crate::presentation::Invocation::FileOperations {
                operations: vec![crate::presentation::FileOperation::Delete {
                    path: PathBuf::from("temp.txt"),
                    old_text: Some("old content\n".into()),
                }],
            }),
            artifacts: vec![],
            raw_input: None,
            raw_output: None,
            owner_request_id: None,
        };
        let cell = ClientToolCell::new(snapshot, PathBuf::from("/tmp/test-cwd"), false);
        let raw_lines = cell.display_lines(80);
        let lines = render_lines(&raw_lines);

        // Green bullet for completed delete
        let bullet_span = &raw_lines[0].spans[0];
        assert!(
            bullet_span.style.fg == Some(ratatui::style::Color::Green),
            "Completed delete should have green bullet, got {:?}",
            bullet_span.style
        );

        // Semantic header
        assert!(
            lines[0].contains("Deleted"),
            "Completed delete should show 'Deleted' header, got: {}",
            lines[0]
        );
        assert!(
            lines[0].contains("temp.txt"),
            "Completed delete header should include path, got: {}",
            lines[0]
        );
    }

    #[test]
    fn completed_move_has_green_bullet_and_semantic_header() {
        let snapshot = ToolSnapshot {
            call_id: "call-move-2".into(),
            title: "Move old.rs to new.rs".into(),
            kind: ToolKind::Move,
            phase: ToolPhase::Completed,
            locations: vec![crate::presentation::ToolLocation {
                path: PathBuf::from("old.rs"),
                line: None,
            }],
            invocation: Some(crate::presentation::Invocation::FileOperations {
                operations: vec![crate::presentation::FileOperation::Move {
                    from_path: PathBuf::from("old.rs"),
                    to_path: PathBuf::from("new.rs"),
                    old_text: Some("content\n".into()),
                    new_text: Some("content\n".into()),
                }],
            }),
            artifacts: vec![],
            raw_input: None,
            raw_output: None,
            owner_request_id: None,
        };
        let cell = ClientToolCell::new(snapshot, PathBuf::from("/tmp/test-cwd"), false);
        let raw_lines = cell.display_lines(80);
        let lines = render_lines(&raw_lines);

        // Green bullet for completed move
        let bullet_span = &raw_lines[0].spans[0];
        assert!(
            bullet_span.style.fg == Some(ratatui::style::Color::Green),
            "Completed move should have green bullet, got {:?}",
            bullet_span.style
        );

        // Semantic header
        assert!(
            lines[0].contains("Moved"),
            "Completed move should show 'Moved' header, got: {}",
            lines[0]
        );
    }

    #[test]
    fn completed_edit_with_file_operations_invocation_renders_diff() {
        // When a completed edit has FileOperations invocation (no Artifact::Diff),
        // it should still render diff content from the invocation data.
        let snapshot = ToolSnapshot {
            call_id: "call-edit-ops".into(),
            title: "Edit src/lib.rs".into(),
            kind: ToolKind::Edit,
            phase: ToolPhase::Completed,
            locations: vec![crate::presentation::ToolLocation {
                path: PathBuf::from("src/lib.rs"),
                line: None,
            }],
            invocation: Some(crate::presentation::Invocation::FileOperations {
                operations: vec![crate::presentation::FileOperation::Update {
                    path: PathBuf::from("src/lib.rs"),
                    old_text: "fn old() {}\n".into(),
                    new_text: "fn new() {}\n".into(),
                }],
            }),
            artifacts: vec![],
            raw_input: None,
            raw_output: None,
            owner_request_id: None,
        };
        let cell = ClientToolCell::new(snapshot, PathBuf::from("/tmp/test-cwd"), false);
        let lines = render_lines(&cell.display_lines(80));

        // Should show "Edited" header
        assert!(
            lines[0].contains("Edited"),
            "Completed edit with FileOperations should show 'Edited', got: {}",
            lines[0]
        );

        // Should render diff content from FileOperations
        let has_diff = lines
            .iter()
            .any(|l| l.contains("lib.rs") || l.contains('+') || l.contains('-'));
        assert!(
            has_diff,
            "Completed edit with FileOperations should render diff content, got: {lines:?}"
        );
    }

    #[test]
    fn file_operations_update_diff_uses_file_line_numbers() {
        let dir = tempfile::tempdir().expect("temp dir");
        write_updated_rust_fixture(dir.path(), "src/lib.rs", 50, "fn line_50_updated() {}");

        let changes = changes_from_invocation(
            &Some(crate::presentation::Invocation::FileOperations {
                operations: vec![crate::presentation::FileOperation::Update {
                    path: PathBuf::from("src/lib.rs"),
                    old_text: "fn line_50() {}\n".into(),
                    new_text: "fn line_50_updated() {}\n".into(),
                }],
            }),
            dir.path(),
        );

        assert_eq!(
            changes.get(&PathBuf::from("src/lib.rs")),
            Some(&crate::ui_types::FileChange::Update {
                old_text: "fn line_50() {}\n".into(),
                new_text: "fn line_50_updated() {}\n".into(),
                first_line: 50,
                move_path: None,
            })
        );
    }

    #[test]
    fn file_operations_move_diff_uses_file_line_numbers() {
        let dir = tempfile::tempdir().expect("temp dir");
        write_updated_rust_fixture(dir.path(), "src/old.rs", 75, "fn renamed_line_75() {}");

        let changes = changes_from_invocation(
            &Some(crate::presentation::Invocation::FileOperations {
                operations: vec![crate::presentation::FileOperation::Move {
                    from_path: PathBuf::from("src/old.rs"),
                    to_path: PathBuf::from("src/new.rs"),
                    old_text: Some("fn line_75() {}\n".into()),
                    new_text: Some("fn renamed_line_75() {}\n".into()),
                }],
            }),
            dir.path(),
        );

        assert_eq!(
            changes.get(&PathBuf::from("src/old.rs")),
            Some(&crate::ui_types::FileChange::Update {
                old_text: "fn line_75() {}\n".into(),
                new_text: "fn renamed_line_75() {}\n".into(),
                first_line: 75,
                move_path: Some(PathBuf::from("src/new.rs")),
            })
        );
    }

    #[test]
    fn edit_cell_keeps_contextual_diff_when_file_disappears_before_render() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file_path =
            write_updated_rust_fixture(dir.path(), "src/lib.rs", 50, "fn line_50_updated() {}");

        let snapshot = ToolSnapshot {
            call_id: "call-edit-ops".into(),
            title: "Edit src/lib.rs".into(),
            kind: ToolKind::Edit,
            phase: ToolPhase::Completed,
            locations: vec![crate::presentation::ToolLocation {
                path: PathBuf::from("src/lib.rs"),
                line: None,
            }],
            invocation: Some(crate::presentation::Invocation::FileOperations {
                operations: vec![crate::presentation::FileOperation::Update {
                    path: PathBuf::from("src/lib.rs"),
                    old_text: "fn line_50() {}\n".into(),
                    new_text: "fn line_50_updated() {}\n".into(),
                }],
            }),
            artifacts: vec![],
            raw_input: None,
            raw_output: None,
            owner_request_id: None,
        };
        let cell = ClientToolCell::new(snapshot, dir.path().to_path_buf(), false);
        std::fs::remove_file(&file_path).expect("remove file before render");

        let lines = render_lines(&cell.display_lines(80));

        assert!(
            lines.iter().any(|line| line.contains("50 -fn line_50")),
            "expected cached contextual line number in rendered diff, got: {lines:?}"
        );
    }

    // --- Spec 13: Deduplicate single-file edit header ---

    #[test]
    fn single_file_edit_shows_verb_path_counts_once() {
        let snapshot = ToolSnapshot {
            call_id: "call-dedup".into(),
            title: "Edit README.md".into(),
            kind: ToolKind::Edit,
            phase: ToolPhase::Completed,
            locations: vec![crate::presentation::ToolLocation {
                path: PathBuf::from("README.md"),
                line: None,
            }],
            invocation: None,
            artifacts: vec![crate::presentation::Artifact::Diff(
                crate::presentation::FileChange {
                    path: PathBuf::from("README.md"),
                    old_text: Some("# Old Title\n".into()),
                    new_text: "# New Title\n".into(),
                },
            )],
            raw_input: None,
            raw_output: None,
            owner_request_id: None,
        };
        let cell = ClientToolCell::new(snapshot, PathBuf::from("/tmp/test-cwd"), false);
        let lines = render_lines(&cell.display_lines(80));

        // Count how many lines contain "Edited README.md" — should be exactly 1
        let edit_header_count = lines
            .iter()
            .filter(|l| l.contains("Edited") && l.contains("README.md"))
            .count();
        assert_eq!(
            edit_header_count, 1,
            "Single-file edit should show 'Edited README.md' exactly once, got {edit_header_count} in: {lines:?}"
        );
        // The single header should include line counts
        let header_with_counts = lines
            .iter()
            .find(|l| l.contains("Edited") && l.contains("README.md"))
            .expect("should have an edit header");
        assert!(
            header_with_counts.contains("+1") && header_with_counts.contains("-1"),
            "Single-file edit header should include line counts, got: {header_with_counts}"
        );
    }

    #[test]
    fn multi_file_edit_uses_aggregate_header() {
        let snapshot = ToolSnapshot {
            call_id: "call-multi-edit".into(),
            title: "Edit multiple files".into(),
            kind: ToolKind::Edit,
            phase: ToolPhase::Completed,
            locations: vec![
                crate::presentation::ToolLocation {
                    path: PathBuf::from("README.md"),
                    line: None,
                },
                crate::presentation::ToolLocation {
                    path: PathBuf::from("src/lib.rs"),
                    line: None,
                },
            ],
            invocation: None,
            artifacts: vec![
                crate::presentation::Artifact::Diff(crate::presentation::FileChange {
                    path: PathBuf::from("README.md"),
                    old_text: Some("# Old\n".into()),
                    new_text: "# New\n".into(),
                }),
                crate::presentation::Artifact::Diff(crate::presentation::FileChange {
                    path: PathBuf::from("src/lib.rs"),
                    old_text: Some("fn old() {}\n".into()),
                    new_text: "fn new() {}\n".into(),
                }),
            ],
            raw_input: None,
            raw_output: None,
            owner_request_id: None,
        };
        let cell = ClientToolCell::new(snapshot, PathBuf::from("/tmp/test-cwd"), false);
        let lines = render_lines(&cell.display_lines(80));

        // First line should be the aggregate header with file count
        assert!(
            lines[0].contains("Edited") && lines[0].contains("2 files"),
            "Multi-file edit header should show 'Edited 2 files' as the first line, got: {:?}",
            lines[0]
        );
        // No subsequent line should repeat the aggregate header
        let has_duplicate_aggregate = lines[1..]
            .iter()
            .any(|l| l.contains("Edited") && l.contains("files"));
        assert!(
            !has_duplicate_aggregate,
            "Should not have a duplicate aggregate header in: {lines:?}"
        );
    }

    // --- Edit cell indentation should match PatchHistoryCell (4 spaces, not 8) ---

    #[test]
    fn edit_cell_diff_lines_have_single_indent() {
        let snapshot = ToolSnapshot {
            call_id: "call-edit-indent".into(),
            title: "Write README.md".into(),
            kind: ToolKind::Edit,
            phase: ToolPhase::Completed,
            locations: vec![crate::presentation::ToolLocation {
                path: PathBuf::from("README.md"),
                line: None,
            }],
            invocation: Some(Invocation::FileChanges {
                changes: vec![crate::presentation::FileChange {
                    path: PathBuf::from("README.md"),
                    old_text: None,
                    new_text: "hello\nworld\n".into(),
                }],
            }),
            artifacts: vec![crate::presentation::Artifact::Diff(
                crate::presentation::FileChange {
                    path: PathBuf::from("README.md"),
                    old_text: None,
                    new_text: "hello\nworld\n".into(),
                },
            )],
            raw_input: None,
            raw_output: None,
            owner_request_id: None,
        };
        let cell = ClientToolCell::new(snapshot, PathBuf::from("/"), false);
        let lines = render_lines(&cell.display_lines(80));

        // The diff content lines (containing "+hello", "+world") should start
        // with exactly 4 spaces, not 8. This matches PatchHistoryCell's output.
        let diff_content: Vec<_> = lines
            .iter()
            .filter(|l| l.contains("+hello") || l.contains("+world"))
            .collect();
        assert!(
            !diff_content.is_empty(),
            "expected diff content lines, got: {lines:?}"
        );
        for line in &diff_content {
            assert!(
                line.starts_with("    ") && !line.starts_with("        "),
                "diff content should have 4-space indent (not 8): {line:?}"
            );
        }
    }

    #[test]
    fn move_cell_diff_lines_have_single_indent() {
        let snapshot = ToolSnapshot {
            call_id: "call-move".into(),
            title: "Move old.rs".into(),
            kind: ToolKind::Move,
            phase: ToolPhase::Completed,
            locations: vec![crate::presentation::ToolLocation {
                path: PathBuf::from("old.rs"),
                line: None,
            }],
            invocation: Some(Invocation::FileOperations {
                operations: vec![crate::presentation::FileOperation::Move {
                    from_path: PathBuf::from("old.rs"),
                    to_path: PathBuf::from("new.rs"),
                    old_text: None,
                    new_text: Some("content\n".into()),
                }],
            }),
            artifacts: vec![],
            raw_input: None,
            raw_output: None,
            owner_request_id: None,
        };
        let cell = ClientToolCell::new(snapshot, PathBuf::from("/"), false);
        let lines = render_lines(&cell.display_lines(80));

        // Header should say "Moved", not "Edited"
        assert!(
            lines[0].contains("Moved"),
            "move header should say 'Moved', got: {}",
            lines[0]
        );

        // Diff content lines should have 4-space indent, not 8
        let diff_content: Vec<_> = lines.iter().filter(|l| l.contains("+content")).collect();
        assert!(
            !diff_content.is_empty(),
            "expected diff content lines for move, got: {lines:?}"
        );
        for line in &diff_content {
            assert!(
                line.starts_with("    ") && !line.starts_with("        "),
                "move diff content should have 4-space indent (not 8): {line:?}"
            );
        }
    }

    // --- Bug fix: Read/Search should auto-detect as exploring ---

    #[test]
    fn standalone_read_renders_as_explored_not_generic() {
        // A Read tool that is NOT explicitly mark_exploring() should still render
        // using the exploring format ("Explored") when it's a completed Read,
        // not the generic "Tool [completed]" format.
        let snapshot = ToolSnapshot {
            call_id: "call-read-auto".into(),
            title: "Read README.md".into(),
            kind: ToolKind::Read,
            phase: ToolPhase::Completed,
            locations: vec![],
            invocation: Some(Invocation::Read {
                path: "README.md".into(),
            }),
            artifacts: vec![],
            raw_input: None,
            raw_output: None,
            owner_request_id: None,
        };
        let cell = ClientToolCell::new(snapshot, PathBuf::from("/tmp/cwd"), false);
        let lines = render_lines(&cell.display_lines(80));

        assert!(
            lines[0].contains("Explored"),
            "Standalone completed Read should auto-render as 'Explored', got: {}",
            lines[0]
        );
        assert!(
            !lines[0].contains("Tool ["),
            "Standalone Read should NOT use generic format, got: {}",
            lines[0]
        );
    }

    #[test]
    fn standalone_search_renders_as_explored_not_generic() {
        let snapshot = ToolSnapshot {
            call_id: "call-search-auto".into(),
            title: "Search TODO".into(),
            kind: ToolKind::Search,
            phase: ToolPhase::Completed,
            locations: vec![],
            invocation: Some(Invocation::Search {
                query: Some("TODO".into()),
                path: Some("/repo/src".into()),
            }),
            artifacts: vec![],
            raw_input: None,
            raw_output: None,
            owner_request_id: None,
        };
        let cell = ClientToolCell::new(snapshot, PathBuf::from("/tmp/cwd"), false);
        let lines = render_lines(&cell.display_lines(80));

        assert!(
            lines[0].contains("Explored"),
            "Standalone completed Search should auto-render as 'Explored', got: {}",
            lines[0]
        );
    }

    // --- Bug fix: Execute transcript should use $ command format ---

    #[test]
    fn execute_transcript_uses_shell_format() {
        // In transcript view, Execute tools should render as "$ command"
        // (shell-style), not "• Ran command" (bullet-style).
        let snapshot = make_execute_snapshot(
            ToolPhase::Completed,
            "date --utc",
            vec![Artifact::Text {
                text: "2026-03-30".into(),
                format: crate::presentation::TextFormat::Plain,
            }],
            Some(serde_json::json!({"exit_code": 0, "stdout": "2026-03-30"})),
        );
        let cell = ClientToolCell::new(snapshot, PathBuf::from("/tmp/cwd"), false);
        let lines = render_lines(&cell.transcript_lines(80));

        let full = lines.join("\n");
        assert!(full.contains("date --utc"));
        assert!(full.contains("2026-03-30"));
        assert!(!full.contains("Tool ["));
    }

    // --- Bug fix: Exploring transcript should use exploring format ---

    #[test]
    fn exploring_transcript_uses_explored_format_not_shell() {
        // In transcript view, Read/Search tools should render as "Explored"
        // with sub-items, NOT as "$ Read /path" (shell-style).
        let snap = ToolSnapshot {
            call_id: "call-read-t".into(),
            title: "Read file.rs".into(),
            kind: ToolKind::Read,
            phase: ToolPhase::Completed,
            locations: vec![],
            invocation: Some(Invocation::Read {
                path: "file.rs".into(),
            }),
            artifacts: vec![],
            raw_input: None,
            raw_output: None,
            owner_request_id: None,
        };
        let mut cell = ClientToolCell::new(snap, PathBuf::from("/tmp/cwd"), false);
        cell.mark_exploring();
        let lines = render_lines(&cell.transcript_lines(80));

        assert!(
            lines[0].contains("Explored"),
            "Exploring transcript should show 'Explored', got: {}",
            lines[0]
        );
        assert!(
            !lines.iter().any(|l| l.starts_with("$ ")),
            "Exploring transcript should NOT use '$ ' shell format, got: {lines:?}"
        );
    }

    fn native_snapshot(
        title: &str,
        input: serde_json::Value,
        phase: ToolPhase,
        result: Option<&str>,
    ) -> ToolSnapshot {
        let call = crate::presentation::NativeCall::recognize(title, Some(&input))
            .expect("a native tool title");
        ToolSnapshot {
            call_id: "native-1".into(),
            title: title.into(),
            kind: ToolKind::Other("Other".into()),
            phase,
            locations: vec![],
            invocation: Some(Invocation::Native(call)),
            artifacts: result
                .map(|text| {
                    vec![Artifact::Text {
                        text: text.into(),
                        format: crate::presentation::TextFormat::Plain,
                    }]
                })
                .unwrap_or_default(),
            raw_input: Some(input),
            raw_output: None,
            owner_request_id: None,
        }
    }

    #[test]
    fn git_status_reads_as_branch_and_changes_not_as_a_tool_call() {
        let snapshot = native_snapshot(
            "mcp__nori-client__git_status",
            serde_json::json!({}),
            ToolPhase::Completed,
            Some(
                r#"{"branch":"main","changes":[{"path":"a.rs","state":"modified"},{"path":"b.rs","state":"added"}]}"#,
            ),
        );
        let cell = ClientToolCell::new(snapshot, PathBuf::from("/tmp/test-cwd"), false);
        let lines = render_lines(&cell.display_lines(80));
        assert_eq!(
            lines,
            vec![
                "⎇ Git status  main".to_string(),
                "  ├ 2 changes".to_string(),
                "  ├ M a.rs".to_string(),
                "  └ A b.rs".to_string(),
            ]
        );
        assert!(
            !lines
                .iter()
                .any(|line| line.contains("Tool [") || line.contains("(Other)")),
            "no raw tool-call chrome: {lines:?}"
        );
    }

    #[test]
    fn git_commit_reads_as_the_commit_it_made_and_a_running_one_says_so() {
        let input = serde_json::json!({"paths": ["math.py"], "message": "feat(math): add sub"});
        let done = ClientToolCell::new(
            native_snapshot(
                "mcp__nori-client__git_commit",
                input.clone(),
                ToolPhase::Completed,
                Some(r#"{"commit":"07a5d0f9c0ffee","remaining_changes":0}"#),
            ),
            PathBuf::from("/tmp/test-cwd"),
            false,
        );
        assert_eq!(
            render_lines(&done.display_lines(80)),
            vec![
                "◆ Committed 07a5d0f  feat(math): add sub".to_string(),
                "  ├ math.py".to_string(),
                "  └ ✓ nothing left uncommitted".to_string(),
            ]
        );

        let running = ClientToolCell::new(
            native_snapshot(
                "mcp__nori-client__git_commit",
                input,
                ToolPhase::InProgress,
                None,
            ),
            PathBuf::from("/tmp/test-cwd"),
            false,
        );
        let lines = render_lines(&running.display_lines(80));
        assert!(
            lines[0].ends_with("Committing 1 file  feat(math): add sub"),
            "{lines:?}"
        );
    }

    #[test]
    fn git_diff_reads_as_files_with_their_line_counts() {
        let diff = "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1,2 @@\n-old\n+new\n+more\ndiff --git a/notes.md b/notes.md\n--- /dev/null\n+++ b/notes.md\n@@ -0,0 +1 @@\n+hello\n";
        let cell = ClientToolCell::new(
            native_snapshot(
                "mcp__nori-client__git_diff",
                serde_json::json!({}),
                ToolPhase::Completed,
                Some(diff),
            ),
            PathBuf::from("/tmp/test-cwd"),
            false,
        );
        insta::assert_snapshot!(render_lines(&cell.display_lines(80)).join("\n"));
    }

    #[test]
    fn git_log_reads_as_the_recent_commits() {
        let log = r#"{"branch":"perf/instant-core","commits":[{"id":"87057b7961de821e","summary":"docs: brief for host-provided tools"},{"id":"24371e4d0000aaaa","summary":"feat(claude): offer mode and model as config options"},{"id":"3b8cb9320000bbbb","summary":"feat(codex): update the embedded Codex to rust-v0.169.0"}]}"#;
        let cell = ClientToolCell::new(
            native_snapshot(
                "mcp__nori-client__git_log",
                serde_json::json!({"limit": 3}),
                ToolPhase::Completed,
                Some(log),
            ),
            PathBuf::from("/tmp/test-cwd"),
            false,
        );
        insta::assert_snapshot!(render_lines(&cell.display_lines(80)).join("\n"));
    }

    #[test]
    fn ci_status_names_what_failed_and_why() {
        let status = r#"{"repo":"o/r","branch":"perf/instant-core","pushed":"abc","head_pushed":true,"pull_request":{"number":412,"title":"Instant agent switching","url":"https://github.com/o/r/pull/412","state":"open","draft":false,"base":"main","head":"perf/instant-core","head_sha":"abc"},"checks":{"state":"failing","passed":7,"failed":[{"name":"Run Tests","state":"failed","log":"error[E0308]: mismatched types\nerror: Process completed with exit code 101.\n"}],"running":[{"name":"Build Native (aarch64-apple-darwin)","state":"running"}]}}"#;
        let cell = ClientToolCell::new(
            native_snapshot(
                "mcp__nori-client__ci_status",
                serde_json::json!({"logs": true}),
                ToolPhase::Completed,
                Some(status),
            ),
            PathBuf::from("/tmp/test-cwd"),
            false,
        );
        insta::assert_snapshot!(render_lines(&cell.display_lines(80)).join("\n"));
    }

    #[test]
    fn an_opened_pull_request_links_to_itself() {
        let opened = r#"{"created":true,"pull_request":{"number":413,"title":"feat(forge): CI status and pull requests","url":"https://github.com/o/r/pull/413","state":"open","draft":true,"base":"main","head":"feat/forge","head_sha":"abc"}}"#;
        let cell = ClientToolCell::new(
            native_snapshot(
                "mcp__nori-client__pull_request",
                serde_json::json!({"title": "feat(forge): CI status and pull requests", "body": "## Why"}),
                ToolPhase::Completed,
                Some(opened),
            ),
            PathBuf::from("/tmp/test-cwd"),
            false,
        );
        insta::assert_snapshot!(render_lines(&cell.display_lines(80)).join("\n"));
    }

    #[test]
    fn a_refused_pull_request_lists_the_broken_rules() {
        let refusal = "The \"land\" workflow's pull request rules are not met:\n- The description has no \"Testing\" section.\n- It changes 812 lines; at most 400 are allowed: split the work into a series of pull requests.\nFix this, then call pull_request again.";
        let cell = ClientToolCell::new(
            native_snapshot(
                "mcp__nori-client__pull_request",
                serde_json::json!({"title": "t", "body": "b"}),
                ToolPhase::Failed,
                Some(refusal),
            ),
            PathBuf::from("/tmp/test-cwd"),
            false,
        );
        insta::assert_snapshot!(render_lines(&cell.display_lines(80)).join("\n"));
    }

    #[test]
    fn a_failed_commit_shows_why() {
        let cell = ClientToolCell::new(
            native_snapshot(
                "mcp__nori-client__git_commit",
                serde_json::json!({"paths": ["a"], "message": "x"}),
                ToolPhase::Failed,
                Some("none of those paths differ from HEAD"),
            ),
            PathBuf::from("/tmp/test-cwd"),
            false,
        );
        assert_eq!(
            render_lines(&cell.display_lines(80)),
            vec![
                "◆ Commit failed".to_string(),
                "  └ none of those paths differ from HEAD".to_string(),
            ]
        );
    }
}
