//! Having an agent commit the working tree's changes, on a hotkey or after a turn.

use super::*;

impl App {
    /// Look at what changed, off the event loop, and carry on in [`Self::submit_commit_request`].
    pub(super) fn request_commit(&mut self, automatic: bool) {
        // The turn that just ended is the commit request's own; committing again would loop.
        if automatic && std::mem::take(&mut self.commit_turn_pending) {
            return;
        }
        let cwd = self.config.cwd.clone();
        let tx = self.app_event_tx.clone();
        tokio::task::spawn_blocking(move || {
            let changes = nori_config::GitRepo::discover(&cwd)
                .map(|repo| repo.changed_files())
                .unwrap_or_default();
            tx.send(AppEvent::CommitChangesReady { automatic, changes });
        });
    }

    /// Ask the agent to commit `changes`, or say there is nothing to do.
    pub(super) fn submit_commit_request(
        &mut self,
        automatic: bool,
        changes: Vec<nori_config::ChangedFile>,
    ) {
        if changes.is_empty() {
            if !automatic {
                self.chat_widget.add_info_message(
                    "Nothing to commit: the working tree is clean.".to_owned(),
                    None,
                );
            }
            return;
        }
        let prompt = crate::nori::git_commit::commit_prompt(
            &self.config.git,
            &changes,
            &self.config.active_agent,
        );
        self.commit_turn_pending = true;
        self.chat_widget.submit_user_message_text(prompt);
    }
}
