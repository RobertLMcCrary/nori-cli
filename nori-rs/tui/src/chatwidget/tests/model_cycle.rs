use super::*;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use nori_protocol::acp::v1 as acp;
use pretty_assertions::assert_eq;

fn models(current: &str) -> Vec<acp::SessionConfigOption> {
    vec![
        acp::SessionConfigOption::select(
            "model",
            "Model",
            current.to_owned(),
            vec![
                acp::SessionConfigSelectOption::new("sonnet", "Sonnet"),
                acp::SessionConfigSelectOption::new("opus", "Opus"),
                acp::SessionConfigSelectOption::new("haiku", "Haiku"),
            ],
        )
        .category(acp::SessionConfigOptionCategory::Model),
    ]
}

fn current_model(chat: &ChatWidget) -> String {
    let option = chat
        .agent_config()
        .option_for_category(&acp::SessionConfigOptionCategory::Model)
        .expect("a model option");
    match &option.raw.kind {
        acp::SessionConfigKind::Select(select) => select.current_value.to_string(),
        _ => panic!("the model option is a select"),
    }
}

#[test]
fn presses_step_through_the_models_and_wrap() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual();
    chat.sync_acp_session_config_snapshot(&models("sonnet"));

    chat.cycle_model(ModelStep::Next);
    assert_eq!(current_model(&chat), "opus");
    chat.cycle_model(ModelStep::Next);
    chat.cycle_model(ModelStep::Next);
    assert_eq!(current_model(&chat), "sonnet");
    chat.cycle_model(ModelStep::Previous);
    assert_eq!(current_model(&chat), "haiku");
}

#[test]
fn a_refused_switch_brings_the_earlier_model_back() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual();
    chat.sync_acp_session_config_snapshot(&models("sonnet"));
    chat.cycle_model(ModelStep::Next);
    assert_eq!(current_model(&chat), "opus");
    let _ = drain_insert_history(&mut rx);

    chat.on_model_cycle_result(Err("not on this plan".to_owned()));

    assert_eq!(current_model(&chat), "sonnet");
    let history: String = drain_insert_history(&mut rx)
        .iter()
        .map(|lines| lines_to_single_string(lines))
        .collect();
    assert!(
        history.contains("Couldn't switch models: not on this plan"),
        "{history}"
    );
}

#[test]
fn an_agent_without_a_choice_of_model_says_so() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual();
    chat.cycle_model(ModelStep::Next);
    let history: String = drain_insert_history(&mut rx)
        .iter()
        .map(|lines| lines_to_single_string(lines))
        .collect();
    assert!(
        history.contains("doesn't offer a choice of model"),
        "{history}"
    );
}

#[test]
fn ctrl_n_and_ctrl_p_in_the_composer_step_the_model() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual();
    for (code, step) in [
        (KeyCode::Char('n'), ModelStep::Next),
        (KeyCode::Char('p'), ModelStep::Previous),
    ] {
        chat.handle_key_event(KeyEvent::new(code, KeyModifiers::CONTROL));
        let mut stepped = None;
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::CycleModel(sent) = event {
                stepped = Some(sent);
            }
        }
        assert_eq!(stepped, Some(step));
    }
}

/// Presses that come while a switch is out only move the target: the agent is asked for the
/// first press's model and then for the last one, never for those in between.
#[tokio::test]
async fn presses_during_a_switch_send_only_the_last() {
    let session = super::part10::start_mock_session().await;
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual();
    chat.harness_handle = Some(session.handle.clone());
    let advertised = session
        .handle
        .get_session_config()
        .await
        .expect("the mock advertises its configuration");
    chat.sync_acp_session_config_snapshot(&advertised);
    let first = current_model(&chat);

    chat.cycle_model(ModelStep::Next);
    let after_one = current_model(&chat);
    chat.cycle_model(ModelStep::Next);
    chat.cycle_model(ModelStep::Next);
    let target = current_model(&chat);
    assert_ne!(first, after_one);

    let mut requested = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while requested.len() < 2 {
            match rx.recv().await {
                Some(AppEvent::AcpSessionConfigSetResult {
                    success,
                    value,
                    config_options,
                    error,
                    ..
                }) => {
                    assert!(success, "{error:?}");
                    requested.push(value);
                    chat.on_model_cycle_result(Ok(config_options));
                }
                Some(_) => {}
                None => panic!("event channel closed"),
            }
        }
    })
    .await
    .expect("both switches complete");

    assert_eq!(requested, vec![after_one, target.clone()]);
    assert_eq!(current_model(&chat), target);
    let _ = session.handle.shutdown().await;
}
