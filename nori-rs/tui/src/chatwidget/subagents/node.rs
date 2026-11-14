//! One running subagent: what the live block shows and the detail its summary will carry.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use nori_protocol::acp::v1 as acp;
use nori_protocol::acp::v1::SessionId;

use super::block::RowCache;
use crate::client_tool_cell::ClientToolCell;
use crate::history_cell::AgentMarkdownCell;
use crate::history_cell::HistoryCell;
use crate::presentation::ClientEvent;
use crate::presentation::ClientEventNormalizer;

/// How much of a child's final message is kept for its summary.
const MAX_FINAL_MESSAGE_BYTES: usize = 8 * 1024;
/// The activity row shows at most this many characters of the latest line.
const MAX_ACTIVITY_CHARS: usize = 240;
/// The latest line is looked for in at most this many trailing bytes of streamed text, so a
/// delta costs the same however long the line it continues is.
const ACTIVITY_SCAN_BYTES: usize = MAX_ACTIVITY_CHARS * 4;

/// Identity of a running subagent within its tree. Ids grow and are never reused, so ordering by
/// id is the order in which subagents appeared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct NodeId(pub(super) i64);

/// A part of a child's detail, in the order it happened.
#[derive(Debug)]
enum Detail {
    Tool(Box<ClientToolCell>),
    Text(String),
}

/// What a child did, kept for the transcript overlay once it finishes.
#[derive(Debug, Default)]
struct ChildTranscript {
    parts: Vec<Detail>,
    /// The last part is text that more deltas continue; a tool call ends it.
    text_open: bool,
    /// Position in `parts` of each tool call.
    tools: HashMap<String, usize>,
    normalizer: ClientEventNormalizer,
}

/// A subagent that is still running. A finished one is turned into a
/// [`super::FinishedSubagent`] and leaves the tree, so nothing here describes a finished child.
#[derive(Debug)]
pub(super) struct Node {
    /// Child session id, once known. A child known only through its launcher call has none.
    pub(super) session_id: Option<SessionId>,
    /// The root tool call that launched this child, once known.
    pub(super) launcher_call_id: Option<String>,
    /// The running subagent that announced this one.
    pub(super) parent: Option<NodeId>,
    /// Running subagents this one announced.
    pub(super) children: BTreeSet<NodeId>,
    pub(super) title: Option<String>,
    pub(super) description: Option<String>,
    /// Known only through a launcher that reported completion at once while its work continues.
    pub(super) async_launch: bool,
    pub(super) started_at: Instant,
    pub(super) tool_count: usize,
    /// The latest tool title or last line of streamed text.
    pub(super) activity: String,
    /// Bumped on every change that affects what the live block shows.
    pub(super) revision: i64,
    /// Rows built for the live block, reused until the node changes or the width does.
    pub(super) row_cache: RefCell<Option<RowCache>>,
    /// Summaries of this child's own finished children, for the detail view.
    pub(super) nested: Vec<Arc<dyn HistoryCell>>,
    transcript: ChildTranscript,
}

impl Node {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            session_id: None,
            launcher_call_id: None,
            parent: None,
            children: BTreeSet::new(),
            title: None,
            description: None,
            async_launch: false,
            started_at: now,
            tool_count: 0,
            activity: String::new(),
            revision: 0,
            row_cache: RefCell::new(None),
            nested: Vec::new(),
            transcript: ChildTranscript::default(),
        }
    }

    /// The main label: what the child was asked to do, else its type.
    pub(super) fn label(&self) -> &str {
        self.description
            .as_deref()
            .or(self.title.as_deref())
            .unwrap_or("subagent")
    }

    /// The type of the child, shown dim next to the task when both exist.
    pub(super) fn kind(&self) -> Option<&str> {
        match (&self.description, &self.title) {
            (Some(_), Some(title)) => Some(title),
            _ => None,
        }
    }

    pub(super) fn touch(&mut self) {
        self.revision += 1;
    }

    /// Absorb an update delivered on the child's session or tagged with its launcher.
    ///
    /// Costs the size of the update: text is appended, a tool call updates its own cell.
    pub(super) fn apply(&mut self, update: &acp::SessionUpdate, cwd: &Path, animations: bool) {
        match update {
            acp::SessionUpdate::AgentMessageChunk(chunk) => {
                if let acp::ContentBlock::Text(text) = &chunk.content {
                    self.push_text(&text.text);
                }
            }
            acp::SessionUpdate::ToolCall(_) | acp::SessionUpdate::ToolCallUpdate(_) => {
                self.transcript.text_open = false;
                for event in self.transcript.normalizer.push_session_update(update) {
                    let ClientEvent::ToolSnapshot(snapshot) = event else {
                        continue;
                    };
                    if !snapshot.title.is_empty() {
                        self.activity.clear();
                        self.activity
                            .extend(snapshot.title.chars().take(MAX_ACTIVITY_CHARS));
                    }
                    match self.transcript.tools.get(&snapshot.call_id) {
                        Some(position) => {
                            if let Some(Detail::Tool(cell)) =
                                self.transcript.parts.get_mut(*position)
                            {
                                cell.apply_snapshot(snapshot);
                            }
                        }
                        None => {
                            self.tool_count += 1;
                            self.transcript
                                .tools
                                .insert(snapshot.call_id.clone(), self.transcript.parts.len());
                            self.transcript.parts.push(Detail::Tool(Box::new(
                                ClientToolCell::new(snapshot, cwd.to_path_buf(), animations),
                            )));
                        }
                    }
                    self.touch();
                }
            }
            // A child's thoughts, plan, usage, mode and metadata updates are never the root's
            // and are not shown live. (The schema's variant set depends on its features.)
            _ => {}
        }
    }

    fn push_text(&mut self, delta: &str) {
        match self.transcript.parts.last_mut() {
            Some(Detail::Text(text)) if self.transcript.text_open => text.push_str(delta),
            _ => self.transcript.parts.push(Detail::Text(delta.to_owned())),
        }
        self.transcript.text_open = true;
        let Some(Detail::Text(text)) = self.transcript.parts.last() else {
            return;
        };
        // The last line may be split across deltas; the text so far has it whole.
        let mut tail_start = text.len().saturating_sub(ACTIVITY_SCAN_BYTES);
        while !text.is_char_boundary(tail_start) {
            tail_start += 1;
        }
        let Some(line) = text[tail_start..]
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
        else {
            return;
        };
        self.activity.clear();
        self.activity
            .extend(line.trim().chars().take(MAX_ACTIVITY_CHARS));
        self.touch();
    }

    /// Take the child's detail as cells for the transcript overlay, and the final message: the
    /// text it streamed after its last tool call.
    pub(super) fn take_detail(&mut self, cwd: &Path) -> (Vec<Arc<dyn HistoryCell>>, String) {
        let parts = std::mem::take(&mut self.transcript.parts);
        let final_message = match parts.last() {
            Some(Detail::Text(text)) if self.transcript.text_open => {
                let mut end = text.len().min(MAX_FINAL_MESSAGE_BYTES);
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                text[..end].to_owned()
            }
            Some(Detail::Text(_) | Detail::Tool(_)) | None => String::new(),
        };
        let mut detail: Vec<Arc<dyn HistoryCell>> = parts
            .into_iter()
            .map(|part| -> Arc<dyn HistoryCell> {
                match part {
                    Detail::Tool(cell) => Arc::new(*cell),
                    Detail::Text(text) => Arc::new(AgentMarkdownCell::new(text, cwd)),
                }
            })
            .collect();
        detail.append(&mut self.nested);
        (detail, final_message)
    }
}

/// The working directory and animation setting every child's cells are built with.
#[derive(Debug, Clone)]
pub(super) struct CellContext {
    pub(super) cwd: PathBuf,
    pub(super) animations: bool,
}
