use std::path::Path;

/// Replace occurrences of the given `cwd` prefix (with trailing `/`) in `text`
/// with an empty string, effectively turning absolute paths under `cwd` into
/// relative ones. Only replaces when the prefix is followed by a non-`/`
/// character (to avoid stripping a sibling directory that shares a prefix).
pub(crate) fn relativize_paths_in_text(text: &str, cwd: &Path) -> String {
    let cwd_str = format!("{}/", cwd.display());
    text.replace(&cwd_str, "")
}

pub(crate) fn format_tool_kind(kind: &crate::presentation::ToolKind) -> &str {
    match kind {
        crate::presentation::ToolKind::Read => "read",
        crate::presentation::ToolKind::Search => "search",
        crate::presentation::ToolKind::Execute => "execute",
        crate::presentation::ToolKind::Create => "create",
        crate::presentation::ToolKind::Edit => "edit",
        crate::presentation::ToolKind::Delete => "delete",
        crate::presentation::ToolKind::Move => "move",
        crate::presentation::ToolKind::Fetch => "fetch",
        crate::presentation::ToolKind::Think => "think",
        crate::presentation::ToolKind::Other(other) => other,
    }
}

pub(crate) fn format_tool_header(snapshot: &crate::presentation::ToolSnapshot) -> String {
    snapshot.title.clone()
}

/// Semantic header for Create/Edit/Delete/Move tool snapshots.
/// Returns verb-based header like "Adding path", "Edited path", "Deleted path".
pub(crate) fn format_edit_tool_header(snapshot: &crate::presentation::ToolSnapshot) -> String {
    let (verb_active, verb_past, verb_failed, prefix) = match &snapshot.kind {
        crate::presentation::ToolKind::Create => ("Adding", "Added", "Add failed:", "Add "),
        crate::presentation::ToolKind::Edit => ("Editing", "Edited", "Edit failed:", "Edit "),
        crate::presentation::ToolKind::Delete => {
            ("Deleting", "Deleted", "Delete failed:", "Delete ")
        }
        crate::presentation::ToolKind::Move => ("Moving", "Moved", "Move failed:", "Move "),
        _ => return format_tool_header(snapshot),
    };

    let path = snapshot
        .locations
        .first()
        .map(|loc| loc.path.display().to_string())
        .unwrap_or_else(|| {
            // Try the kind-specific prefix first, then fall back to "Edit " since
            // some agents (Codex) always send title "Edit /path" regardless of the
            // actual operation type (which we refine via rawInput.changes).
            snapshot
                .title
                .strip_prefix(prefix)
                .or_else(|| snapshot.title.strip_prefix("Edit "))
                .unwrap_or(&snapshot.title)
                .to_string()
        });

    let verb = match &snapshot.phase {
        crate::presentation::ToolPhase::Failed => verb_failed,
        crate::presentation::ToolPhase::Completed => verb_past,
        _ => verb_active,
    };
    format!("{verb} {path}")
}

pub(crate) fn is_exploring_snapshot(snapshot: &crate::presentation::ToolSnapshot) -> bool {
    matches!(
        snapshot.kind,
        crate::presentation::ToolKind::Read | crate::presentation::ToolKind::Search
    ) || matches!(
        snapshot.invocation,
        Some(crate::presentation::Invocation::ListFiles { .. })
    )
}
