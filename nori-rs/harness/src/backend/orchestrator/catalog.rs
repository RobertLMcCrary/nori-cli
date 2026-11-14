//! The models and settings each agent offers, remembered across runs, so `agent_capabilities`
//! lists them for agents that have not run in this process.
//!
//! Every session that opens records what its agent advertised. The catalog is kept in memory
//! and in `<nori home>/agent-models.json`, which is read once per process and rewritten in the
//! background, never on the way to a frame.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::PoisonError;

use nori_protocol::acp::v1 as acp;

use super::models::Model;
use super::models::Models;

const FILE: &str = "agent-models.json";

/// What one agent offered when it last ran.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) struct Offer {
    pub(super) models: Vec<Model>,
    /// Its other select settings, such as reasoning effort or mode.
    #[serde(default)]
    pub(super) settings: Vec<Setting>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) struct Setting {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) values: Vec<Model>,
}

#[derive(Default)]
struct Catalog {
    /// The Nori home `agents` was read from.
    home: Option<PathBuf>,
    agents: BTreeMap<String, Offer>,
}

static CATALOG: LazyLock<Mutex<Catalog>> = LazyLock::new(Default::default);

fn with_catalog<T>(home: &Path, read: impl FnOnce(&mut BTreeMap<String, Offer>) -> T) -> T {
    let mut catalog = CATALOG.lock().unwrap_or_else(PoisonError::into_inner);
    if catalog.home.as_deref() != Some(home) {
        catalog.agents = std::fs::read(home.join(FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        catalog.home = Some(home.to_owned());
    }
    read(&mut catalog.agents)
}

/// Remember what `agent` offers, from a session's advertised options.
pub(crate) fn remember(home: &Path, agent: &str, options: &[acp::SessionConfigOption]) {
    let offer = Offer {
        models: Models::of(options)
            .map(|models| models.offered)
            .unwrap_or_default(),
        settings: Models::settings(options)
            .into_iter()
            .map(|setting| Setting {
                id: setting.config_id,
                name: setting.name,
                values: setting.offered,
            })
            .collect(),
    };
    if offer == Offer::default() {
        return;
    }
    let changed = with_catalog(home, |agents| {
        if agents.get(agent) == Some(&offer) {
            return None;
        }
        agents.insert(agent.to_owned(), offer);
        Some(agents.clone())
    });
    let Some(agents) = changed else {
        return;
    };
    let path = home.join(FILE);
    let write = move || {
        let Ok(json) = serde_json::to_vec_pretty(&agents) else {
            return;
        };
        let temporary = path.with_extension("json.tmp");
        if std::fs::write(&temporary, json).is_ok() {
            let _ = std::fs::rename(&temporary, &path);
        }
    };
    match tokio::runtime::Handle::try_current() {
        Ok(runtime) => {
            runtime.spawn_blocking(write);
        }
        Err(_) => write(),
    }
}

/// What `agent` offered when it last ran.
pub(super) fn remembered(home: &Path, agent: &str) -> Offer {
    with_catalog(home, |agents| {
        agents.get(agent).cloned().unwrap_or_default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[tokio::test]
    async fn models_are_kept_across_runs() {
        let home = tempfile::tempdir().expect("home");
        let options = vec![
            acp::SessionConfigOption::select(
                "model",
                "Model",
                "gpt-5.1".to_owned(),
                vec![acp::SessionConfigSelectOption::new("gpt-5.1", "GPT-5.1")],
            )
            .category(acp::SessionConfigOptionCategory::Model),
        ];
        remember(home.path(), "codex", &options);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !home.path().join(FILE).exists() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        // A new process reads the file.
        CATALOG.lock().unwrap_or_else(PoisonError::into_inner).home = None;
        assert_eq!(
            remembered(home.path(), "codex").models,
            vec![Model {
                value: "gpt-5.1".to_owned(),
                name: "GPT-5.1".to_owned(),
            }]
        );
        assert_eq!(remembered(home.path(), "claude"), Offer::default());
    }
}
