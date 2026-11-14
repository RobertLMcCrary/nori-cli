//! File changes drawn as diffs, in the style Codex uses: a `• Edited path (+N -M)` header, then
//! each file's lines under a right-aligned line-number gutter with `+`/`-` signs, syntax
//! colours, and tinted backgrounds. Edits are diffed with disparate ([`update`]), and the words
//! or tokens that changed within an edited line are emphasised ([`line`], [`style`]).

mod line;
mod style;
mod update;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Stylize;
use ratatui::text::Line as RtLine;
use ratatui::text::Span as RtSpan;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

use self::line::DiffLineLayout;
use self::line::DiffLineType;
use self::line::content_spans;
use self::line::diff_line_rows;
use self::line::line_number_width;
use self::style::DiffRenderStyleContext;
use self::update::EditDiff;
use crate::exec_command::relativize_to_home;
use crate::render::Insets;
use crate::render::highlight::highlight_code_to_lines_for_path;
use crate::render::line_utils::prefix_lines;
use crate::render::renderable::ColumnRenderable;
use crate::render::renderable::InsetRenderable;
use crate::render::renderable::Renderable;
use crate::ui_types::FileChange;

pub struct DiffSummary {
    changes: HashMap<PathBuf, FileChange>,
    cwd: PathBuf,
}

impl DiffSummary {
    pub(crate) fn new(changes: HashMap<PathBuf, FileChange>, cwd: PathBuf) -> Self {
        Self { changes, cwd }
    }
}

impl From<DiffSummary> for Box<dyn Renderable> {
    fn from(val: DiffSummary) -> Self {
        let mut rows: Vec<Box<dyn Renderable>> = vec![];

        for (i, row) in collect_rows(&val.changes).into_iter().enumerate() {
            if i > 0 {
                rows.push(Box::new(RtLine::from("")));
            }
            let mut path = RtLine::from(display_path_for(row.path, &val.cwd));
            path.push_span(" ");
            path.extend(render_line_count_summary(row.added, row.removed));
            rows.push(Box::new(path));
            rows.push(Box::new(RtLine::from("")));
            rows.push(Box::new(InsetRenderable::new(
                Box::new(PathAwareFileChange {
                    path: row.path.to_path_buf(),
                    change: row.change.clone(),
                }) as Box<dyn Renderable>,
                Insets::tlbr(0, 2, 0, 0),
            )));
        }

        Box::new(ColumnRenderable::with(rows))
    }
}

struct PathAwareFileChange {
    path: PathBuf,
    change: FileChange,
}

impl Renderable for PathAwareFileChange {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        let mut lines = vec![];
        let row = Row::new(&self.path, &self.change);
        let ctx = DiffRenderStyleContext::detected();
        render_body(&row, &mut lines, area.width as usize, &ctx);
        Paragraph::new(lines).render(area, buf);
    }

    fn desired_height(&self, width: u16) -> u16 {
        let mut lines = vec![];
        let row = Row::new(&self.path, &self.change);
        let ctx = DiffRenderStyleContext::detected();
        render_body(&row, &mut lines, width as usize, &ctx);
        lines.len() as u16
    }
}

pub(crate) fn create_diff_summary(
    changes: &HashMap<PathBuf, FileChange>,
    cwd: &Path,
    wrap_cols: usize,
) -> Vec<RtLine<'static>> {
    let rows = collect_rows(changes);
    render_changes_block(rows, wrap_cols, cwd, &DiffRenderStyleContext::detected())
}

// Shared row for per-file presentation
struct Row<'a> {
    path: &'a Path,
    change: &'a FileChange,
    move_path: Option<&'a Path>,
    /// The path whose language the file is read in: its new name, when it moved.
    syntax_path: &'a Path,
    added: usize,
    removed: usize,
    body: Body<'a>,
}

/// What a file's change shows: all of its lines, or an edit's hunks.
enum Body<'a> {
    Added(&'a str),
    Deleted(&'a str),
    /// An update, diffed once for both its counts and its lines.
    Edited {
        diff: EditDiff<'a>,
        first_line: usize,
    },
}

impl<'a> Row<'a> {
    fn new(path: &'a Path, change: &'a FileChange) -> Self {
        let move_path = match change {
            FileChange::Update { move_path, .. } => move_path.as_deref(),
            FileChange::Add { .. } | FileChange::Delete { .. } => None,
        };
        let syntax_path = move_path.unwrap_or(path);
        let (added, removed, body) = match change {
            FileChange::Add { content } => (content.lines().count(), 0, Body::Added(content)),
            FileChange::Delete { content } => (0, content.lines().count(), Body::Deleted(content)),
            FileChange::Update {
                old_text,
                new_text,
                first_line,
                ..
            } => {
                let diff = EditDiff::new(old_text, new_text, syntax_path);
                let changed = diff.changed();
                let first_line = *first_line;
                (
                    changed.after,
                    changed.before,
                    Body::Edited { diff, first_line },
                )
            }
        };
        Self {
            path,
            change,
            move_path,
            syntax_path,
            added,
            removed,
            body,
        }
    }
}

fn collect_rows(changes: &HashMap<PathBuf, FileChange>) -> Vec<Row<'_>> {
    let mut rows: Vec<Row<'_>> = changes
        .iter()
        .map(|(path, change)| Row::new(path, change))
        .collect();
    rows.sort_by_key(|r| r.path);
    rows
}

fn render_line_count_summary(added: usize, removed: usize) -> Vec<RtSpan<'static>> {
    let mut spans = Vec::new();
    spans.push("(".into());
    spans.push(format!("+{added}").green());
    spans.push(" ".into());
    spans.push(format!("-{removed}").red());
    spans.push(")".into());
    spans
}

fn render_changes_block(
    rows: Vec<Row<'_>>,
    wrap_cols: usize,
    cwd: &Path,
    ctx: &DiffRenderStyleContext,
) -> Vec<RtLine<'static>> {
    let mut out: Vec<RtLine<'static>> = Vec::new();

    let render_path = |row: &Row<'_>| -> Vec<RtSpan<'static>> {
        let mut spans = Vec::new();
        spans.push(display_path_for(row.path, cwd).into());
        if let Some(move_path) = row.move_path {
            spans.push(format!(" → {}", display_path_for(move_path, cwd)).into());
        }
        spans
    };

    // Header
    let total_added: usize = rows.iter().map(|r| r.added).sum();
    let total_removed: usize = rows.iter().map(|r| r.removed).sum();
    let file_count = rows.len();
    let noun = if file_count == 1 { "file" } else { "files" };
    let mut header_spans: Vec<RtSpan<'static>> = vec!["• ".dim()];
    if let [row] = &rows[..] {
        let verb = match &row.body {
            Body::Added(_) => "Added",
            Body::Deleted(_) => "Deleted",
            Body::Edited { .. } => "Edited",
        };
        header_spans.push(verb.bold());
        header_spans.push(" ".into());
        header_spans.extend(render_path(row));
        header_spans.push(" ".into());
        header_spans.extend(render_line_count_summary(row.added, row.removed));
    } else {
        header_spans.push("Edited".bold());
        header_spans.push(format!(" {file_count} {noun} ").into());
        header_spans.extend(render_line_count_summary(total_added, total_removed));
    }
    out.push(RtLine::from(header_spans));

    for (idx, r) in rows.iter().enumerate() {
        // Insert a blank separator between file chunks (except before the first)
        if idx > 0 {
            out.push("".into());
        }
        // File header line (skip when single-file header already shows the name)
        let skip_file_header = file_count == 1;
        if !skip_file_header {
            let mut header: Vec<RtSpan<'static>> = Vec::new();
            header.push("  └ ".dim());
            header.extend(render_path(r));
            header.push(" ".into());
            header.extend(render_line_count_summary(r.added, r.removed));
            out.push(RtLine::from(header));
        }

        let mut lines = vec![];
        let prefix_width = 4;
        render_body(
            r,
            &mut lines,
            wrap_cols.saturating_sub(prefix_width).max(1),
            ctx,
        );
        out.extend(prefix_lines(lines, "    ".into(), "    ".into()));
    }

    out
}

/// The lines of one file's change, within `width`.
fn render_body(
    row: &Row<'_>,
    out: &mut Vec<RtLine<'static>>,
    width: usize,
    ctx: &DiffRenderStyleContext,
) {
    let (kind, content) = match &row.body {
        Body::Added(content) => (DiffLineType::Insert, *content),
        Body::Deleted(content) => (DiffLineType::Delete, *content),
        Body::Edited { diff, first_line } => {
            return diff.render(*first_line, row.syntax_path, width, ctx, out);
        }
    };
    let syntax_lines = highlighted_diff_lines_for_path(content, row.syntax_path);
    let layout = DiffLineLayout {
        width,
        line_number_width: line_number_width(content.lines().count()),
    };
    for (i, raw) in content.lines().enumerate() {
        let syntax = syntax_lines
            .as_ref()
            .and_then(|lines| lines.get(i))
            .map(Vec::as_slice);
        let spans = content_spans(kind, raw, syntax, &[], ctx);
        out.extend(diff_line_rows(i + 1, kind, spans, layout, ctx));
    }
}

pub(crate) fn display_path_for(path: &Path, cwd: &Path) -> String {
    let path_in_same_repo = match (
        nori_config::resolve_root_git_project_for_trust(cwd),
        nori_config::resolve_root_git_project_for_trust(path),
    ) {
        (Some(cwd_repo), Some(path_repo)) => cwd_repo == path_repo,
        _ => false,
    };
    let chosen = if path_in_same_repo {
        pathdiff::diff_paths(path, cwd).unwrap_or_else(|| path.to_path_buf())
    } else {
        relativize_to_home(path)
            .map(|p| PathBuf::from_iter([Path::new("~"), p.as_path()]))
            .unwrap_or_else(|| path.to_path_buf())
    };
    chosen.display().to_string()
}

/// The highlighter's spans for each line of `code`, or `None` when it has no colours for the
/// language (plain text is drawn in the line's own colour).
fn highlighted_diff_lines_for_path(code: &str, path: &Path) -> Option<Vec<Vec<RtSpan<'static>>>> {
    if code.is_empty() {
        return None;
    }

    let lines = highlight_code_to_lines_for_path(code, path);
    let has_syntax_style = lines.iter().flat_map(|line| &line.spans).any(|span| {
        span.style.fg.is_some()
            && span.style.fg != Some(Color::default())
            && span.style.fg != Some(Color::Reset)
    });
    has_syntax_style.then(|| lines.into_iter().map(|line| line.spans).collect())
}

#[cfg(test)]
mod tests {
    use super::style::DiffColorLevel;
    use super::style::DiffTheme;
    use super::*;
    use insta::assert_snapshot;
    use pretty_assertions::assert_eq;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::text::Text;
    use ratatui::widgets::Wrap;
    use unicode_width::UnicodeWidthStr;

    /// Snapshots are drawn for a dark truecolor terminal, whatever the test runs in.
    fn truecolor() -> DiffRenderStyleContext {
        DiffRenderStyleContext::new(DiffTheme::Dark, DiffColorLevel::TrueColor)
    }

    fn update(old_text: &str, new_text: &str) -> FileChange {
        FileChange::Update {
            old_text: old_text.to_string(),
            new_text: new_text.to_string(),
            first_line: 1,
            move_path: None,
        }
    }

    fn summary_lines(
        changes: &HashMap<PathBuf, FileChange>,
        cwd: &Path,
        wrap_cols: usize,
    ) -> Vec<RtLine<'static>> {
        render_changes_block(collect_rows(changes), wrap_cols, cwd, &truecolor())
    }

    fn diff_summary_for_tests(changes: &HashMap<PathBuf, FileChange>) -> Vec<RtLine<'static>> {
        summary_lines(changes, &PathBuf::from("/"), 80)
    }

    /// How emphasis is marked in snapshots, as git's word diff marks it: `[-removed-]`,
    /// `{+added+}`.
    fn emphasis_marks(
        bg: Option<Color>,
        ctx: &DiffRenderStyleContext,
    ) -> Option<(&'static str, &'static str)> {
        if bg.is_some() && bg == ctx.add_emphasis.bg {
            Some(("{+", "+}"))
        } else if bg.is_some() && bg == ctx.del_emphasis.bg {
            Some(("[-", "-]"))
        } else {
            None
        }
    }

    /// The terminal rows of `lines`, quoted as `TestBackend` prints them, with emphasised cells
    /// marked.
    fn snapshot_lines(name: &str, lines: Vec<RtLine<'static>>, width: u16, height: u16) {
        let ctx = truecolor();
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal
            .draw(|f| {
                Paragraph::new(Text::from(lines))
                    .wrap(Wrap { trim: false })
                    .render(f.area(), f.buffer_mut())
            })
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let mut rows = Vec::new();
        for y in 0..height {
            let (mut row, mut open, mut skip) = (String::new(), None, 0);
            for x in 0..width {
                let cell = &buffer[(x, y)];
                let marks = emphasis_marks(Some(cell.bg), &ctx);
                if marks != open {
                    row.extend(open.map(|(_, close)| close));
                    row.extend(marks.map(|(start, _)| start));
                    open = marks;
                }
                if skip == 0 {
                    row.push_str(cell.symbol());
                }
                skip = skip.max(cell.symbol().width()).saturating_sub(1);
            }
            row.extend(open.map(|(_, close)| close));
            rows.push(format!("\"{row}\""));
        }
        assert_snapshot!(name, rows.join("\n"));
    }

    /// `lines` as plain text with emphasis marked, trailing spaces trimmed so that indentation
    /// is easy to check.
    fn snapshot_lines_text(name: &str, lines: &[RtLine<'static>]) {
        let ctx = truecolor();
        let text = lines
            .iter()
            .map(|line| {
                let (mut row, mut open) = (String::new(), None);
                for span in &line.spans {
                    let marks = emphasis_marks(span.style.bg, &ctx);
                    if marks != open {
                        row.extend(open.map(|(_, close)| close));
                        row.extend(marks.map(|(start, _)| start));
                        open = marks;
                    }
                    row.push_str(&span.content);
                }
                row.extend(open.map(|(_, close)| close));
                row.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert_snapshot!(name, text);
    }

    #[test]
    fn ui_snapshot_apply_update_block() {
        let mut changes: HashMap<PathBuf, FileChange> = HashMap::new();
        let original = "line one\nline two\nline three\n";
        let modified = "line one\nline two changed\nline three\n";

        changes.insert(PathBuf::from("example.txt"), update(original, modified));

        let lines = diff_summary_for_tests(&changes);

        snapshot_lines("apply_update_block", lines, 80, 12);
    }

    #[test]
    fn ui_snapshot_apply_update_with_rename_block() {
        let mut changes: HashMap<PathBuf, FileChange> = HashMap::new();
        let original = "A\nB\nC\n";
        let modified = "A\nB changed\nC\n";

        changes.insert(
            PathBuf::from("old_name.rs"),
            FileChange::Update {
                old_text: original.to_string(),
                new_text: modified.to_string(),
                first_line: 1,
                move_path: Some(PathBuf::from("new_name.rs")),
            },
        );

        let lines = diff_summary_for_tests(&changes);

        snapshot_lines("apply_update_with_rename_block", lines, 80, 12);
    }

    #[test]
    fn ui_snapshot_apply_multiple_files_block() {
        // Two files: one update and one add, to exercise combined header and per-file rows
        let mut changes: HashMap<PathBuf, FileChange> = HashMap::new();

        // File a.txt: single-line replacement (one delete, one insert)
        changes.insert(PathBuf::from("a.txt"), update("one\n", "one changed\n"));

        // File b.txt: newly added with one line
        changes.insert(
            PathBuf::from("b.txt"),
            FileChange::Add {
                content: "new\n".to_string(),
            },
        );

        let lines = diff_summary_for_tests(&changes);

        snapshot_lines("apply_multiple_files_block", lines, 80, 14);
    }

    #[test]
    fn ui_snapshot_apply_add_block() {
        let mut changes: HashMap<PathBuf, FileChange> = HashMap::new();
        changes.insert(
            PathBuf::from("new_file.txt"),
            FileChange::Add {
                content: "alpha\nbeta\n".to_string(),
            },
        );

        let lines = diff_summary_for_tests(&changes);

        snapshot_lines("apply_add_block", lines, 80, 10);
    }

    #[test]
    fn ui_snapshot_apply_delete_block() {
        let mut changes: HashMap<PathBuf, FileChange> = HashMap::new();
        changes.insert(
            PathBuf::from("tmp_delete_example.txt"),
            FileChange::Delete {
                content: "first\nsecond\nthird\n".to_string(),
            },
        );

        let lines = diff_summary_for_tests(&changes);

        snapshot_lines("apply_delete_block", lines, 80, 12);
    }

    #[test]
    fn ui_snapshot_apply_update_block_wraps_long_lines() {
        // Create a patch with a long modified line to force wrapping
        let original = "line 1\nshort\nline 3\n";
        let modified = "line 1\nshort this_is_a_very_long_modified_line_that_should_wrap_across_multiple_terminal_columns_and_continue_even_further_beyond_eighty_columns_to_force_multiple_wraps\nline 3\n";

        let mut changes: HashMap<PathBuf, FileChange> = HashMap::new();
        changes.insert(
            PathBuf::from("long_example.txt"),
            update(original, modified),
        );

        let lines = summary_lines(&changes, &PathBuf::from("/"), 72);

        // Render with backend width wider than wrap width to avoid Paragraph auto-wrap.
        snapshot_lines("apply_update_block_wraps_long_lines", lines, 80, 12);
    }

    #[test]
    fn ui_snapshot_apply_update_block_wraps_long_lines_text() {
        // This mirrors the desired layout example: sign only on first inserted line,
        // subsequent wrapped pieces start aligned under the line number gutter.
        let original = "1\n2\n3\n4\n";
        let modified = "1\nadded long line which wraps and_if_there_is_a_long_token_it_will_be_broken\n3\n4 context line which also wraps across\n";

        let mut changes: HashMap<PathBuf, FileChange> = HashMap::new();
        changes.insert(PathBuf::from("wrap_demo.txt"), update(original, modified));

        let lines = summary_lines(&changes, &PathBuf::from("/"), 28);
        snapshot_lines_text("apply_update_block_wraps_long_lines_text", &lines);
    }

    #[test]
    fn ui_snapshot_apply_update_block_line_numbers_three_digits_text() {
        let original = (1..=110).map(|i| format!("line {i}\n")).collect::<String>();
        let modified = (1..=110)
            .map(|i| {
                if i == 100 {
                    format!("line {i} changed\n")
                } else {
                    format!("line {i}\n")
                }
            })
            .collect::<String>();

        let mut changes: HashMap<PathBuf, FileChange> = HashMap::new();
        changes.insert(PathBuf::from("hundreds.txt"), update(&original, &modified));

        let lines = summary_lines(&changes, &PathBuf::from("/"), 80);
        snapshot_lines_text("apply_update_block_line_numbers_three_digits_text", &lines);
    }

    #[test]
    fn ui_snapshot_apply_update_block_relativizes_path() {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let abs_old = cwd.join("abs_old.rs");
        let abs_new = cwd.join("abs_new.rs");

        let original = "X\nY\n";
        let modified = "X changed\nY\n";

        let mut changes: HashMap<PathBuf, FileChange> = HashMap::new();
        changes.insert(
            abs_old,
            FileChange::Update {
                old_text: original.to_string(),
                new_text: modified.to_string(),
                first_line: 1,
                move_path: Some(abs_new),
            },
        );

        let lines = summary_lines(&changes, &cwd, 80);

        snapshot_lines("apply_update_block_relativizes_path", lines, 80, 10);
    }

    /// Hunks further apart than twice the context are shown apart, `⋮` between them, numbered
    /// from the line of the file the texts begin on.
    #[test]
    fn ui_snapshot_hunks_fold_unchanged_lines_between_them() {
        let original = (1..=30).map(|i| format!("line {i}\n")).collect::<String>();
        let modified = original
            .replace("line 4\n", "line four\n")
            .replace("line 26\n", "line 26\nline 26 and a half\n");

        let mut changes: HashMap<PathBuf, FileChange> = HashMap::new();
        changes.insert(
            PathBuf::from("story.txt"),
            FileChange::Update {
                old_text: original,
                new_text: modified,
                first_line: 91,
                move_path: None,
            },
        );

        let lines = diff_summary_for_tests(&changes);
        snapshot_lines_text("hunks_fold_unchanged_lines_between_them", &lines);
    }

    const BEFORE_RS: &str = "fn area(width: u32, height: u32) -> u32 {
    width * height
}

fn perimeter(width: u32, height: u32) -> u32 {
  2 * (width + height)
}

fn main() {
    // Print the area and the perimeter.
    let total = area(3, 4) + perimeter(3, 4);
    println!(\"{total}\");
}
";

    /// `BEFORE_RS` with the body of `perimeter` re-indented, an addition rewrapped, a variable
    /// renamed and a comment edited.
    const AFTER_RS: &str = "fn area(width: u32, height: u32) -> u32 {
    width * height
}

fn perimeter(width: u32, height: u32) -> u32 {
    2 * (width + height)
}

fn main() {
    // Print the sum of the area and the perimeter.
    let sum = area(3, 4)
        + perimeter(3, 4);
    println!(\"{sum}\");
}
";

    /// Rust is diffed by its syntax: the re-indented line is context, the rewrapped addition is
    /// changed but has nothing new in it, and only the renamed variable and the comment's new
    /// words are emphasised.
    #[test]
    fn ui_snapshot_structural_edit() {
        let mut changes: HashMap<PathBuf, FileChange> = HashMap::new();
        changes.insert(
            PathBuf::from("src/geometry.rs"),
            update(BEFORE_RS, AFTER_RS),
        );
        snapshot_lines_text("structural_edit", &diff_summary_for_tests(&changes));
    }

    /// The same edit to a file with no grammar is diffed by lines: the re-indentation is a
    /// change, and the words that differ between paired lines are emphasised.
    #[test]
    fn ui_snapshot_line_edit() {
        let mut changes: HashMap<PathBuf, FileChange> = HashMap::new();
        changes.insert(PathBuf::from("geometry.txt"), update(BEFORE_RS, AFTER_RS));
        snapshot_lines_text("line_edit", &diff_summary_for_tests(&changes));
    }

    /// Two functions swapped: the moved code is removed and added, but none of it is new, so
    /// nothing in it is emphasised.
    #[test]
    fn ui_snapshot_structural_move_is_not_emphasised() {
        let alpha = "fn alpha(values: &[u32]) -> u32 {\n    values.iter().sum()\n}\n";
        let beta = "fn beta(values: &[u32]) -> usize {\n    values.len()\n}\n";
        let mut changes: HashMap<PathBuf, FileChange> = HashMap::new();
        changes.insert(
            PathBuf::from("src/stats.rs"),
            update(&format!("{alpha}\n{beta}"), &format!("{beta}\n{alpha}")),
        );
        snapshot_lines_text("structural_move", &diff_summary_for_tests(&changes));
    }

    #[test]
    fn moved_update_uses_destination_path_for_syntax_highlighting() {
        let mut changes: HashMap<PathBuf, FileChange> = HashMap::new();
        changes.insert(
            PathBuf::from("notes/plain.txt"),
            FileChange::Update {
                old_text: "plain text\n".to_string(),
                new_text: "fn renamed() {\n    println!(\"hello\");\n}\n".to_string(),
                first_line: 1,
                move_path: Some(PathBuf::from("src/renamed.rs")),
            },
        );

        let lines = create_diff_summary(&changes, &PathBuf::from("/"), 80);
        let inserted_fn = lines
            .iter()
            .find(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
                    .contains("+fn renamed")
            })
            .expect("inserted Rust function diff line");

        assert!(
            inserted_fn.spans.iter().any(|span| {
                span.style.fg.is_some()
                    && span.style.fg != Some(Color::Green)
                    && span.content.as_ref().contains("fn")
            }),
            "expected moved update to highlight inserted Rust using destination path, got {inserted_fn:?}"
        );
    }

    /// A 5,000-line file with a few scattered edits renders in time proportional to the edits:
    /// well inside a frame in an optimised build. The bound asserted is for unoptimised test
    /// builds; the times measured are printed.
    #[test]
    #[allow(clippy::print_stderr)] // The times measured are printed for `--nocapture`.
    fn large_file_with_scattered_edits_renders_within_a_frame() {
        let original: String = (0..1000)
            .map(|i| {
                format!(
                    "fn item_{i}(input: u32) -> u32 {{\n    let doubled = input * 2;\n    doubled + {i}\n}}\n\n"
                )
            })
            .collect();
        assert_eq!(original.lines().count(), 5000);
        let modified = original
            .replace("doubled + 120\n", "doubled + 121\n")
            .replace("fn item_480(input: u32)", "fn item_480(value: u32)")
            .replace(
                "    let doubled = input * 2;\n    doubled + 777\n",
                "    let doubled = input * 2;\n    let tripled = input * 3;\n    doubled + tripled\n",
            )
            .replace("doubled + 990\n", "doubled - 990\n");
        for path in ["src/items.rs", "items.txt"] {
            let mut changes: HashMap<PathBuf, FileChange> = HashMap::new();
            changes.insert(PathBuf::from(path), update(&original, &modified));
            // The first render loads the highlighter's syntaxes and the grammar.
            let lines = summary_lines(&changes, Path::new("/"), 100);
            assert!(
                lines.len() < 60,
                "only the hunks are shown, got {} lines",
                lines.len()
            );
            let times: Vec<std::time::Duration> = (0..5)
                .map(|_| {
                    let start = std::time::Instant::now();
                    let lines = summary_lines(&changes, Path::new("/"), 100);
                    let elapsed = start.elapsed();
                    assert!(!lines.is_empty());
                    elapsed
                })
                .collect();
            let fastest = times.iter().min().copied().unwrap_or_default();
            eprintln!("{path}: 5,000 lines, 4 edits rendered in {times:?} (fastest {fastest:?})");
            assert!(
                fastest < std::time::Duration::from_millis(150),
                "{path} took {fastest:?}"
            );
        }
    }

    #[test]
    fn counts_follow_the_diff() {
        let mut changes: HashMap<PathBuf, FileChange> = HashMap::new();
        changes.insert(
            PathBuf::from("src/geometry.rs"),
            update(BEFORE_RS, AFTER_RS),
        );
        changes.insert(PathBuf::from("geometry.txt"), update(BEFORE_RS, AFTER_RS));
        let rows = collect_rows(&changes);
        let counts: Vec<(&Path, usize, usize)> = rows
            .iter()
            .map(|row| (row.path, row.added, row.removed))
            .collect();
        // The line diff counts the re-indented line; the structural diff does not.
        assert_eq!(
            counts,
            [
                (Path::new("geometry.txt"), 5, 4),
                (Path::new("src/geometry.rs"), 4, 3),
            ]
        );
    }

    /// A tinted line is padded to the right edge of the block, indent included, and no
    /// further: a row wider than the cell would wrap into a blank one.
    #[test]
    fn tinted_rows_fill_the_width_exactly() {
        let mut changes: HashMap<PathBuf, FileChange> = HashMap::new();
        changes.insert(PathBuf::from("a.txt"), update("one\ntwo\n", "one\n2\n"));
        let widths: Vec<usize> = diff_summary_for_tests(&changes)
            .iter()
            .filter(|line| line.style.bg.is_some())
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                    .sum()
            })
            .collect();
        assert_eq!(widths, [80, 80]);
    }
}
