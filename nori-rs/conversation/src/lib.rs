//! An append-only, non-executing conversation event log.
//!
//! The log is a sequence of line-delimited Pkl properties. Replaying it only
//! reduces data; it never evaluates Pkl or repeats external effects. A log
//! file store serializes readers and writers with OS locks.
//!
//! Complete records are Pkl properties. If a process dies mid-record, recovery
//! preserves those bytes and skips the interrupted record. The resulting journal
//! may not parse as a whole Pkl module; export its replayed projection to a new
//! file when an evaluable derived artifact is needed. Never rewrite the journal.

mod codec;
mod log;
mod projection;
mod store;

pub use log::ConversationLog;
pub use projection::Projection;
pub use store::ConversationStore;
pub use store::FileStore;
pub use store::MemoryStore;

/// A stable, positive event identifier, scoped to one log.
pub type EventId = i64;

/// Inclusive bounds within a particular visible conversation head.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SliceRef {
    pub start: EventId,
    pub end: EventId,
}

/// Data to append to the conversation. Operations are recorded, not executed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Message {
        role: String,
        content: String,
    },
    ToolCall {
        id: String,
        name: String,
        input: String,
    },
    ToolResult {
        id: String,
        output: String,
    },
    Rewind {
        head: Option<EventId>,
    },
    Fork {
        source: SliceRef,
    },
    Operation {
        name: String,
        args: String,
    },
    Command {
        name: String,
        payload: String,
    },
    SessionEvent {
        payload: String,
    },
    /// A future operation preserved as its original Pkl property.
    Unknown {
        form: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub id: EventId,
    pub command_id: Option<String>,
    pub event: Event,
}

#[cfg(test)]
mod tests;
