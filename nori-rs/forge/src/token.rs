//! Which hosts are GitHub, and the token to use with each.

use std::path::PathBuf;

pub(crate) const GITHUB_COM: &str = "github.com";

/// `gh`'s `hosts.yml`, when it exists.
pub(crate) fn hosts_file() -> Option<String> {
    let dir = std::env::var_os("GH_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(|dir| PathBuf::from(dir).join("gh")))
        .or_else(|| dirs::home_dir().map(|home| home.join(".config").join("gh")))?;
    std::fs::read_to_string(dir.join("hosts.yml")).ok()
}

/// Whether `host` is GitHub: github.com, the host `GH_HOST` names, or one `gh` is logged in to.
pub(crate) fn is_github(host: &str, gh_host: Option<&str>, hosts_file: Option<&str>) -> bool {
    host == GITHUB_COM
        || gh_host.is_some_and(|gh_host| gh_host.eq_ignore_ascii_case(host))
        || hosts_file.is_some_and(|text| host_block(text, host).is_some())
}

/// The token for `host`, from the environment (`env` reads a variable), else from `gh`'s hosts
/// file.
pub(crate) fn token_for(
    host: &str,
    env: impl Fn(&str) -> Option<String>,
    hosts_file: Option<&str>,
) -> Option<String> {
    let names: [&str; 2] = if host == GITHUB_COM {
        ["GH_TOKEN", "GITHUB_TOKEN"]
    } else {
        ["GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN"]
    };
    names
        .into_iter()
        .filter_map(&env)
        .map(|value| value.trim().to_owned())
        .find(|value| !value.is_empty())
        .or_else(|| {
            let block = host_block(hosts_file?, host)?;
            // Only the host's own `oauth_token`, not one nested under `users:`.
            let indent = block.first()?.len() - block.first()?.trim_start().len();
            block
                .iter()
                .filter(|line| line.len() - line.trim_start().len() == indent)
                .find_map(|line| line.trim_start().strip_prefix("oauth_token:"))
                .map(|token| token.trim().trim_matches(['"', '\'']).to_owned())
                .filter(|token| !token.is_empty())
        })
}

/// The indented lines under `host:` in a `hosts.yml`.
fn host_block<'a>(text: &'a str, host: &str) -> Option<Vec<&'a str>> {
    let mut lines = text.lines();
    lines.find(|line| {
        line.strip_suffix(':')
            .map(|key| key.trim_matches(['"', '\'']))
            .is_some_and(|key| key.eq_ignore_ascii_case(host))
    })?;
    Some(
        lines
            .take_while(|line| line.is_empty() || line.starts_with([' ', '\t']))
            .filter(|line| !line.trim().is_empty())
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    const HOSTS: &str = "github.com:\n    users:\n        someone:\n            oauth_token: nested\n    git_protocol: https\n    oauth_token: gho_top\n    user: someone\nghe.corp.example:\n    user: someone\n";

    #[test]
    fn the_environment_comes_before_the_hosts_file() {
        let env = |name: &str| (name == "GITHUB_TOKEN").then(|| " ghp_env \n".to_owned());
        assert_eq!(
            token_for(GITHUB_COM, env, Some(HOSTS)),
            Some("ghp_env".to_owned())
        );
        assert_eq!(
            token_for(GITHUB_COM, |_| None, Some(HOSTS)),
            Some("gho_top".to_owned())
        );
        // An Enterprise host reads its own variables, and gh keeps this one's token elsewhere.
        assert_eq!(token_for("ghe.corp.example", env, Some(HOSTS)), None);
        assert_eq!(token_for(GITHUB_COM, |_| None, None), None);
    }

    #[test]
    fn github_hosts_are_github_com_gh_host_and_gh_logins() {
        assert!(is_github(GITHUB_COM, None, None));
        assert!(is_github("ghe.corp.example", None, Some(HOSTS)));
        assert!(is_github("git.example", Some("GIT.example"), None));
        assert!(!is_github("gitlab.com", None, Some(HOSTS)));
    }
}
