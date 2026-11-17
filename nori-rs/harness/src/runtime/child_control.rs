//! Access to the independent control plane of an orchestrated child.

use super::HarnessCommand;
use super::HarnessHandle;
use nori_protocol::acp;

impl HarnessHandle {
    pub(crate) async fn current_session_id(&self) -> anyhow::Result<acp::v1::SessionId> {
        self.request(|response_tx| HarnessCommand::CurrentSessionId { response_tx })
            .await
    }

    /// Resolve a child view to its own harness control plane.
    ///
    /// Native provider children without an independent harness return `None`.
    /// Callers must not substitute the parent handle for such a child.
    pub async fn controlled_subagent_handle(
        &self,
        session_id: acp::v1::SessionId,
    ) -> anyhow::Result<Option<Self>> {
        self.request(|response_tx| HarnessCommand::ControlledSubagentHandle {
            session_id,
            response_tx,
        })
        .await
    }
    /// Read the focused child's configuration without changing the root snapshot.
    pub async fn child_config_options(
        &self,
        session_id: acp::v1::SessionId,
    ) -> anyhow::Result<Vec<acp::v1::SessionConfigOption>> {
        self.request(|response_tx| HarnessCommand::ChildConfigOptions {
            session_id,
            response_tx,
        })
        .await
    }

    /// Change a configuration option on one child session.
    pub async fn set_child_config_option(
        &self,
        session_id: acp::v1::SessionId,
        config_id: String,
        value: String,
    ) -> anyhow::Result<Vec<acp::v1::SessionConfigOption>> {
        self.request(|response_tx| HarnessCommand::SetChildConfigOption {
            session_id,
            config_id,
            value,
            response_tx,
        })
        .await
    }
}
