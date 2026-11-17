//! The workflow's CI hooks. After a turn in which the agent pushed its branch, Nori watches the
//! checks on the pushed commit. Once they decide, it sends the workflow's `on-ci-fail` prompt
//! with what failed and the end of each failed job's log, or its `on-ci-pass` prompt, as a hidden
//! turn when the session is idle and the turn budget allows.

use std::time::Duration;
use std::time::Instant;

use nori_forge::CheckState;
use nori_forge::Checks;
use nori_forge::RunState;
use nori_workflow::HookEvent;
use nori_workflow::Workflow;

use super::*;
use workflow::notice;

/// The waits between reads of a commit's checks; the last repeats.
const POLLS: [Duration; 4] = [
    Duration::from_secs(10),
    Duration::from_secs(15),
    Duration::from_secs(30),
    Duration::from_secs(60),
];
/// Checks still undecided this long after the push are left alone.
const GIVE_UP: Duration = Duration::from_secs(45 * 60);
/// A commit with no checks this long after its push has no CI.
const NO_CI: Duration = Duration::from_secs(3 * 60);
/// Reads in a row that may fail before the watch stops.
const MAX_ERRORS: i64 = 5;
const LOG_LINES: i64 = 30;
const LOGS_READ: usize = 3;

/// What CI decided about a pushed commit, ready to go to the agent.
pub(crate) struct CiReport {
    generation: i64,
    event: HookEvent,
    /// One line for the user.
    summary: String,
    /// The detail for the agent.
    text: String,
}

/// Whether `workflow` has a prompt for CI's verdict, so pushes are worth watching.
pub(super) fn hooks_ci(workflow: &Workflow) -> bool {
    workflow
        .hooks
        .iter()
        .any(|hook| matches!(hook.event, HookEvent::CiFailed | HookEvent::CiPassed))
}

impl AcpBackend {
    /// The commit the checked-out branch's remote copy is at, as this machine last pushed or
    /// fetched it.
    pub(super) async fn pushed_commit(&self) -> Option<String> {
        let cwd = self.cwd.clone();
        tokio::task::spawn_blocking(move || {
            let repo = nori_config::GitRepo::discover(&cwd)?;
            repo.tracking_commit(&repo.push_remote(), &repo.branch()?)
        })
        .await
        .ok()
        .flatten()
    }

    /// After a turn: when the agent pushed during the user's turn and the workflow has CI
    /// hooks, start watching the pushed commit. A newer push replaces the watch.
    pub(super) async fn maybe_watch_ci(&self, generation: i64) {
        if generation != self.orchestrator.admission_generation() {
            return;
        }
        let before = {
            let state = self.workflow.lock().await;
            match &state.active {
                Some(workflow) if hooks_ci(workflow) => state.pushed_before.clone(),
                Some(_) | None => return,
            }
        };
        // Without a record of where the branch was, nothing is known to have been pushed.
        let (Some(before), Some(pushed)) = (before, self.pushed_commit().await) else {
            return;
        };
        if generation != self.orchestrator.admission_generation() {
            return;
        }
        {
            let mut state = self.workflow.lock().await;
            if pushed == before || state.ci_watching.as_ref() == Some(&pushed) {
                return;
            }
            state.ci_watching = Some(pushed.clone());
        }
        let backend = self.clone();
        tokio::spawn(async move { backend.watch_ci(pushed, generation).await });
    }

    async fn watch_ci(&self, sha: String, generation: i64) {
        let place = match forge_tools::place(self.cwd.clone(), None).await {
            Ok(place) => place,
            Err(error) => {
                notice(&self.backend_event_tx, format!("CI watch: {error}")).await;
                return;
            }
        };
        let started = Instant::now();
        let mut waits = POLLS.into_iter().chain(std::iter::repeat(POLLS[3]));
        let mut errors = 0;
        loop {
            tokio::time::sleep(waits.next().unwrap_or(POLLS[3])).await;
            let superseded = self.workflow.lock().await.ci_watching.as_ref() != Some(&sha);
            if superseded || self.is_shutting_down.load(Ordering::Relaxed) {
                return;
            }
            let checks = match place.forge.checks(&sha).await {
                Ok(checks) => checks,
                Err(error) => {
                    errors += 1;
                    if errors >= MAX_ERRORS {
                        notice(&self.backend_event_tx, format!("CI watch stopped: {error}")).await;
                        return;
                    }
                    continue;
                }
            };
            errors = 0;
            let event = match checks.state() {
                // A failure decides, even while other checks run.
                CheckState::Failing => HookEvent::CiFailed,
                CheckState::Passing => HookEvent::CiPassed,
                CheckState::Cancelled => return,
                CheckState::None if started.elapsed() > NO_CI => return,
                CheckState::None | CheckState::Pending if started.elapsed() > GIVE_UP => return,
                CheckState::None | CheckState::Pending => continue,
            };
            let report = report(&place, &checks, event, generation).await;
            {
                let _admission = self.orchestrator.rewind_admission().await;
                if generation != self.orchestrator.admission_generation() {
                    return;
                }
                self.workflow.lock().await.ci_owed = Some(report);
            }
            self.deliver_ci_if_idle().await;
            return;
        }
    }

    /// Hand the agent what CI said, if the session is idle. A busy session gets it when its
    /// turn ends. Past the workflow's turn budget, or without a prompt for the verdict, only
    /// the user is told.
    pub(super) async fn deliver_ci_if_idle(&self) {
        if self.session_driver.lock().await.prompt_is_busy() {
            return;
        }
        let (workflow, report, prompt) = {
            let mut state = self.workflow.lock().await;
            let Some(report) = state.ci_owed.take() else {
                return;
            };
            if report.generation != self.orchestrator.admission_generation() {
                return;
            }
            let Some(workflow) = state.active.clone() else {
                return;
            };
            let prompt = workflow.hook_prompts(report.event).collect::<Vec<_>>();
            let prompt = (!prompt.is_empty() && state.hook_turns < state.hook_budget())
                .then(|| prompt.join("\n"));
            if prompt.is_some() {
                state.hook_turns += 1;
            }
            (workflow, report, prompt)
        };
        let label = format!(
            "Workflow \"{}\" · {} · {}",
            workflow.name,
            report.event.form(),
            report.summary
        );
        match prompt {
            Some(prompt) => {
                self.send_hook_turn(
                    report.generation,
                    label,
                    report.event,
                    format!("{}\n{prompt}", report.text),
                )
                .await;
            }
            None => notice(&self.backend_event_tx, report.summary).await,
        }
    }
}

/// The report of `checks` on the pushed commit: each failed check with the end of its log, or
/// how many passed.
async fn report(
    place: &forge_tools::Place,
    checks: &Checks,
    event: HookEvent,
    generation: i64,
) -> CiReport {
    let short = checks.sha.get(..7).unwrap_or(&checks.sha);
    let failed: Vec<_> = checks.in_states(&[RunState::Failed]).collect();
    if event == HookEvent::CiPassed || failed.is_empty() {
        let passed = checks.in_states(&[RunState::Passed]).count();
        let summary = format!("CI passed on {short}");
        return CiReport {
            generation,
            event,
            text: format!("{summary} (`{}`): {passed} checks passed.\n", place.branch),
            summary,
        };
    }
    let names: Vec<&str> = failed.iter().map(|run| run.name.as_str()).collect();
    let summary = format!("CI failed on {short}: {}", names.join(", "));
    let logs = failed
        .iter()
        .filter_map(|run| run.job)
        .take(LOGS_READ)
        .map(|job| async move { (job, place.forge.job_log(job).await.ok()) });
    let logs: std::collections::HashMap<i64, Option<String>> =
        futures::future::join_all(logs).await.into_iter().collect();
    let mut text = format!("CI failed on {short} (`{}`):\n", place.branch);
    for run in &failed {
        text.push_str(&format!("- {}", run.name));
        if let Some(url) = &run.url {
            text.push_str(&format!(" ({url})"));
        }
        text.push('\n');
        if let Some(Some(log)) = run.job.and_then(|job| logs.get(&job)) {
            text.push_str(&format!(
                "```\n{}```\n",
                nori_forge::log_excerpt(log, LOG_LINES)
            ));
        }
    }
    CiReport {
        generation,
        event,
        summary,
        text,
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn only_a_workflow_with_a_ci_prompt_watches_pushes() {
        let hook = |event| nori_workflow::Hook {
            event,
            prompt: "p".to_owned(),
        };
        let mut workflow = Workflow::named("land");
        workflow.hooks.push(hook(HookEvent::AfterChange));
        assert_eq!(hooks_ci(&workflow), false);
        workflow.hooks.push(hook(HookEvent::CiFailed));
        assert_eq!(hooks_ci(&workflow), true);
    }

    #[tokio::test]
    async fn a_failure_report_names_each_failed_check_with_its_log() {
        use wiremock::Mock;
        use wiremock::MockServer;
        use wiremock::ResponseTemplate;
        use wiremock::matchers::path;

        let server = MockServer::start().await;
        Mock::given(path("/repos/o/r/actions/jobs/7/logs"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "2026-11-16T10:00:00.0000001Z error[E0308]: mismatched types\n2026-11-16T10:00:01.0000001Z ##[error]Process completed with exit code 101.\n",
            ))
            .mount(&server)
            .await;
        let repo = nori_forge::ForgeRepo {
            host: "github.com".to_owned(),
            owner: "o".to_owned(),
            name: "r".to_owned(),
        };
        let place = forge_tools::Place {
            forge: std::sync::Arc::new(nori_forge::Forge::new(
                repo,
                server.uri(),
                Some("t".to_owned()),
            )),
            branch: "topic".to_owned(),
            head: None,
            remote: "origin".to_owned(),
            default_branch: None,
            cwd: std::path::PathBuf::from("/tmp"),
        };
        let run = |name: &str, state, job| nori_forge::CheckRun {
            name: name.to_owned(),
            state,
            url: Some(format!("https://ci/{name}")),
            job,
        };
        let checks = Checks {
            sha: "0123456789".to_owned(),
            runs: vec![
                run("test", RunState::Failed, Some(7)),
                run("lint", RunState::Failed, None),
                run("build", RunState::Passed, Some(8)),
            ],
        };
        let report = report(&place, &checks, HookEvent::CiFailed, 0).await;
        assert_eq!(report.summary, "CI failed on 0123456: test, lint");
        assert_eq!(
            report.text,
            "CI failed on 0123456 (`topic`):\n- test (https://ci/test)\n```\nerror[E0308]: mismatched types\nerror: Process completed with exit code 101.\n```\n- lint (https://ci/lint)\n"
        );
    }
}
