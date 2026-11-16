use pretty_assertions::assert_eq;
use serde_json::json;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::body_json;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;
use wiremock::matchers::query_param;

use super::*;
use crate::model::RunState;

fn forge(server: &MockServer, token: Option<&str>) -> Forge {
    let repo = ForgeRepo {
        host: "github.com".to_owned(),
        owner: "o".to_owned(),
        name: "r".to_owned(),
    };
    Forge::new(repo, server.uri(), token.map(str::to_owned))
}

fn pull_json(number: i64, state: &str) -> serde_json::Value {
    json!({"number": number, "title": format!("pull {number}"), "html_url": format!("https://github.com/o/r/pull/{number}"),
        "state": state, "merged_at": null, "draft": false,
        "base": {"ref": "main"}, "head": {"ref": "topic/x", "sha": "abc"}})
}

#[tokio::test]
async fn the_open_pull_request_of_a_branch_wins_over_older_ones() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/pulls"))
        .and(query_param("head", "o:topic/x"))
        .and(query_param("state", "all"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!([pull_json(4, "closed"), pull_json(9, "open")])),
        )
        .mount(&server)
        .await;
    let pull = forge(&server, None)
        .pull_for_branch("topic/x")
        .await
        .expect("answers");
    assert_eq!(
        pull.map(|pull| (pull.number, pull.state)),
        Some((9, PullState::Open))
    );
}

#[tokio::test]
async fn checks_join_runs_and_statuses_and_ask_again_conditionally() {
    let server = MockServer::start().await;
    // Registered first, so a later unconditional mock does not shadow it.
    Mock::given(method("GET"))
        .and(path("/repos/o/r/commits/abc/check-runs"))
        .and(header("if-none-match", "\"v1\""))
        .respond_with(ResponseTemplate::new(304))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/commits/abc/check-runs"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"v1\"")
                .set_body_json(json!({"total_count": 1, "check_runs": [
                    {"id": 7, "name": "test", "status": "completed", "conclusion": "failure",
                     "html_url": "https://github.com/o/r/runs/7", "app": {"slug": "github-actions"}}
                ]})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/commits/abc/status"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"statuses": [
                {"context": "deploy", "state": "pending", "target_url": "https://ci.example/1"}
            ]})),
        )
        .mount(&server)
        .await;
    let forge = forge(&server, Some("t"));
    let first = forge.checks("abc").await.expect("answers");
    assert_eq!(
        first
            .runs
            .iter()
            .map(|run| (run.name.as_str(), run.state, run.job))
            .collect::<Vec<_>>(),
        vec![
            ("test", RunState::Failed, Some(7)),
            ("deploy", RunState::Pending, None)
        ]
    );
    let again = forge.checks("abc").await.expect("answers from the cache");
    assert_eq!(again, first);
}

#[tokio::test]
async fn a_private_repository_without_a_token_says_what_would_help() {
    let server = MockServer::start().await;
    let error = forge(&server, None)
        .pull_for_branch("x")
        .await
        .expect_err("nothing is mounted");
    assert_eq!(
        error.to_string(),
        "GitHub answered 404: o/r was not found; a private repository needs GH_TOKEN"
    );
    let error = forge(&server, None)
        .open_pull(&NewPull {
            title: "t".to_owned(),
            body: String::new(),
            head: "x".to_owned(),
            base: "main".to_owned(),
            draft: false,
        })
        .await
        .expect_err("needs a token");
    assert_eq!(
        error.to_string(),
        "Opening a pull request needs a GitHub token: set GH_TOKEN"
    );
}

#[tokio::test]
async fn opening_a_pull_request_reports_github_reasons() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/o/r/pulls"))
        .and(header("authorization", "Bearer t"))
        .and(body_json(
            json!({"title": "feat: x", "body": "Why", "head": "topic/x",
            "base": "main", "draft": true}),
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(pull_json(12, "open")))
        .mount(&server)
        .await;
    let forge = forge(&server, Some("t"));
    let new = NewPull {
        title: "feat: x".to_owned(),
        body: "Why".to_owned(),
        head: "topic/x".to_owned(),
        base: "main".to_owned(),
        draft: true,
    };
    assert_eq!(forge.open_pull(&new).await.expect("opened").number, 12);

    Mock::given(method("PATCH"))
        .and(path("/repos/o/r/pulls/12"))
        .respond_with(ResponseTemplate::new(422).set_body_json(json!({
            "message": "Validation Failed",
            "errors": [{"resource": "PullRequest", "code": "custom", "message": "title is too long"}]
        })))
        .mount(&server)
        .await;
    let error = forge.update_pull(12, "t", "b").await.expect_err("refused");
    assert_eq!(
        error.to_string(),
        "GitHub answered 422: Validation Failed; title is too long"
    );
}

#[tokio::test]
async fn a_branch_head_is_read_from_its_ref_and_missing_when_unpushed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/git/ref/heads/topic/a%23b"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"object": {"sha": "def"}})))
        .mount(&server)
        .await;
    let forge = forge(&server, None);
    assert_eq!(
        forge.branch_head("topic/a#b").await.expect("answers"),
        Some("def".to_owned())
    );
    assert_eq!(forge.branch_head("never").await.expect("answers"), None);
}

#[tokio::test]
async fn a_job_log_follows_the_redirect_to_the_text() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/actions/jobs/7/logs"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", format!("{}/blob/7.txt", server.uri())),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/blob/7.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_string("line\n"))
        .mount(&server)
        .await;
    assert_eq!(
        forge(&server, Some("t")).job_log(7).await.expect("read"),
        "line\n"
    );
}
