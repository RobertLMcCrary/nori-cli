//! A small client for the forge a repository lives on: its pull requests and CI checks.
//!
//! Only GitHub is supported: github.com, and a GitHub Enterprise host that `GH_HOST` names or
//! `gh` is logged in to. Requests go to the REST API over HTTPS; nothing shells out to `gh`.
//!
//! The token comes from `GH_TOKEN` or `GITHUB_TOKEN` (`GH_ENTERPRISE_TOKEN` or
//! `GITHUB_ENTERPRISE_TOKEN` for an Enterprise host), else from `gh`'s `hosts.yml` when `gh`
//! keeps it there rather than in the system keychain. Without a token, public repositories still
//! answer reads, within GitHub's anonymous rate limit of 60 requests an hour. Reads are
//! conditional (`If-None-Match`), so polling an unchanged answer costs no rate limit.

mod github;
mod log;
mod model;
mod token;

pub use github::Forge;
pub use log::log_excerpt;
pub use model::CheckRun;
pub use model::CheckState;
pub use model::Checks;
pub use model::NewPull;
pub use model::PullRequest;
pub use model::PullState;
pub use model::RunState;

/// What went wrong talking to the forge, worded for the agent or user who reads it.
#[derive(Debug, thiserror::Error)]
pub enum ForgeError {
    #[error("{0} is not a GitHub host Nori knows: set GH_HOST to it if it is GitHub Enterprise")]
    UnsupportedHost(String),
    #[error("`{0}` is not a remote URL Nori can read")]
    UnreadableRemote(String),
    #[error("{0} needs a GitHub token: set GH_TOKEN")]
    NoToken(&'static str),
    #[error("GitHub could not be reached: {0}")]
    Http(#[from] reqwest::Error),
    #[error("GitHub answered {status}: {message}")]
    Status { status: i64, message: String },
}

/// A repository on a forge: `host/owner/name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgeRepo {
    pub host: String,
    pub owner: String,
    pub name: String,
}

impl ForgeRepo {
    /// The repository a remote URL points at: `https://host/owner/name`, `git@host:owner/name`
    /// or `ssh://git@host:port/owner/name`, each with or without `.git`.
    pub fn from_remote_url(url: &str) -> Option<Self> {
        let url = url.trim();
        let (host, path) = match url.split_once("://") {
            Some((scheme, rest)) => {
                if !matches!(
                    scheme,
                    "https" | "http" | "ssh" | "git" | "git+ssh" | "ssh+git"
                ) {
                    return None;
                }
                let (authority, path) = rest.split_once('/')?;
                let host = authority.rsplit('@').next()?;
                (host.split(':').next()?, path)
            }
            // scp-like `user@host:owner/name`; a local path has a slash before any colon.
            None => {
                let (authority, path) = url.split_once(':')?;
                if authority.contains('/') {
                    return None;
                }
                (authority.rsplit('@').next()?, path)
            }
        };
        let path = path.trim_matches('/');
        let path = path.strip_suffix(".git").unwrap_or(path);
        let (owner, name) = path.split_once('/')?;
        if host.is_empty() || owner.is_empty() || name.is_empty() || name.contains('/') {
            return None;
        }
        Some(Self {
            host: host.to_ascii_lowercase(),
            owner: owner.to_owned(),
            name: name.to_owned(),
        })
    }

    /// `owner/name`.
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn repo(host: &str, owner: &str, name: &str) -> Option<ForgeRepo> {
        Some(ForgeRepo {
            host: host.to_owned(),
            owner: owner.to_owned(),
            name: name.to_owned(),
        })
    }

    #[test]
    fn remote_urls_name_their_repository() {
        let nori = repo("github.com", "tilework-tech", "nori-cli");
        for url in [
            "https://github.com/tilework-tech/nori-cli.git",
            "https://github.com/tilework-tech/nori-cli",
            "https://someone@github.com/tilework-tech/nori-cli/",
            "git@github.com:tilework-tech/nori-cli.git",
            "ssh://git@github.com/tilework-tech/nori-cli",
            "ssh://git@GitHub.com:22/tilework-tech/nori-cli.git",
        ] {
            assert_eq!(ForgeRepo::from_remote_url(url), nori, "{url}");
        }
        assert_eq!(
            ForgeRepo::from_remote_url("git@ghe.corp.example:team/app.git"),
            repo("ghe.corp.example", "team", "app")
        );
    }

    #[test]
    fn paths_and_odd_urls_are_not_repositories() {
        for url in [
            "/srv/git/app.git",
            "./relative:odd/path",
            "file:///srv/git/owner/app.git",
            "https://gitlab.com/group/sub/app.git",
            "https://github.com/only-owner",
            "",
        ] {
            assert_eq!(ForgeRepo::from_remote_url(url), None, "{url}");
        }
    }
}
