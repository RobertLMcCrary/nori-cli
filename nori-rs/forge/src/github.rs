//! The GitHub REST calls.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use reqwest::Method;
use reqwest::StatusCode;
use reqwest::header;

use crate::ForgeError;
use crate::ForgeRepo;
use crate::model::CheckRun;
use crate::model::Checks;
use crate::model::NewPull;
use crate::model::PullRequest;
use crate::model::PullState;
use crate::token;

const TIMEOUT: Duration = Duration::from_secs(15);

/// A client for one repository on GitHub.
pub struct Forge {
    repo: ForgeRepo,
    /// The REST API root, such as `https://api.github.com`.
    api: String,
    token: Option<String>,
    client: reqwest::Client,
    /// The last answer to each read and its ETag, to ask again conditionally.
    cache: Mutex<HashMap<String, (String, serde_json::Value)>>,
}

impl std::fmt::Debug for Forge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Forge")
            .field("repo", &self.repo)
            .field("api", &self.api)
            .field("token", &self.token.as_ref().map(|_| "set"))
            .finish_non_exhaustive()
    }
}

impl Forge {
    /// The forge for the repository `remote_url` points at, with the token from the environment
    /// or `gh`'s configuration.
    pub fn for_remote(remote_url: &str) -> Result<Self, ForgeError> {
        let repo = ForgeRepo::from_remote_url(remote_url)
            .ok_or_else(|| ForgeError::UnreadableRemote(remote_url.to_owned()))?;
        let hosts = token::hosts_file();
        let env = |name: &str| std::env::var(name).ok();
        if !token::is_github(&repo.host, env("GH_HOST").as_deref(), hosts.as_deref()) {
            return Err(ForgeError::UnsupportedHost(repo.host));
        }
        let api = if repo.host == token::GITHUB_COM {
            "https://api.github.com".to_owned()
        } else {
            format!("https://{}/api/v3", repo.host)
        };
        let token = token::token_for(&repo.host, env, hosts.as_deref());
        Ok(Self::new(repo, api, token))
    }

    /// A client for `repo` at the API root `api`.
    pub fn new(repo: ForgeRepo, api: impl Into<String>, token: Option<String>) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(concat!("nori-cli/", env!("CARGO_PKG_VERSION")))
            .timeout(TIMEOUT)
            .build()
            .unwrap_or_default();
        Self {
            repo,
            api: api.into().trim_end_matches('/').to_owned(),
            token,
            client,
            cache: Mutex::default(),
        }
    }

    pub fn repo(&self) -> &ForgeRepo {
        &self.repo
    }

    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }

    /// The pull request from `branch`: the open one, else the one updated last. `None` when
    /// there has never been one.
    pub async fn pull_for_branch(&self, branch: &str) -> Result<Option<PullRequest>, ForgeError> {
        let head = format!("{}:{branch}", self.repo.owner);
        let query = [
            ("head", head.as_str()),
            ("state", "all"),
            ("sort", "updated"),
            ("direction", "desc"),
            ("per_page", "10"),
        ];
        let pulls = self
            .get("/pulls", &query)
            .await?
            .ok_or_else(|| self.not_found())?;
        let pulls: Vec<PullRequest> = pulls
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(PullRequest::from_json)
            .collect();
        Ok(pulls
            .iter()
            .find(|pull| pull.state == PullState::Open)
            .or(pulls.first())
            .cloned())
    }

    /// The commit `branch` points at on GitHub; `None` when it was never pushed.
    pub async fn branch_head(&self, branch: &str) -> Result<Option<String>, ForgeError> {
        let path = format!("/git/ref/heads/{}", encode_path(branch));
        Ok(self
            .get(&path, &[])
            .await?
            .and_then(|reference| reference["object"]["sha"].as_str().map(str::to_owned)))
    }

    /// Every check on commit `sha`: its check runs and its commit statuses.
    pub async fn checks(&self, sha: &str) -> Result<Checks, ForgeError> {
        let runs_path = format!("/commits/{sha}/check-runs");
        let statuses_path = format!("/commits/{sha}/status");
        let (runs, statuses) = tokio::join!(
            self.get(&runs_path, &[("per_page", "100")]),
            self.get(&statuses_path, &[("per_page", "100")]),
        );
        let runs = runs?.ok_or_else(|| self.not_found())?;
        let statuses = statuses?.unwrap_or_default();
        let mut all: Vec<CheckRun> = runs["check_runs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(CheckRun::from_check_run)
            .collect();
        all.extend(
            statuses["statuses"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(CheckRun::from_status),
        );
        Ok(Checks {
            sha: sha.to_owned(),
            runs: all,
        })
    }

    /// The whole log of a GitHub Actions job. GitHub only serves logs to a signed-in caller.
    pub async fn job_log(&self, job: i64) -> Result<String, ForgeError> {
        if self.token.is_none() {
            return Err(ForgeError::NoToken("Reading a CI log"));
        }
        let response = self
            .request(Method::GET, &format!("/actions/jobs/{job}/logs"))
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(self.refusal(response).await);
        }
        Ok(response.text().await?)
    }

    /// Open a pull request from an already pushed branch.
    pub async fn open_pull(&self, new: &NewPull) -> Result<PullRequest, ForgeError> {
        if self.token.is_none() {
            return Err(ForgeError::NoToken("Opening a pull request"));
        }
        let body = serde_json::json!({
            "title": new.title,
            "body": new.body,
            "head": new.head,
            "base": new.base,
            "draft": new.draft,
        });
        let response = self
            .request(Method::POST, "/pulls")
            .json(&body)
            .send()
            .await?;
        self.pull_answer(response).await
    }

    /// Replace the title and description of pull request `number`.
    pub async fn update_pull(
        &self,
        number: i64,
        title: &str,
        body: &str,
    ) -> Result<PullRequest, ForgeError> {
        if self.token.is_none() {
            return Err(ForgeError::NoToken("Updating a pull request"));
        }
        let response = self
            .request(Method::PATCH, &format!("/pulls/{number}"))
            .json(&serde_json::json!({ "title": title, "body": body }))
            .send()
            .await?;
        self.pull_answer(response).await
    }

    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        let url = format!(
            "{}/repos/{}/{}{path}",
            self.api, self.repo.owner, self.repo.name
        );
        let request = self
            .client
            .request(method, url)
            .header(header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        match &self.token {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }

    /// A read, conditional on the last answer to it. `None` when GitHub says not found.
    async fn get(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<Option<serde_json::Value>, ForgeError> {
        let mut request = self.request(Method::GET, path).query(query).build()?;
        let key = request.url().to_string();
        let cached = self
            .cache
            .lock()
            .ok()
            .and_then(|cache| cache.get(&key).cloned());
        if let Some((etag, _)) = &cached
            && let Ok(etag) = header::HeaderValue::from_str(etag)
        {
            request.headers_mut().insert(header::IF_NONE_MATCH, etag);
        }
        let response = self.client.execute(request).await?;
        match response.status() {
            StatusCode::NOT_MODIFIED => return Ok(cached.map(|(_, body)| body)),
            StatusCode::NOT_FOUND => return Ok(None),
            status if !status.is_success() => return Err(self.refusal(response).await),
            _ => {}
        }
        let etag = response
            .headers()
            .get(header::ETAG)
            .and_then(|etag| etag.to_str().ok())
            .map(str::to_owned);
        let body: serde_json::Value = response.json().await?;
        if let Some(etag) = etag
            && let Ok(mut cache) = self.cache.lock()
        {
            cache.insert(key, (etag, body.clone()));
        }
        Ok(Some(body))
    }

    async fn pull_answer(&self, response: reqwest::Response) -> Result<PullRequest, ForgeError> {
        if !response.status().is_success() {
            return Err(self.refusal(response).await);
        }
        let body: serde_json::Value = response.json().await?;
        PullRequest::from_json(&body).ok_or_else(|| ForgeError::Status {
            status: 200,
            message: "the answer did not describe a pull request".to_owned(),
        })
    }

    /// The error a refused request stands for, with GitHub's reasons and what would help.
    async fn refusal(&self, response: reqwest::Response) -> ForgeError {
        let status = response.status();
        let spent = response
            .headers()
            .get("x-ratelimit-remaining")
            .is_some_and(|remaining| remaining == "0");
        let body: serde_json::Value = response.json().await.unwrap_or_default();
        let mut message = body["message"]
            .as_str()
            .or(status.canonical_reason())
            .unwrap_or("refused")
            .to_owned();
        // A refused field is explained in `errors`.
        for error in body["errors"].as_array().into_iter().flatten() {
            if let Some(detail) = error["message"].as_str().or(error["code"].as_str()) {
                message.push_str(&format!("; {detail}"));
            }
        }
        if spent {
            message.push_str(match self.token {
                Some(_) => "; the rate limit is spent for now",
                None => "; the anonymous rate limit is spent: set GH_TOKEN",
            });
        } else if self.token.is_none()
            && matches!(
                status,
                StatusCode::NOT_FOUND | StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
            )
        {
            message.push_str("; a private repository needs GH_TOKEN");
        }
        ForgeError::Status {
            status: i64::from(status.as_u16()),
            message,
        }
    }

    fn not_found(&self) -> ForgeError {
        let mut message = format!("{} was not found", self.repo.slug());
        if self.token.is_none() {
            message.push_str("; a private repository needs GH_TOKEN");
        }
        ForgeError::Status {
            status: 404,
            message,
        }
    }
}

/// `path` with every byte but unreserved characters and `/` percent-encoded.
fn encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
#[path = "github_tests.rs"]
mod tests;
