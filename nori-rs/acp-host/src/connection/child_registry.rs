//! Which session a `session/update` belongs to.
//!
//! An agent that supports subagents delivers a child's updates on the same connection as the
//! root's, distinguished only by `sessionId`. The announcement that makes a session a child
//! arrives first, on its parent, so the order of the stream is enough to tell them apart: an
//! update for an announced child is the child's, anything else on the root session is the root's.
//! Updates for a session that is neither are held, bounded, until its announcement arrives.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;

use nori_protocol::acp::v1 as acp;
use tracing::debug;

/// Updates held per unannounced session.
const MAX_PENDING_PER_SESSION: usize = 256;
/// Unannounced sessions held at once.
const MAX_PENDING_SESSIONS: usize = 64;

pub(crate) type SharedChildren = Arc<Mutex<ChildRegistry>>;

#[derive(Debug, Default)]
pub(crate) struct ChildRegistry {
    root: Option<acp::SessionId>,
    children: HashSet<acp::SessionId>,
    pending: HashMap<acp::SessionId, Vec<acp::SessionNotification>>,
}

/// Where an update goes.
#[derive(Debug)]
pub(crate) enum Routed {
    Root(acp::SessionNotification),
    Child(acp::SessionNotification),
    /// Held until the session it belongs to is announced.
    Held,
}

impl ChildRegistry {
    /// The root session's id. Until it is known every unannounced update counts as the root's.
    pub(crate) fn set_root(&mut self, session_id: acp::SessionId) {
        self.root = Some(session_id);
    }

    pub(crate) fn route(&mut self, notification: acp::SessionNotification) -> Routed {
        if self.children.contains(&notification.session_id) {
            return Routed::Child(notification);
        }
        match &self.root {
            Some(root) if *root != notification.session_id => {
                let session_id = notification.session_id.clone();
                if let Some(held) = self.pending.get_mut(&session_id) {
                    if held.len() < MAX_PENDING_PER_SESSION {
                        held.push(notification);
                    } else {
                        debug!(%session_id, "dropping update for an unannounced session");
                    }
                } else if self.pending.len() < MAX_PENDING_SESSIONS {
                    self.pending.insert(session_id, vec![notification]);
                } else {
                    debug!(%session_id, "dropping update for an unannounced session");
                }
                Routed::Held
            }
            _ => Routed::Root(notification),
        }
    }

    /// Record `session_id` as a child and release the updates held for it, in order.
    pub(crate) fn announce(
        &mut self,
        session_id: &acp::SessionId,
    ) -> Vec<acp::SessionNotification> {
        self.children.insert(session_id.clone());
        self.pending.remove(session_id).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(session: &str, text: &str) -> acp::SessionNotification {
        acp::SessionNotification::new(
            session.to_string(),
            acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
                acp::TextContent::new(text),
            ))),
        )
    }

    fn describe(routed: &Routed) -> &'static str {
        match routed {
            Routed::Root(_) => "root",
            Routed::Child(_) => "child",
            Routed::Held => "held",
        }
    }

    #[test]
    fn announced_children_are_routed_apart_from_the_root() {
        let mut registry = ChildRegistry::default();
        registry.set_root(acp::SessionId::new("root"));
        assert_eq!(describe(&registry.route(update("root", "a"))), "root");
        assert!(registry.announce(&acp::SessionId::new("child")).is_empty());
        assert_eq!(describe(&registry.route(update("child", "b"))), "child");
        assert_eq!(describe(&registry.route(update("root", "c"))), "root");
    }

    #[test]
    fn updates_before_the_announcement_are_released_in_order() {
        let mut registry = ChildRegistry::default();
        registry.set_root(acp::SessionId::new("root"));
        assert_eq!(describe(&registry.route(update("child", "1"))), "held");
        assert_eq!(describe(&registry.route(update("child", "2"))), "held");
        let released = registry.announce(&acp::SessionId::new("child"));
        let ids: Vec<String> = released
            .iter()
            .map(|n| match &n.update {
                acp::SessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
                    acp::ContentBlock::Text(text) => text.text.clone(),
                    _ => String::new(),
                },
                _ => String::new(),
            })
            .collect();
        assert_eq!(ids, vec!["1".to_string(), "2".to_string()]);
    }

    #[test]
    fn held_updates_are_bounded() {
        let mut registry = ChildRegistry::default();
        registry.set_root(acp::SessionId::new("root"));
        for index in 0..(MAX_PENDING_PER_SESSION + 50) {
            let _ = registry.route(update("never", &index.to_string()));
        }
        assert_eq!(
            registry.announce(&acp::SessionId::new("never")).len(),
            MAX_PENDING_PER_SESSION
        );
        for index in 0..(MAX_PENDING_SESSIONS + 10) {
            let _ = registry.route(update(&format!("s{index}"), "x"));
        }
        assert_eq!(registry.pending.len(), MAX_PENDING_SESSIONS);
    }

    #[test]
    fn everything_is_the_roots_until_the_root_is_known() {
        let mut registry = ChildRegistry::default();
        assert_eq!(describe(&registry.route(update("whoever", "a"))), "root");
    }
}
