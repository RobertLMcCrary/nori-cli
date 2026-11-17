//! Rebind a rewound conversation to a fresh ACP engine session.

use super::*;

impl AcpBackend {
    /// Rewind the canonical visible head and replace the opaque ACP session.
    pub(crate) async fn rewind_and_rebind(
        &self,
        expected_session_id: String,
        head: Option<i64>,
    ) -> Result<()> {
        self.rewind_checked(expected_session_id, head, None).await
    }

    /// Rewind for a Pkl program only if its original generation is still current.
    pub(crate) async fn rewind_program(
        &self,
        expected_session_id: String,
        head: Option<i64>,
        expected_generation: i64,
    ) -> Result<()> {
        self.rewind_checked(expected_session_id, head, Some(expected_generation))
            .await
    }

    async fn rewind_checked(
        &self,
        expected_session_id: String,
        head: Option<i64>,
        expected_generation: Option<i64>,
    ) -> Result<()> {
        let _program_admission = self.program_effects.try_write().map_err(|_| {
            anyhow::anyhow!("Cannot rewind while a conversation program effect is running")
        })?;
        if let Some(expected) = expected_generation {
            anyhow::ensure!(
                self.orchestrator.admission_generation() == expected,
                "The conversation was rewound before this program could continue"
            );
        }
        let recorder = self
            .transcript_recorder
            .read()
            .await
            .clone()
            .ok_or_else(|| anyhow::anyhow!("The conversation has no journal"))?;
        anyhow::ensure!(
            recorder.session_id() == expected_session_id,
            "The conversation changed before rewind"
        );
        let _admission = self.orchestrator.rewind_admission().await;
        anyhow::ensure!(
            !self.orchestrator.has_pending_work().await,
            "Cannot rewind while delegated work is running or its result is pending"
        );
        anyhow::ensure!(
            !self.native_children.has_pending_work().await,
            "Cannot rewind while native child work is running or queued"
        );
        let driver = self.session_driver.lock().await;
        anyhow::ensure!(
            matches!(driver.public_phase(), nori_protocol::SessionPhase::Idle)
                && driver.queue_len() == 0,
            "Cannot rewind while a turn is active or queued"
        );
        drop(driver);
        recorder.flush().await?;
        let context = recorder.projected_context(head).await?;
        self.swap_active_session(session_swap::SessionSwapMode::Rewind {
            head,
            context,
            recorder,
        })
        .await?;
        self.orchestrator.advance_generation();
        Ok(())
    }
}
