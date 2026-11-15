//! How long each replayed turn had been running, for the "Worked for" separators of a resumed
//! session.
//!
//! A live client times a turn itself, from the prompt to each run of agent text that follows a
//! tool call, pausing while it waits on the user for a permission. A replay has no running turn
//! to time, but the transcript recorded when every event was observed. So the first chunk of
//! each run of agent text says how long its turn had been running at that point
//! (`nori_protocol::turn_elapsed_meta`), measured the way the client's timer measures it.

use chrono::DateTime;
use chrono::FixedOffset;
use chrono::TimeDelta;
use nori_protocol::AcpEvent;
use nori_protocol::NoriEvent;
use nori_protocol::SessionEvent;
use nori_protocol::SessionPhase;
use nori_protocol::acp::v1 as acp;

use crate::transcript::SessionEventEntry;

#[derive(Debug, Default)]
pub(super) struct TurnTiming {
    started: Option<DateTime<FixedOffset>>,
    /// Since when the turn has been waiting on the user to answer a permission request.
    waiting_since: Option<DateTime<FixedOffset>>,
    /// How long the turn waited on the user so far.
    waited: TimeDelta,
    /// The last update replayed was agent text, so the next one continues its run.
    in_text: bool,
}

impl TurnTiming {
    /// A user message recorded at `ts` starts a turn.
    pub(super) fn start_turn(&mut self, ts: &str) {
        *self = Self {
            started: parse(Some(ts)),
            ..Self::default()
        };
    }

    /// Account for a recorded event: the turn it starts or ends, and the time it spent waiting.
    pub(super) fn observe(&mut self, entry: &SessionEventEntry) {
        if let Some(since) = self.waiting_since.take()
            && let Some(at) = parse(entry.observed_at.as_deref())
        {
            // Whatever is recorded after a permission request comes after the user answered it.
            self.waited += at - since;
        }
        match &entry.event {
            SessionEvent::Acp(AcpEvent::Request {
                request: acp::AgentRequest::RequestPermissionRequest(_),
                ..
            }) => self.waiting_since = parse(entry.observed_at.as_deref()),
            SessionEvent::Nori(NoriEvent::SessionPhaseChanged(SessionPhase::Prompting {
                ..
            })) if self.started.is_none() => {
                self.started = parse(entry.observed_at.as_deref());
            }
            SessionEvent::Nori(NoriEvent::SessionPhaseChanged(SessionPhase::Idle)) => {
                self.started = None;
            }
            _ => {}
        }
    }

    /// `notification`, observed at `observed_at`, as it is replayed: the first chunk of a run of
    /// agent text carries how long its turn had been running.
    pub(super) fn replayed(
        &mut self,
        notification: &acp::SessionNotification,
        observed_at: Option<&str>,
    ) -> SessionEvent {
        let mut notification = notification.clone();
        let is_text = matches!(
            notification.update,
            acp::SessionUpdate::AgentMessageChunk(_)
        );
        let starts_text_run = is_text && !self.in_text;
        self.in_text = is_text;
        if starts_text_run
            && let (Some(started), Some(at)) = (self.started, parse(observed_at))
            && let acp::SessionUpdate::AgentMessageChunk(chunk) = &mut notification.update
        {
            let elapsed = (at - started - self.waited).to_std().unwrap_or_default();
            chunk
                .meta
                .get_or_insert_with(Default::default)
                .extend(nori_protocol::turn_elapsed_meta(elapsed));
        }
        SessionEvent::Acp(AcpEvent::Notification(
            acp::AgentNotification::SessionNotification(notification),
        ))
    }
}

fn parse(ts: Option<&str>) -> Option<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc3339(ts?).ok()
}
