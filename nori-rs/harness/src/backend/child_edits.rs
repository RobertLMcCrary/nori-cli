//! File edits made by subagents, for `/undo`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use nori_protocol::acp::v1 as acp;

use crate::normalized::ClientEvent;
use crate::normalized::ClientEventNormalizer;
use crate::undo::EditJournal;

/// Turns the tool calls of subagent sessions into journal entries.
///
/// A subagent's file diffs belong to the root turn that is running, so they are recorded in the
/// same journal as the root's, scoped by the child's session id so call ids cannot collide.
#[derive(Default)]
pub(super) struct ChildEdits {
    normalizers: HashMap<String, ClientEventNormalizer>,
}

impl ChildEdits {
    pub(super) fn observe(
        &mut self,
        notification: &acp::SessionNotification,
        journal: &Arc<EditJournal>,
        cwd: &Path,
    ) {
        if !matches!(
            notification.update,
            acp::SessionUpdate::ToolCall(_) | acp::SessionUpdate::ToolCallUpdate(_)
        ) {
            return;
        }
        let session_id = notification.session_id.to_string();
        let normalizer = self.normalizers.entry(session_id.clone()).or_default();
        for event in normalizer.push_session_update(&notification.update) {
            if let ClientEvent::ToolSnapshot(snapshot) = event {
                journal.record(&snapshot, cwd, Some(&session_id));
            }
        }
    }
}
