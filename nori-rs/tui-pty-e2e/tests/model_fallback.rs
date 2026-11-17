//! Provider exhaustion carries the current turn to the next configured model.
#![cfg(unix)]

use pretty_assertions::assert_eq;
use tui_pty_e2e::SessionConfig;
use tui_pty_e2e::TIMEOUT;
use tui_pty_e2e::TuiSession;

#[test]
fn exhausted_provider_switches_and_retries_the_prompt() {
    let config = SessionConfig::new()
        .with_policy_pkl(
            "fallback_ranked = false\nfallback_models = List(\"mock-model-alt/mock-model-fast\")",
        )
        .with_agent_env("MOCK_AGENT_EXHAUSTED_MODEL", "mock-model")
        .with_agent_env("MOCK_AGENT_RESPONSE_MOCK_MODEL_ALT", "Fallback answered");
    let mut session = TuiSession::spawn_with_config(30, 100, config).unwrap();
    session.wait_for_text("›", TIMEOUT).unwrap();
    session.submit_input("Remember my request").unwrap();
    session.wait_for_text("Fallback answered", TIMEOUT).unwrap();
    let screen = session.screen_contents();
    assert!(!screen.contains("mock subscription exhausted"), "{screen}");
    assert_eq!(screen.matches("Remember my request").count(), 1, "{screen}");
    let projects = session
        .nori_home_path()
        .unwrap()
        .join("transcripts/by-project");
    let journals: Vec<_> = std::fs::read_dir(projects)
        .unwrap()
        .filter_map(Result::ok)
        .flat_map(|project| {
            std::fs::read_dir(project.path().join("sessions"))
                .into_iter()
                .flatten()
        })
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "pkl"))
        .collect();
    assert_eq!(
        journals.len(),
        1,
        "fallback must keep one canonical journal: {journals:?}"
    );
}

#[test]
fn ranked_policy_uses_the_highest_ranked_cached_model() {
    let refreshed_at: i64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .try_into()
        .unwrap();
    let model_catalog = serde_json::json!({
        "mock-model-alt": {
            "models": [
                {"value": "mock-model-default", "name": "GPT-6 Astra"},
                {"value": "mock-model-fast", "name": "Fable 5.2"}
            ],
            "settings": [],
            "refreshed_at": refreshed_at
        }
    });
    let config = SessionConfig::new()
        .with_policy_pkl("fallback_ranked = true\nfallback_models = List()")
        .with_agent_models_json(model_catalog.to_string())
        .with_agent_env("MOCK_AGENT_EXHAUSTED_MODEL", "mock-model")
        .with_agent_env(
            "MOCK_AGENT_RESPONSE_MOCK_MODEL_ALT",
            "Ranked fallback answered",
        );
    let mut session = TuiSession::spawn_with_config(30, 100, config).unwrap();
    session.wait_for_text("›", TIMEOUT).unwrap();
    session.submit_input("Keep this ranked request").unwrap();
    session
        .wait_for_text("Ranked fallback answered", TIMEOUT)
        .unwrap();
    session.wait_for_text("Mock Fast Model", TIMEOUT).unwrap();

    let screen = session.screen_contents();
    assert_eq!(
        screen.matches("Keep this ranked request").count(),
        1,
        "{screen}"
    );
}

#[test]
fn exhausted_candidates_stop_and_show_the_original_error() {
    let config = SessionConfig::new()
        .with_policy_pkl(
            "fallback_ranked = false\nfallback_models = List(\"mock-model-alt/mock-model-fast\")",
        )
        .with_agent_env("MOCK_AGENT_EXHAUSTED_MODEL", "*");
    let mut session = TuiSession::spawn_with_config(30, 100, config).unwrap();
    session.wait_for_text("›", TIMEOUT).unwrap();
    session.submit_input("Do the work").unwrap();
    session
        .wait_for_text("mock subscription exhausted", TIMEOUT)
        .unwrap();
}
