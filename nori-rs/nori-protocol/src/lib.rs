//! Public types for embedding a Nori ACP session.
//!
//! ACP owns agent/client semantics. Nori adds only the lifecycle and product
//! notifications that ACP does not define.

mod session_event;
mod subagent;

pub use agent_client_protocol_schema as acp;
pub use session_event::*;
pub use subagent::*;

/// ACP `_meta` key correlating a prompt with user-message chunks that echo it.
pub const PROMPT_ECHO_ID_META_KEY: &str = "nori.dev/promptEchoId";

/// Identity of one submitted prompt, carried in `_meta` under [`PROMPT_ECHO_ID_META_KEY`] on the
/// prompt and on every user-message chunk that echoes it.
///
/// A client that shows its prompt before the echo arrives recognizes the echo by this id, never by
/// comparing text.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct PromptEchoId(String);

impl PromptEchoId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `_meta` that carries this id.
    pub fn to_meta(&self) -> acp::v1::Meta {
        acp::v1::Meta::from_iter([(
            PROMPT_ECHO_ID_META_KEY.to_string(),
            serde_json::Value::String(self.0.clone()),
        )])
    }

    /// The id that `meta` carries, if any.
    pub fn in_meta(meta: Option<&acp::v1::Meta>) -> Option<&str> {
        meta?.get(PROMPT_ECHO_ID_META_KEY)?.as_str()
    }
}
