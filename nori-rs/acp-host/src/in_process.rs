//! Agents that run inside Nori's own process.
//!
//! An ACP agent normally is a program Nori starts and talks to over its stdin and stdout. A
//! built-in one is a library instead: Nori gives it one end of an in-memory pipe, keeps the other
//! end for its own ACP client, and no process is started. The agents themselves live in crates
//! `acp-host` does not depend on; the application registers a launcher for each by name at
//! startup, and a distribution of `native = { agent = "..." }` selects it.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::RwLock;

use tokio::io::DuplexStream;

/// What an in-process agent is started with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InProcessSpec {
    /// The built-in agent's name, e.g. `codex`.
    pub agent: String,
    /// Where this agent's state and login live. Per agent, because a process-wide environment
    /// variable cannot tell two accounts apart.
    pub home: Option<PathBuf>,
    /// `key=value` overrides, as `codex -c` takes them.
    pub config: Vec<String>,
}

/// Runs an agent on `pipe` until the connection ends.
pub type Launcher = Arc<
    dyn Fn(InProcessSpec, DuplexStream) -> Pin<Box<dyn Future<Output = std::io::Result<()>> + Send>>
        + Send
        + Sync,
>;

fn launchers() -> &'static RwLock<HashMap<String, Launcher>> {
    static LAUNCHERS: OnceLock<RwLock<HashMap<String, Launcher>>> = OnceLock::new();
    LAUNCHERS.get_or_init(Default::default)
}

/// Make the built-in agent `name` available to `native` distributions.
pub fn register_in_process_agent(name: &str, launcher: Launcher) {
    launchers()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(name.to_owned(), launcher);
}

/// The launcher registered for `name`, if this build has one.
pub fn in_process_launcher(name: &str) -> Option<Launcher> {
    launchers()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(name)
        .cloned()
}

/// Whether this build can run the built-in agent `name`.
pub fn in_process_available(name: &str) -> bool {
    in_process_launcher(name).is_some()
}

/// The `CODEX_CONFIG` environment variable an adapter would read, as the `key=value` overrides an
/// in-process Codex takes: nested objects become dotted keys (`{"features":{"goals":false}}` is
/// `features.goals=false`).
pub fn codex_overrides_from_env(env: &HashMap<String, String>) -> Vec<String> {
    let Some(json) = env
        .get("CODEX_CONFIG")
        .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
    else {
        return Vec::new();
    };
    let mut overrides = Vec::new();
    flatten(&mut overrides, String::new(), &json);
    overrides
}

/// The environment an adapter would read for Claude Code, as the `key=value` options the
/// in-process driver takes: which `claude` to run (how an account is chosen) and the model.
pub fn claude_overrides_from_env(env: &HashMap<String, String>) -> Vec<String> {
    let mut overrides = Vec::new();
    if let Some(path) = env.get("CLAUDE_CODE_EXECUTABLE").filter(|v| !v.is_empty()) {
        overrides.push(format!("cli_path={path}"));
    }
    if let Some(model) = env.get("ANTHROPIC_MODEL").filter(|v| !v.is_empty()) {
        overrides.push(format!("model={model}"));
    }
    overrides
}

fn flatten(out: &mut Vec<String>, prefix: String, value: &serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                let key = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten(out, key, value);
            }
        }
        // A JSON string, number or boolean is also a valid TOML one, which is what `-c` parses.
        scalar if !prefix.is_empty() => out.push(format!("{prefix}={scalar}")),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_config_json_becomes_dotted_overrides() {
        let env = HashMap::from([(
            "CODEX_CONFIG".to_owned(),
            r#"{"model":"o3","features":{"goals":false},"model_reasoning_effort":"high"}"#
                .to_owned(),
        )]);
        let mut overrides = codex_overrides_from_env(&env);
        overrides.sort();
        assert_eq!(
            overrides,
            vec![
                "features.goals=false",
                "model=\"o3\"",
                "model_reasoning_effort=\"high\"",
            ]
        );
        assert!(codex_overrides_from_env(&HashMap::new()).is_empty());
    }

    #[test]
    fn claude_environment_becomes_driver_options() {
        let env = HashMap::from([
            (
                "CLAUDE_CODE_EXECUTABLE".to_owned(),
                "/bin/claude2".to_owned(),
            ),
            ("ANTHROPIC_MODEL".to_owned(), "opus".to_owned()),
        ]);
        assert_eq!(
            claude_overrides_from_env(&env),
            vec!["cli_path=/bin/claude2", "model=opus"]
        );
        assert!(claude_overrides_from_env(&HashMap::new()).is_empty());
    }

    #[test]
    fn a_registered_agent_is_found_by_name_and_others_are_not() {
        register_in_process_agent(
            "echo-test",
            Arc::new(|_spec, _pipe| Box::pin(async { Ok(()) })),
        );
        assert!(in_process_available("echo-test"));
        assert!(!in_process_available("not-registered"));
    }
}
