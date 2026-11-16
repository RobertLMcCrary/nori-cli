//! The session's workflow: what the user's and the project's Steel scripts say about how the
//! agent works (see `docs/workflows.md`).
//!
//! Nori loads the scripts in the background when the session starts (the project's only in a
//! trusted directory), tells the agent the active workflow's rules with its first prompt, takes
//! inline workflow clauses out of prompts, and after a turn sends the prompts the workflow hooks
//! to what happened (files changed, a commit landed, the turn ended) as hidden turns, at most
//! the workflow's turn budget in a row. A workflow with a commit agent hands the after-change
//! work to that agent as a delegated task instead of to the session's agent.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use nori_script::HookEvent;
use nori_script::ScriptHost;
use nori_script::Workflow;

use super::*;
use crate::normalized::session_runtime::QueuedPrompt;
use crate::normalized::session_runtime::QueuedPromptKind;
use session_runtime_driver::CompletedTurn;

/// Hidden hook turns that may follow one user turn when the workflow does not say.
const DEFAULT_HOOK_TURNS: i64 = 3;

/// One session's workflow state.
#[derive(Default)]
pub(crate) struct WorkflowState {
    /// The thread that runs the scripts, once started.
    host: Option<ScriptHost>,
    /// The workflow in force: the scripts' default, or one an inline clause set up.
    active: Option<Workflow>,
    /// The agent has been told the active workflow's rules.
    told: bool,
    /// Hook turns sent since the user's last turn.
    hook_turns: i64,
    /// The `on-stop` hook already ran since the user's last turn.
    stop_hooked: bool,
    /// The working tree when the user's current turn started.
    before: Option<TreeState>,
}

/// What a turn could have changed: HEAD, and every changed file with its size and mtime.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TreeState {
    head: Option<String>,
    files: Vec<(String, i64, i128)>,
}

fn tree_state(cwd: &Path) -> Option<TreeState> {
    let repo = nori_config::GitRepo::discover(cwd)?;
    let root = repo.work_dir()?;
    let files = repo
        .changed_files()
        .into_iter()
        .map(|file| {
            let meta = std::fs::metadata(root.join(&file.path)).ok();
            let len = meta
                .as_ref()
                .map_or(-1, |meta| i64::try_from(meta.len()).unwrap_or(i64::MAX));
            let modified = meta
                .and_then(|meta| meta.modified().ok())
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(-1, |since| i128::try_from(since.as_nanos()).unwrap_or(-1));
            (file.path, len, modified)
        })
        .collect();
    Some(TreeState {
        head: repo.head_commit(),
        files,
    })
}

async fn tree_state_of(cwd: PathBuf) -> Option<TreeState> {
    tokio::task::spawn_blocking(move || tree_state(&cwd))
        .await
        .ok()
        .flatten()
}

impl WorkflowState {
    fn hook_budget(&self) -> i64 {
        self.active
            .as_ref()
            .and_then(|workflow| workflow.max_turns)
            .unwrap_or(DEFAULT_HOOK_TURNS)
    }
}

impl AcpBackend {
    /// Load the workflow scripts off the session's start-up path. A prompt sent before they load
    /// runs without a workflow; a script error is shown once. Without a script nothing starts:
    /// a script engine costs about 100 ms of a thread. A delegated child follows its parent's
    /// workflow, which its task carries, so it loads none.
    pub(super) fn load_workflow(&self) {
        if self.orchestrator.is_child() {
            return;
        }
        let global =
            Some(nori_script::global_script_path(&self.nori_home)).filter(|path| path.exists());
        let project = self
            .orchestrator
            .project_trusted()
            .then(|| nori_script::project_script_path(&self.cwd))
            .filter(|path| path.exists());
        if global.is_none() && project.is_none() {
            return;
        }
        let workflow = Arc::clone(&self.workflow);
        let events = self.backend_event_tx.clone();
        tokio::spawn(async move {
            let started = tokio::task::spawn_blocking(move || {
                let host = ScriptHost::spawn(global, project).map_err(|error| error.to_string())?;
                let loaded = host.loaded().map_err(|error| error.to_string());
                Ok::<_, String>((host, loaded))
            })
            .await;
            match started {
                Ok(Ok((host, loaded))) => {
                    let mut state = workflow.lock().await;
                    state.host = Some(host);
                    match loaded {
                        Ok(loaded) => state.active = loaded.default_workflow().cloned(),
                        Err(error) => {
                            drop(state);
                            notice(&events, format!("Workflow script: {error}")).await;
                        }
                    }
                }
                Ok(Err(error)) => notice(&events, format!("Workflow scripts: {error}")).await,
                Err(error) => debug!("Loading workflow scripts stopped: {error}"),
            }
        });
    }

    /// Apply a user prompt's inline workflow clauses and, the first time a workflow is in force,
    /// its rules. Returns the prompt text without the clauses, and the rules to put before it.
    pub(super) async fn workflow_for_prompt(&self, prompt_text: &str) -> (String, Option<String>) {
        let mut text = prompt_text.to_owned();
        let inline = if nori_script::has_inline_dsl(prompt_text) {
            // A prompt sent before the scripts load still gets its clauses applied.
            let host = self.workflow.lock().await.host.clone();
            let prompt = prompt_text.to_owned();
            let applied = tokio::task::spawn_blocking(move || {
                let host = match host {
                    Some(host) => host,
                    None => ScriptHost::spawn(None, None).map_err(|error| error.to_string())?,
                };
                let applied = host
                    .apply_inline(&prompt)
                    .map_err(|error| error.to_string());
                Ok::<_, String>((host, applied))
            })
            .await
            .unwrap_or_else(|error| Err(error.to_string()));
            Some(match applied {
                Ok((host, applied)) => {
                    // Started for this clause: keep it for the next.
                    self.workflow.lock().await.host.get_or_insert(host);
                    applied
                }
                Err(error) => Err(error),
            })
        } else {
            None
        };
        let mut state = self.workflow.lock().await;
        if let Some(applied) = inline {
            match applied {
                Ok(inline) => {
                    text = inline.prompt;
                    if let Some(workflow) = inline.workflow {
                        state.active = Some(workflow);
                        state.told = false;
                    }
                }
                Err(error) => {
                    notice(&self.backend_event_tx, format!("Workflow clause: {error}")).await;
                }
            }
        }
        state.hook_turns = 0;
        state.stop_hooked = false;
        let rules = state
            .active
            .as_ref()
            .filter(|_| !state.told)
            .map(Workflow::instructions)
            .filter(|rules| !rules.is_empty())
            .map(|rules| format!("<workflow>\n{rules}</workflow>"));
        state.told |= rules.is_some();
        (text, rules)
    }

    /// A user turn is starting: note the working tree, to tell afterwards what it changed.
    pub(super) async fn workflow_turn_started(&self) {
        if self.workflow.lock().await.active.is_none() {
            return;
        }
        let before = tree_state_of(self.cwd.clone()).await;
        self.workflow.lock().await.before = before;
    }

    /// After a turn, send the prompts the workflow hooks to what happened, if the session is idle
    /// and the turn budget allows.
    pub(super) async fn maybe_submit_workflow_hook(&self, completed_turn: &CompletedTurn) {
        if completed_turn.stop_reason != acp::StopReason::EndTurn {
            return;
        }
        let (workflow, before, stop_hooked) = {
            let state = self.workflow.lock().await;
            let Some(workflow) = state.active.clone() else {
                return;
            };
            if workflow.hooks.is_empty() || state.hook_turns >= state.hook_budget() {
                return;
            }
            (workflow, state.before.clone(), state.stop_hooked)
        };
        // Without a snapshot from the turn's start (the workflow loaded during the turn, or this
        // is not a git repository) nothing is known to have changed.
        let after = tree_state_of(self.cwd.clone()).await;
        let (changed, committed) = match (&before, &after) {
            (Some(before), Some(after)) => (before != after, before.head != after.head),
            _ => (false, false),
        };
        let events: &[HookEvent] = match completed_turn.prompt.kind {
            // Hooks answer what the user's turn did; a hook turn's own commit may still be
            // followed up, but its edits never start another round.
            QueuedPromptKind::User | QueuedPromptKind::TaskResults => &[
                HookEvent::AfterChange,
                HookEvent::AfterCommit,
                HookEvent::AfterTurn,
                HookEvent::BeforeStop,
            ],
            QueuedPromptKind::WorkflowHook => &[HookEvent::AfterCommit, HookEvent::BeforeStop],
            QueuedPromptKind::GoalContinuation
            | QueuedPromptKind::Compact
            | QueuedPromptKind::NativeCompact => return,
        };
        let Some(event) = events.iter().copied().find(|event| {
            let happened = match event {
                HookEvent::AfterChange => changed,
                HookEvent::AfterCommit => committed,
                HookEvent::AfterTurn => true,
                // The last word before control returns to the user, once per user turn.
                HookEvent::BeforeStop => !stop_hooked,
                // Nori cannot see a pull request coming; the agent is told the rule instead.
                HookEvent::BeforePr => false,
            };
            happened && workflow.hook_prompts(*event).next().is_some()
        }) else {
            return;
        };
        let prompt = workflow.hook_prompts(event).collect::<Vec<_>>().join("\n");

        // A designated commit agent writes the commits, so the session's agent keeps going.
        if event == HookEvent::AfterChange
            && let Some(agent) = workflow
                .commit
                .as_ref()
                .and_then(|commit| commit.agent.clone())
        {
            self.workflow.lock().await.hook_turns += 1;
            let task = format!("{prompt}\n\n{}", workflow.instructions());
            if let Err(error) = self
                .orchestrator
                .delegate_background(
                    "Commit the changes".to_owned(),
                    task,
                    agent.agent,
                    agent.model,
                )
                .await
            {
                notice(&self.backend_event_tx, format!("Commit agent: {error}")).await;
            }
            return;
        }

        if self.session_driver.lock().await.prompt_is_busy() {
            return;
        }
        {
            let mut state = self.workflow.lock().await;
            state.hook_turns += 1;
            state.stop_hooked |= event == HookEvent::BeforeStop;
        }
        let form = event.form();
        // Routine, so an info line rather than a warning.
        let _ = self
            .backend_event_tx
            .send(BackendEvent::Public(SessionEvent::Nori(
                nori_protocol::NoriEvent::HookOutput(nori_protocol::HookOutput {
                    message: format!("Workflow \"{}\" · {form}", workflow.name),
                    level: nori_protocol::HookOutputLevel::Info,
                }),
            )))
            .await;
        let text = format!("<workflow_hook event=\"{form}\">\n{prompt}\n</workflow_hook>");
        let _ = self
            .session_event_tx
            .send(session_runtime_driver::SessionRuntimeInput::Reducer(
                session_reducer::InboundEvent::PromptSubmit(QueuedPrompt {
                    event_id: format!("workflow-hook-{}", uuid::Uuid::new_v4()),
                    kind: QueuedPromptKind::WorkflowHook,
                    text: text.clone(),
                    prompt_meta: None,
                    original_content: Vec::new(),
                    content: vec![acp::ContentBlock::Text(acp::TextContent::new(text))],
                    display_text: None,
                }),
            ))
            .await;
    }
}

async fn notice(events: &mpsc::Sender<BackendEvent>, message: String) {
    let _ = events
        .send(BackendEvent::Public(SessionEvent::Nori(
            nori_protocol::NoriEvent::Notice(nori_protocol::Notice { message }),
        )))
        .await;
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn a_tree_state_sees_edits_to_an_already_changed_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .expect("git runs");
            assert!(out.status.success(), "git {args:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.name", "T"]);
        git(&["config", "user.email", "t@example.com"]);
        std::fs::write(dir.join("a.txt"), "a\n").expect("write");
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);
        assert_eq!(tree_state(dir).map(|state| state.files), Some(Vec::new()));

        std::fs::write(dir.join("a.txt"), "b\n").expect("edit");
        let first = tree_state(dir).expect("state");
        std::fs::write(dir.join("a.txt"), "bigger\n").expect("edit again");
        let second = tree_state(dir).expect("state");
        assert_ne!(first, second, "the same file changed again");
        assert_eq!(first.head, second.head);
    }

    #[test]
    fn the_hook_budget_defaults_when_the_workflow_does_not_say() {
        let mut state = WorkflowState {
            active: Some(Workflow::named("land")),
            ..WorkflowState::default()
        };
        assert_eq!(state.hook_budget(), DEFAULT_HOOK_TURNS);
        state.active = Some(Workflow {
            max_turns: Some(1),
            ..Workflow::named("land")
        });
        assert_eq!(state.hook_budget(), 1);
    }
}
