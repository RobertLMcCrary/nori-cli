//! Load a selected conversation before adding its reference to the current draft.

use super::App;
use crate::app_event::AppEvent;

impl App {
    pub(super) fn load_session_reference(
        &self,
        project_id: String,
        session_id: String,
        agent: String,
        target: crate::subagent_switcher::Target,
    ) {
        let nori_home = self.config.nori_home.clone();
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let loader = nori_harness::transcript::TranscriptLoader::new(nori_home);
            match loader.load_transcript(&project_id, &session_id).await {
                Ok(transcript) => {
                    let text = crate::nori::reference_session_picker::format_reference(
                        &project_id,
                        &session_id,
                        &agent,
                        &transcript,
                    );
                    tx.send(AppEvent::InsertSessionReference { text, target });
                }
                Err(error) => {
                    let message = format!("Failed to load session {session_id}: {error}");
                    match target {
                        crate::subagent_switcher::Target::Main => {
                            tx.send(AppEvent::InsertHistoryCell(Box::new(
                                crate::history_cell::new_error_event(message),
                            )))
                        }
                        crate::subagent_switcher::Target::Subagent(key) => {
                            tx.send(AppEvent::ChildSessionOutput {
                                key,
                                text: format!("{message}\n"),
                            })
                        }
                    }
                }
            }
        });
    }
}
