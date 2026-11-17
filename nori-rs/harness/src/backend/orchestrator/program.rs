//! Actor operations exposed to Pkl programs.
use super::*;

impl Orchestrator {
    pub(crate) fn install_program_backend(
        &self,
        backend: std::sync::Weak<crate::backend::AcpBackend>,
    ) {
        let _ = self.inner.program_backend.set(backend);
    }

    pub(crate) async fn execute_conversation_program(
        &self,
        source: String,
    ) -> anyhow::Result<crate::runtime::ConversationProgramResult> {
        let backend = self
            .inner
            .program_backend
            .get()
            .and_then(std::sync::Weak::upgrade)
            .ok_or_else(|| anyhow::anyhow!("conversation runtime is no longer available"))?;
        crate::runtime::program::execute(&source, &backend, &backend.transcript_recorder_cell())
            .await
    }

    pub(crate) async fn spawn_program_actor(
        &self,
        actor: String,
        agent: String,
    ) -> Result<String, String> {
        let snapshot = self
            .inner
            .delegate(launch::Delegation {
                task: actor.clone(),
                agent: Some(agent),
                model: None,
                title: Some(actor),
                role: Some(launch::Role::General),
                isolation: launch::Isolation::Shared,
                mode: launch::Mode::Background,
                timeout: None,
                client_request_id: None,
                options: Vec::new(),
                fork_context: None,
                fork_source: None,
                fork_entries: Vec::new(),
                parent_view: None,
            })
            .await?;
        self.inner
            .state
            .lock()
            .await
            .tasks
            .iter()
            .find(|task| task.id == snapshot.task_id)
            .map(|task| task.view_id.to_string())
            .ok_or_else(|| "child was not registered".to_owned())
    }
}

impl crate::backend::AcpBackend {
    /// Hold while dispatching one Pkl effect so a rewind cannot race admission.
    pub(crate) async fn program_effect_admission(&self) -> tokio::sync::RwLockReadGuard<'_, ()> {
        self.program_effects.read().await
    }

    /// Generation of the conversation's currently bound ACP session.
    pub(crate) fn program_conversation_generation(&self) -> i64 {
        self.orchestrator.admission_generation()
    }
}
