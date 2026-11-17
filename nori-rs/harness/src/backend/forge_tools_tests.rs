use std::sync::Arc;

use nori_workflow::Hook;
use nori_workflow::HookEvent;
use nori_workflow::PrRules;
use nori_workflow::Workflow;
use pretty_assertions::assert_eq;
use tokio::sync::Mutex;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

use super::*;
use crate::backend::orchestrator::Orchestrator;
use crate::backend::orchestrator::Seed;
use crate::backend::workflow::WorkflowState;

#[test]
fn sections_count_as_headings_in_any_markdown_spelling() {
    let sections = ["Why".to_owned(), "What".to_owned(), "Testing".to_owned()];
    let body = "## Why\nBecause.\n\n**What**\nThis.\n\nThe testing was light.\n";
    assert_eq!(missing_sections(body, &sections), vec!["Testing"]);
    assert_eq!(
        missing_sections("why:\n\n### WHAT\n\nTesting\n", &sections),
        Vec::<&str>::new()
    );
}

fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// A repository on `topic`, one commit of three lines past `origin/main`, whose `origin` is
/// served by `server`.
async fn repository(server: &MockServer, remote: &str) -> (tempfile::TempDir, String) {
    let temp = tempfile::tempdir().expect("tempdir");
    let dir = temp.path();
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.name", "T"]);
    git(dir, &["config", "user.email", "t@example.com"]);
    std::fs::write(dir.join("a.txt"), "a\n").expect("write");
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "base"]);
    git(dir, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    git(dir, &["checkout", "-q", "-b", "topic"]);
    std::fs::write(dir.join("b.txt"), "1\n2\n3\n").expect("write");
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "three lines"]);
    git(dir, &["remote", "add", "origin", remote]);
    let head = git(dir, &["rev-parse", "HEAD"]);
    let repo = nori_forge::ForgeRepo::from_remote_url(remote).expect("a GitHub URL");
    FORGES.lock().expect("forges").insert(
        remote.to_owned(),
        Arc::new(Forge::new(repo, server.uri(), Some("t".to_owned()))),
    );
    (temp, head)
}

fn orchestrator() -> Orchestrator {
    let (events, _events_rx) = tokio::sync::mpsc::channel(8);
    Orchestrator::new(
        Seed::root(
            Arc::new(crate::config::NoriConfig::default()),
            "test".to_owned(),
        ),
        events,
    )
    .0
}

fn request(body: &str) -> PullRequestRequest {
    PullRequestRequest {
        title: "feat: three lines".to_owned(),
        body: body.to_owned(),
        base: None,
        draft: None,
    }
}

#[tokio::test]
async fn a_pull_request_waits_for_the_push_the_hook_and_the_rules() {
    let server = MockServer::start().await;
    let remote = "https://github.com/o/pull-request-flow.git";
    let (temp, head) = repository(&server, remote).await;
    let mut workflow = Workflow::named("land");
    workflow.hooks.push(Hook {
        event: HookEvent::BeforePr,
        prompt: "List every user-visible change.".to_owned(),
    });
    workflow.pr = Some(PrRules {
        sections: vec!["Why".to_owned()],
        max_lines: Some(2),
        base: Some("main".to_owned()),
        upstream: Some(Upstream::TestsPass),
        ..PrRules::default()
    });
    let state = Arc::new(Mutex::new(WorkflowState::default()));
    state.lock().await.active = Some(workflow);
    let workspace = Workspace {
        cwd: temp.path().to_path_buf(),
        workflow: Arc::clone(&state),
    };
    let orchestrator = orchestrator();
    let open = |body: &'static str| pull_request(workspace.clone(), request(body), &orchestrator);

    // Not pushed yet.
    let error = open("## Why\nx").await.expect_err("unpushed");
    assert_eq!(
        error,
        "`topic` is not on GitHub yet: push it first (`git push -u origin topic`)."
    );

    Mock::given(method("GET"))
        .and(path("/repos/o/pull-request-flow/git/ref/heads/topic"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"object": {"sha": head}})),
        )
        .mount(&server)
        .await;
    // The workflow's word comes first, once for this commit.
    assert_eq!(
        open("x").await.expect("held"),
        serde_json::json!({
            "opened": false,
            "workflow": "land",
            "before_opening": "List every user-visible change.",
            "then": "Do this first, then call pull_request again.",
        })
    );
    let error = open("x").await.expect_err("rules broken");
    assert_eq!(
        error,
        "The \"land\" workflow's pull request rules are not met:\n\
         - The description has no \"Why\" section.\n\
         - It changes 3 lines; at most 2 are allowed: split the work into a series of pull requests.\n\
         Fix this, then call pull_request again."
    );

    if let Some(workflow) = state.lock().await.active.as_mut() {
        workflow.pr.as_mut().expect("rules").max_lines = Some(10);
    }
    Mock::given(method("GET"))
        .and(path("/repos/o/pull-request-flow/pulls"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/o/pull-request-flow/pulls"))
        .and(wiremock::matchers::body_json(serde_json::json!({
            "title": "feat: three lines", "body": "## Why\nBecause.", "head": "topic",
            "base": "main", "draft": false,
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "number": 7, "title": "feat: three lines",
            "html_url": "https://github.com/o/pull-request-flow/pull/7", "state": "open",
            "merged_at": null, "draft": false, "base": {"ref": "main"},
            "head": {"ref": "topic", "sha": head},
        })))
        .expect(1)
        .mount(&server)
        .await;
    let opened = open("## Why\nBecause.").await.expect("opened");
    assert_eq!(opened["created"], serde_json::json!(true));
    assert_eq!(opened["pull_request"]["number"], serde_json::json!(7));
}

#[tokio::test]
async fn a_workflow_that_never_upstreams_opens_nothing() {
    let mut workflow = Workflow::named("explore");
    workflow.pr = Some(PrRules {
        upstream: Some(Upstream::Never),
        ..PrRules::default()
    });
    let state = Arc::new(Mutex::new(WorkflowState::default()));
    state.lock().await.active = Some(workflow);
    let workspace = Workspace {
        cwd: std::env::temp_dir(),
        workflow: state,
    };
    assert_eq!(
        pull_request(workspace, request("b"), &orchestrator()).await,
        Err(
            "The \"explore\" workflow never upstreams: leave pushing and pull requests to the user."
                .to_owned()
        )
    );
}
