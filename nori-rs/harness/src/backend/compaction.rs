//! Summarize-and-swap `/compact`, for agents that do not compact on their own (see
//! `crate::compact` for the prompt, the checks, and the checkpoint).

use std::sync::atomic::Ordering;

use nori_protocol::NoriEvent;

use super::*;
use crate::backend::session_runtime_driver::CompletedTurn;
use crate::normalized::session_runtime::QueuedPrompt;
use crate::normalized::session_runtime::QueuedPromptKind;

impl AcpBackend {
    /// Ask the agent, in the live session, for the summary (`text` is the prompt or the reminder).
    pub(super) async fn submit_compaction_prompt(&self, id: &str, text: &str) {
        let _ = self
            .session_event_tx
            .send(session_runtime_driver::SessionRuntimeInput::Reducer(
                session_reducer::InboundEvent::PromptSubmit(QueuedPrompt {
                    event_id: id.to_string(),
                    kind: QueuedPromptKind::Compact,
                    text: text.to_string(),
                    prompt_meta: None,
                    original_content: Vec::new(),
                    content: vec![acp::ContentBlock::Text(acp::TextContent::new(text))],
                    display_text: None,
                }),
            ))
            .await;
    }

    /// The summary turn ended: check the summary, remind the agent of the template once, then
    /// swap to a new session that opens with the checkpoint. On any failure the old session
    /// stays, and the user is told why.
    pub(super) async fn finish_compaction(&self, completed_turn: &CompletedTurn) {
        let summary = completed_turn
            .last_agent_message
            .clone()
            .unwrap_or_default();
        if completed_turn.stop_reason != acp::StopReason::EndTurn {
            self.compact_nudged.store(false, Ordering::Release);
            self.notice("Compaction stopped before the agent wrote its summary; the conversation goes on as it was.")
                .await;
            return;
        }
        if !crate::compact::fills_template(&summary) {
            if !self.compact_nudged.swap(true, Ordering::AcqRel) {
                self.submit_compaction_prompt(&generate_id(), crate::compact::NUDGE)
                    .await;
                return;
            }
            self.compact_nudged.store(false, Ordering::Release);
            self.notice("Compaction failed: the agent did not write its summary in the template, so the conversation goes on as it was.")
                .await;
            return;
        }
        self.compact_nudged.store(false, Ordering::Release);
        let recent = self.recent_context().await;
        *self.pending_compact_summary.lock().await =
            Some(crate::compact::checkpoint(&summary, recent.as_deref()));
        if let Err(error) = self
            .swap_active_session(session_swap::SessionSwapMode::NewAfterCompact)
            .await
        {
            warn!("Failed to create new session after compact: {error}");
            self.pending_compact_summary.lock().await.take();
            self.notice(&format!(
                "Compaction failed: no new session could be started ({error}); the conversation goes on as it was."
            ))
            .await;
            return;
        }
        self.forward_client_event(crate::normalized::ClientEvent::ContextCompacted(
            crate::normalized::ContextCompacted {
                summary: Some(summary),
            },
        ))
        .await;
        self.notice("Heads up: Long conversations and multiple compactions can cause the model to be less accurate. Start a new conversation when possible to keep conversations small and targeted.")
            .await;
    }

    /// The newest exchanges of this session's transcript, for the checkpoint.
    async fn recent_context(&self) -> Option<String> {
        let recorder = self.transcript_recorder.read().await.clone()?;
        recorder.flush().await.ok()?;
        let transcript = crate::transcript::TranscriptLoader::new(self.nori_home.clone())
            .load_transcript(recorder.project_id(), recorder.session_id())
            .await
            .ok()?;
        crate::compact::recent_context(&transcript)
    }

    async fn notice(&self, message: &str) {
        let _ = self
            .backend_event_tx
            .send(BackendEvent::Public(SessionEvent::Nori(NoriEvent::Notice(
                nori_protocol::Notice {
                    message: message.to_string(),
                },
            ))))
            .await;
    }
}
