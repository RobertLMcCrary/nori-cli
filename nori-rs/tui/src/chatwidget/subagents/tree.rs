//! The subagents of one conversation: who is running, under whom, and who finished.
//!
//! Every operation costs O(1) map work plus the size of the update it applies, except
//! [`SubagentTree::on_root_turn_end`], which visits each running child once. Finished children
//! leave the tree as [`FinishedSubagent`]s; their ids stay mapped to [`Link::Ended`] so late
//! updates for them are absorbed instead of resurrecting them.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use nori_protocol::SubagentMessage;
use nori_protocol::SubagentOutcome;
use nori_protocol::SubagentState;
use nori_protocol::SubagentUpdate;
use nori_protocol::acp::v1 as acp;
use nori_protocol::acp::v1::SessionId;

use super::node::CellContext;
use super::node::Node;
use super::node::NodeId;
use crate::history_cell::HistoryCell;
use crate::presentation::Artifact;
use crate::presentation::ClientEvent;
use crate::presentation::ClientEventNormalizer;
use crate::presentation::ToolPhase;
use crate::presentation::ToolSnapshot;

/// A child that finished, ready to become a scrollback cell.
#[derive(Debug, Clone)]
pub(crate) struct FinishedSubagent {
    pub(crate) label: String,
    pub(crate) kind: Option<String>,
    pub(crate) outcome: SubagentOutcome,
    pub(crate) tool_count: usize,
    pub(crate) elapsed: Duration,
    /// What the child said after its last tool call.
    pub(crate) message: String,
    pub(crate) detail: Vec<Arc<dyn HistoryCell>>,
    /// How deep in the tree the child was, for indenting its summary.
    pub(crate) depth: usize,
}

/// What a session id or launcher call id refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Link {
    Live(NodeId),
    /// The child finished; anything more for it is dropped.
    Ended,
}

/// Which node an update for a child belongs to.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ChildTarget<'a> {
    /// Delivered on the child's own session.
    Session(&'a SessionId),
    /// A root-session update tagged with the id of the tool call that launched the child.
    Launcher(&'a str),
}

/// All subagents of the session.
#[derive(Debug)]
pub(crate) struct SubagentTree {
    /// Running subagents by id, so iteration is in the order they appeared.
    pub(super) live: BTreeMap<NodeId, Node>,
    by_session: HashMap<SessionId, Link>,
    by_launcher: HashMap<String, Link>,
    root_session_id: Option<SessionId>,
    context: CellContext,
    /// Merges launcher tool-call updates, which carry only the changed fields.
    launcher_normalizer: ClientEventNormalizer,
    finished: Vec<FinishedSubagent>,
    next_id: i64,
    /// Live-block rows built so far, for tests that count work.
    pub(super) rows_built: std::cell::Cell<usize>,
}

impl SubagentTree {
    pub(crate) fn new(cwd: std::path::PathBuf, animations: bool) -> Self {
        Self {
            live: BTreeMap::new(),
            by_session: HashMap::new(),
            by_launcher: HashMap::new(),
            root_session_id: None,
            context: CellContext { cwd, animations },
            launcher_normalizer: ClientEventNormalizer::default(),
            finished: Vec::new(),
            next_id: 0,
            rows_built: std::cell::Cell::new(0),
        }
    }

    pub(crate) fn set_root(&mut self, session_id: SessionId) {
        self.root_session_id = Some(session_id);
    }

    pub(crate) fn is_root(&self, session_id: &SessionId) -> bool {
        self.root_session_id.as_ref() == Some(session_id)
    }

    pub(crate) fn animations(&self) -> bool {
        self.context.animations
    }

    #[cfg(test)]
    pub(crate) fn rows_built(&self) -> usize {
        self.rows_built.get()
    }

    pub(crate) fn running_count(&self) -> usize {
        self.live.len()
    }

    pub(crate) fn has_running(&self) -> bool {
        !self.live.is_empty()
    }

    /// Children that finished since the last call, in completion order.
    pub(crate) fn take_finished(&mut self) -> Vec<FinishedSubagent> {
        std::mem::take(&mut self.finished)
    }

    /// The label to show for a request that came from `session_id`, if it is a running child.
    pub(crate) fn label_for_session(&self, session_id: &SessionId) -> Option<String> {
        let Some(Link::Live(id)) = self.by_session.get(session_id) else {
            return None;
        };
        Some(self.live.get(id)?.label().to_owned())
    }

    /// Whether `call_id` is a root tool call that stands for a subagent.
    pub(crate) fn is_launcher_call(&self, call_id: &str) -> bool {
        self.by_launcher.contains_key(call_id)
    }

    fn create(&mut self, now: Instant) -> NodeId {
        self.next_id += 1;
        let id = NodeId(self.next_id);
        self.live.insert(id, Node::new(now));
        id
    }

    /// The running node for a child session, created on first sight; `None` once it finished.
    fn node_for_session(&mut self, session_id: &SessionId, now: Instant) -> Option<NodeId> {
        match self.by_session.get(session_id) {
            Some(Link::Live(id)) => return Some(*id),
            Some(Link::Ended) => return None,
            None => {}
        }
        let id = self.create(now);
        if let Some(node) = self.live.get_mut(&id) {
            node.session_id = Some(session_id.clone());
        }
        self.by_session.insert(session_id.clone(), Link::Live(id));
        Some(id)
    }

    /// The running node for a launcher tool call, created on first sight; `None` once it finished.
    fn node_for_launcher(&mut self, call_id: &str, now: Instant) -> Option<NodeId> {
        match self.by_launcher.get(call_id) {
            Some(Link::Live(id)) => return Some(*id),
            Some(Link::Ended) => return None,
            None => {}
        }
        let id = self.create(now);
        if let Some(node) = self.live.get_mut(&id) {
            node.launcher_call_id = Some(call_id.to_owned());
        }
        self.by_launcher.insert(call_id.to_owned(), Link::Live(id));
        Some(id)
    }

    /// Make `child` a child of `parent` unless it already has a parent or `parent` descends from
    /// it. An agent that contradicts itself cannot make a cycle, which would hide both nodes.
    fn adopt(&mut self, parent: NodeId, child: NodeId) {
        let mut ancestor = Some(parent);
        while let Some(id) = ancestor {
            if id == child {
                return;
            }
            ancestor = self.live.get(&id).and_then(|node| node.parent);
        }
        let Some(node) = self.live.get_mut(&child) else {
            return;
        };
        if node.parent.is_some() {
            return;
        }
        node.parent = Some(parent);
        if let Some(parent) = self.live.get_mut(&parent) {
            parent.children.insert(child);
        }
    }

    /// An announcement, or a change to a child already announced.
    pub(crate) fn on_update(&mut self, update: SubagentUpdate, now: Instant) {
        let Some(id) = self.node_for_session(&update.session_id, now) else {
            return;
        };
        if let Some(Link::Live(parent)) = self.by_session.get(&update.parent_session_id).copied() {
            self.adopt(parent, id);
        }
        if let Some(node) = self.live.get_mut(&id) {
            update.title.apply_to(&mut node.title);
            update.description.apply_to(&mut node.description);
            node.touch();
        }
        match update.state {
            nori_protocol::Patch::Set(SubagentState::Ended(outcome)) => {
                self.finish(id, outcome, None, now);
            }
            nori_protocol::Patch::Set(SubagentState::Running)
            | nori_protocol::Patch::Keep
            | nori_protocol::Patch::Clear => {}
        }
    }

    /// A message between sessions. The prompt a parent sent to a child names the parent's launcher
    /// tool call, which links the call to the child: the child's node takes over the launcher's.
    pub(crate) fn on_message(&mut self, message: SubagentMessage, now: Instant) {
        let (Some(call_id), Some(recipient)) = (
            message.message_id.as_deref(),
            message.recipient_session_id.as_ref(),
        ) else {
            return;
        };
        let Some(Link::Live(launcher)) = self.by_launcher.get(call_id).copied() else {
            return;
        };
        let Some(child) = self.node_for_session(recipient, now) else {
            return;
        };
        if launcher == child {
            return;
        }
        let Some(stand_in) = self.live.remove(&launcher) else {
            return;
        };
        self.by_launcher
            .insert(call_id.to_owned(), Link::Live(child));
        if let Some(parent) = stand_in
            .parent
            .and_then(|parent| self.live.get_mut(&parent))
        {
            parent.children.remove(&launcher);
        }
        for grandchild in &stand_in.children {
            if let Some(node) = self.live.get_mut(grandchild) {
                node.parent = None;
            }
        }
        for grandchild in stand_in.children.iter().copied() {
            self.adopt(child, grandchild);
        }
        if let Some(parent) = stand_in.parent {
            self.adopt(parent, child);
        }
        if let Some(node) = self.live.get_mut(&child) {
            node.launcher_call_id = Some(call_id.to_owned());
            node.started_at = stand_in.started_at.min(node.started_at);
            node.tool_count += stand_in.tool_count;
            if node.description.is_none() {
                node.description = stand_in.description;
            }
            if node.title.is_none() {
                node.title = stand_in.title;
            }
            node.touch();
        }
    }

    /// A root-session tool call that launches a subagent, or an update to one.
    pub(crate) fn on_launcher_update(
        &mut self,
        update: &acp::SessionUpdate,
        async_launch: bool,
        now: Instant,
    ) {
        let Some(snapshot) = self
            .launcher_normalizer
            .push_session_update(update)
            .into_iter()
            .find_map(|event| match event {
                ClientEvent::ToolSnapshot(snapshot) => Some(snapshot),
                _ => None,
            })
        else {
            return;
        };
        let Some(id) = self.node_for_launcher(&snapshot.call_id, now) else {
            return;
        };
        let Some(node) = self.live.get_mut(&id) else {
            return;
        };
        let announced = node.session_id.is_some();
        // The launcher's title starts as the tool's name and becomes the task once the adapter
        // fills it in; an announced child carries its own description.
        if !announced
            && !snapshot.title.is_empty()
            && !matches!(snapshot.title.as_str(), "Task" | "Agent")
        {
            node.description = Some(snapshot.title.clone());
        }
        node.async_launch |= async_launch;
        node.touch();
        // An announced child reports its own state, and an asynchronous launch finishes within
        // milliseconds whatever the child is doing.
        if announced || node.async_launch {
            return;
        }
        match snapshot.phase {
            ToolPhase::Completed => {
                let message = snapshot_text(&snapshot);
                self.finish(
                    id,
                    SubagentOutcome::Completed { stop_reason: None },
                    Some(message),
                    now,
                );
            }
            ToolPhase::Failed => {
                self.finish(id, SubagentOutcome::Failed { reason: None }, None, now);
            }
            ToolPhase::Pending | ToolPhase::PendingApproval | ToolPhase::InProgress => {}
        }
    }

    /// An update delivered on a child's own session, or tagged with its launcher's id.
    pub(crate) fn on_child_update(
        &mut self,
        target: ChildTarget<'_>,
        update: &acp::SessionUpdate,
        now: Instant,
    ) {
        let id = match target {
            ChildTarget::Session(session_id) => self.node_for_session(session_id, now),
            ChildTarget::Launcher(call_id) => self.node_for_launcher(call_id, now),
        };
        if let Some(node) = id.and_then(|id| self.live.get_mut(&id)) {
            node.apply(update, &self.context.cwd, self.context.animations);
        }
    }

    /// End a running node: it leaves the tree and becomes a [`FinishedSubagent`].
    ///
    /// `message` replaces the child's own final message, for a child known only through a launcher
    /// whose result is the message.
    fn finish(
        &mut self,
        id: NodeId,
        outcome: SubagentOutcome,
        message: Option<String>,
        now: Instant,
    ) {
        let depth = self.depth_of(id);
        let Some(mut node) = self.live.remove(&id) else {
            return;
        };
        if let Some(session_id) = node.session_id.take() {
            self.by_session.insert(session_id, Link::Ended);
        }
        if let Some(call_id) = node.launcher_call_id.take() {
            self.by_launcher.insert(call_id, Link::Ended);
        }
        // Running grandchildren outlive their parent and move up to the top of the block.
        for child in &node.children {
            if let Some(child) = self.live.get_mut(child) {
                child.parent = None;
            }
        }
        let (detail, final_message) = node.take_detail(&self.context.cwd);
        let finished = FinishedSubagent {
            label: node.label().to_owned(),
            kind: node.kind().map(str::to_owned),
            outcome,
            tool_count: node.tool_count,
            elapsed: now.saturating_duration_since(node.started_at),
            message: message.unwrap_or(final_message),
            detail,
            depth,
        };
        if let Some(parent) = node.parent.and_then(|parent| self.live.get_mut(&parent)) {
            parent.children.remove(&id);
            parent
                .nested
                .push(Arc::new(super::SubagentSummaryCell::new(finished.clone())));
            parent.touch();
        }
        self.finished.push(finished);
    }

    fn depth_of(&self, id: NodeId) -> usize {
        let mut depth = 0;
        let mut current = self.live.get(&id).and_then(|node| node.parent);
        while let Some(parent) = current {
            depth += 1;
            current = self.live.get(&parent).and_then(|node| node.parent);
        }
        depth
    }

    /// The root turn ended. Children that were only ever a launcher call (no announcement) were
    /// kept running because the launcher returned at once; they end with the turn. When the turn
    /// was cancelled every running child is shown cancelled.
    pub(crate) fn on_root_turn_end(&mut self, cancelled: bool, now: Instant) {
        let ending: Vec<NodeId> = self
            .live
            .iter()
            .filter(|(_, node)| cancelled || node.session_id.is_none())
            .map(|(id, _)| *id)
            .collect();
        for id in ending {
            let outcome = if cancelled {
                SubagentOutcome::Cancelled
            } else {
                SubagentOutcome::Completed { stop_reason: None }
            };
            self.finish(id, outcome, None, now);
        }
    }

    /// Running nodes in display order, each with its depth: a child follows its parent, children
    /// in the order they appeared. Stops after `limit` nodes, so a frame visits only the nodes it
    /// shows.
    pub(super) fn display_order(&self, limit: usize) -> Vec<(&Node, usize)> {
        let mut out = Vec::new();
        let mut stack: Vec<(NodeId, usize)> = Vec::new();
        for (id, node) in &self.live {
            if node.parent.is_some() {
                continue;
            }
            stack.push((*id, 0));
            while let Some((id, depth)) = stack.pop() {
                if out.len() == limit {
                    return out;
                }
                let Some(node) = self.live.get(&id) else {
                    continue;
                };
                out.push((node, depth));
                stack.extend(node.children.iter().rev().map(|child| (*child, depth + 1)));
            }
        }
        out
    }
}

/// The text a completed tool call returned, for a child known only through its launcher.
fn snapshot_text(snapshot: &ToolSnapshot) -> String {
    snapshot
        .artifacts
        .iter()
        .find_map(|artifact| match artifact {
            Artifact::Text { text } => Some(text.clone()),
            Artifact::Diff(_) => None,
        })
        .unwrap_or_default()
}
