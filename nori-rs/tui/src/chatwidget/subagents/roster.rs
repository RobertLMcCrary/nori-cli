//! The subagent tree as the switcher sees it: one entry per subagent, running or finished, and the
//! tail of any one's conversation.

use std::time::Instant;

use nori_protocol::SubagentOutcome;
use ratatui::text::Line;

use super::SPINNER_PERIOD;
use super::SubagentTree;
use super::node::NodeId;
use crate::chatwidget::ChatWidget;
use crate::history_cell::AgentMarkdownCell;
use crate::subagent_switcher::Body;
use crate::subagent_switcher::BodyQuery;
use crate::subagent_switcher::Entry;
use crate::subagent_switcher::Roster;
use crate::subagent_switcher::Status;
use crate::subagent_switcher::SubagentKey;
use crate::subagent_switcher::Tail;
use crate::subagent_switcher::Target;
use crate::subagent_switcher::cell_lines;

impl SubagentTree {
    /// Every subagent so far: the running ones and the finished ones.
    pub(crate) fn entries(&self, now: Instant) -> (Vec<Entry>, Vec<(SubagentKey, SubagentKey)>) {
        let mut entries = Vec::with_capacity(self.live.len() + self.ended.len());
        for (id, node) in &self.live {
            let parent = node
                .parent
                .and_then(|parent| self.live.get(&parent))
                .map(|parent| parent.label().to_owned());
            entries.push(Entry {
                target: Target::Subagent(key(*id)),
                title: node.label().to_owned(),
                parent,
                agent: node.kind().map(str::to_owned),
                model: None,
                status: Status::Running,
                detail: None,
                elapsed: now.saturating_duration_since(node.started_at),
                ended_ago: None,
                tool_count: node.tool_count,
                revision: node.revision,
                active_at: node.active_at,
            });
        }
        for (id, record) in &self.ended {
            let finished = &record.finished;
            let (status, detail) = match &finished.outcome {
                SubagentOutcome::Completed { .. } => (Status::Completed, None),
                SubagentOutcome::Failed { reason } => (Status::Failed, reason.clone()),
                SubagentOutcome::Cancelled => (Status::Cancelled, None),
            };
            entries.push(Entry {
                target: Target::Subagent(key(*id)),
                title: finished.label.clone(),
                parent: record.parent.clone(),
                agent: finished.kind.clone(),
                model: None,
                status,
                detail,
                elapsed: finished.elapsed,
                ended_ago: Some(now.saturating_duration_since(record.ended_at)),
                tool_count: finished.tool_count,
                revision: ENDED_REVISION,
                active_at: record.ended_at,
            });
        }
        let replaced = self
            .replaced
            .iter()
            .map(|(old, new)| (key(*old), key(*new)))
            .collect();
        (entries, replaced)
    }

    /// The last `rows` rows of one subagent's conversation at `width`, and the revision they show.
    pub(crate) fn body(
        &self,
        subagent: SubagentKey,
        width: u16,
        rows: usize,
    ) -> Option<(i64, Vec<Line<'static>>)> {
        let id = NodeId(subagent.0);
        if let Some(node) = self.live.get(&id) {
            return Some((
                node.revision,
                node.detail_lines(&self.context.cwd, width, rows),
            ));
        }
        let finished = &self.ended.get(&id)?.finished;
        let mut tail = Tail::new(rows);
        for cell in finished.detail.iter().rev() {
            if tail.is_full() {
                break;
            }
            tail.push_front(cell_lines(cell.as_ref(), width));
        }
        // A child known only through its launcher has no detail, only the result.
        if finished.detail.is_empty() && !finished.message.trim().is_empty() {
            tail.push_front(cell_lines(
                &AgentMarkdownCell::new(finished.message.clone(), &self.context.cwd),
                width,
            ));
        }
        Some((ENDED_REVISION, tail.finish()))
    }
}

/// A finished subagent never changes again, so one revision stands for all of its states.
const ENDED_REVISION: i64 = -1;

fn key(id: NodeId) -> SubagentKey {
    SubagentKey(id.0)
}

impl ChatWidget {
    /// The roster for the subagent switcher, with the root session first.
    pub(crate) fn subagent_roster(&self, now: Instant) -> Roster {
        let (subagents, replaced) = self.subagents.entries(now);
        let agent = self.bottom_pane.agent_display_name().to_owned();
        let model = self
            .agent_config
            .option_for_category(&nori_protocol::acp::v1::SessionConfigOptionCategory::Model)
            .map(crate::nori::agent_config_state::AgentConfigOption::display_value);
        let main = Entry {
            target: Target::Main,
            title: "main".to_owned(),
            parent: None,
            agent: (!agent.is_empty()).then_some(agent),
            model,
            status: if self.bottom_pane.is_task_running() {
                Status::Running
            } else {
                Status::Idle
            },
            detail: None,
            elapsed: std::time::Duration::ZERO,
            ended_ago: None,
            tool_count: 0,
            revision: 0,
            active_at: now,
        };
        Roster {
            main,
            subagents,
            replaced,
            animations: self.subagents.animations(),
            tick: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| {
                    i64::try_from(since.as_millis() / SPINNER_PERIOD.as_millis().max(1))
                        .unwrap_or(0)
                }),
        }
    }

    /// The conversation tail of one subagent. The root session's is built by the app, which owns
    /// its transcript.
    pub(crate) fn subagent_body(&self, query: BodyQuery, now: Instant) -> Option<Body> {
        let Target::Subagent(subagent) = query.target else {
            return None;
        };
        let (revision, lines) = self.subagents.body(subagent, query.width, query.rows)?;
        Some(Body {
            query,
            revision,
            lines,
            built_at: now,
        })
    }

    /// Keep animation frames coming while a subagent runs and the switcher is up.
    pub(crate) fn subagent_overlay_tick(&mut self) {
        self.subagent_pre_draw_tick();
    }
}
