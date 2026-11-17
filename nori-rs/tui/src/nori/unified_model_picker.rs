//! Flat model choices with their underlying ACP routes kept together.
use std::path::Path;

use super::session_config_picker::ConfigPickerOrigin;
use crate::app_event::AppEvent;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;
use nori_protocol::acp::v1 as acp;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ModelRoute {
    Session {
        agent_name: String,
        config_id: String,
        value: String,
        option_name: String,
    },
    Agent {
        agent_name: String,
        display_name: String,
        model: String,
    },
}

#[derive(Clone)]
struct ModelChoice {
    name: String,
    identity: String,
    version: Option<String>,
    variant: Option<String>,
    routes: Vec<ModelRoute>,
    current: bool,
}

fn model_name<'a>(name: &'a str, provider: Option<&str>) -> &'a str {
    let mut name = name.rsplit_once('/').map_or(name, |(_, name)| name).trim();
    if let Some(provider) = provider {
        if let Some(rest) = name.strip_prefix(provider)
            && let Some(rest) = rest
                .strip_prefix(" · ")
                .or_else(|| rest.strip_prefix(": "))
                .or_else(|| rest.strip_prefix(" - "))
        {
            name = rest.trim();
        }
        if let Some(rest) = name.strip_suffix(&format!(" ({provider})")) {
            name = rest.trim();
        }
    }
    name
}

// Treat identical floating names as one intent, while keeping numeric versions
// and named variants distinct. A versioned display name can identify a model even
// when an agent advertises a generic value such as `sonnet`.
fn normalized(value: &str) -> String {
    let chars: Vec<char> = value.to_lowercase().chars().collect();
    let mut key = String::new();
    for (index, ch) in chars.iter().copied().enumerate() {
        if ch.is_alphanumeric() {
            key.push(ch);
        } else if matches!(ch, '.' | '-' | '_')
            && index > 0
            && index + 1 < chars.len()
            && chars[index - 1].is_ascii_digit()
            && chars[index + 1].is_ascii_digit()
        {
            key.push('.');
        }
    }
    key
}

fn version(value: &str) -> Option<String> {
    let value = value.rsplit_once('/').map_or(value, |(_, suffix)| suffix);
    let numbers = value
        .split(|ch: char| !ch.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    (!numbers.is_empty()).then(|| numbers.join("."))
}

fn variant(value: &str) -> Option<String> {
    let suffix = value.rsplit_once('/').map_or(value, |(_, suffix)| suffix);
    version(suffix)?;
    let tokens = suffix
        .to_ascii_lowercase()
        .split(['-', '_', '.'])
        .map(str::to_owned)
        .collect::<Vec<_>>();
    ["fast", "thinking", "mini", "lite", "preview"]
        .into_iter()
        .find(|variant| tokens.iter().any(|token| token == variant))
        .map(str::to_owned)
}

fn identity(name: &str, value: &str) -> String {
    let label = model_name(name, None);
    let selected = if label.chars().any(|ch| ch.is_ascii_digit()) {
        label
    } else {
        value.rsplit_once('/').map_or(value, |(_, suffix)| suffix)
    };
    let key = normalized(selected);
    if matches!(key.as_str(), "default" | "auto" | "automatic") {
        "automatic".into()
    } else {
        key
    }
}

fn add(choices: &mut Vec<ModelChoice>, name: &str, value: &str, route: ModelRoute, current: bool) {
    let key = identity(name, value);
    let route_version = version(value);
    let route_variant = variant(value);
    if let Some(choice) = choices.iter_mut().find(|choice| {
        choice.identity == key
            && (choice.version.is_none()
                || route_version.is_none()
                || choice.version == route_version)
            && (choice.version.is_none()
                || route_version.is_none()
                || choice.variant == route_variant)
    }) {
        if !choice.routes.contains(&route) {
            if current {
                choice.routes.insert(0, route);
            } else {
                choice.routes.push(route);
            }
        }
        choice.current |= current;
        if choice.version.is_none() {
            choice.version = route_version;
            choice.variant = route_variant;
        }
    } else {
        let mut display = if key == "automatic" {
            "Automatic".to_owned()
        } else {
            name.to_owned()
        };
        if !name.chars().any(|ch| ch.is_ascii_digit())
            && let Some(version) = &route_version
        {
            display.push_str(&format!(" {version}"));
        }
        if let Some(variant) = &route_variant
            && !normalized(name).contains(variant)
        {
            let mut chars = variant.chars();
            if let Some(first) = chars.next() {
                display.push_str(&format!(
                    " {}{}",
                    first.to_ascii_uppercase(),
                    chars.as_str()
                ));
            }
        }
        choices.push(ModelChoice {
            name: display,
            identity: key,
            version: route_version,
            variant: route_variant,
            routes: vec![route],
            current,
        });
    }
}

fn collect_choices(
    home: &Path,
    active_agent: Option<&str>,
    option: Option<&acp::SessionConfigOption>,
    origin: ConfigPickerOrigin,
) -> Vec<ModelChoice> {
    let mut choices = Vec::new();
    let agents = nori_harness::list_available_agents();
    if let Some(option) = option
        && let acp::SessionConfigKind::Select(select) = &option.kind
    {
        let values: Vec<(&acp::SessionConfigSelectOption, Option<&str>)> = match &select.options {
            acp::SessionConfigSelectOptions::Ungrouped(values) => {
                values.iter().map(|value| (value, None)).collect()
            }
            acp::SessionConfigSelectOptions::Grouped(groups) => groups
                .iter()
                .flat_map(|group| {
                    group
                        .options
                        .iter()
                        .map(|value| (value, Some(group.name.as_str())))
                })
                .collect(),
            _ => Vec::new(),
        };
        for (value, provider) in values {
            add(
                &mut choices,
                model_name(&value.name, provider),
                &value.value.0,
                ModelRoute::Session {
                    agent_name: active_agent.unwrap_or_default().to_owned(),
                    config_id: option.id.to_string(),
                    value: value.value.to_string(),
                    option_name: option.name.clone(),
                },
                value.value == select.current_value,
            );
        }
    }
    if !matches!(origin, ConfigPickerOrigin::Child(_)) {
        for agent in agents
            .iter()
            .filter(|agent| agent.is_installed && Some(agent.agent_name.as_str()) != active_agent)
        {
            for model in nori_harness::cached_agent_models(home, &agent.agent_name) {
                add(
                    &mut choices,
                    model_name(&model.name, Some(&agent.display_name)),
                    &model.value,
                    ModelRoute::Agent {
                        agent_name: agent.agent_name.clone(),
                        display_name: agent.display_name.clone(),
                        model: model.value.clone(),
                    },
                    false,
                );
            }
        }
        for choice in &mut choices {
            let Some(native) = native_agent(&choice.identity) else {
                continue;
            };
            let Some(agent) = agents
                .iter()
                .find(|agent| agent.is_installed && agent.agent_name == native)
            else {
                continue;
            };
            prefer_native(choice, native, &agent.display_name, active_agent);
        }
    }
    super::agent_config_state::sort_models(&mut choices, |choice| (&choice.identity, &choice.name));
    choices
}

fn prefer_native(
    choice: &mut ModelChoice,
    native: &str,
    display_name: &str,
    active_agent: Option<&str>,
) {
    if !choice
        .routes
        .iter()
        .any(|route| route_agent(route) == native)
        && Some(native) != active_agent
        && let Some(model) = choice.routes.first().and_then(native_model_id)
    {
        choice.routes.push(ModelRoute::Agent {
            agent_name: native.to_owned(),
            display_name: display_name.to_owned(),
            model,
        });
    }
    choice.routes.sort_by_key(|route| {
        (
            route_agent(route) != native,
            !matches!(route, ModelRoute::Session { .. }),
        )
    });
}

fn native_agent(identity: &str) -> Option<&'static str> {
    if ["gpt", "astra", "terra", "sol", "luna"]
        .iter()
        .any(|name| identity.starts_with(name))
    {
        Some("codex")
    } else if ["claude", "fable", "opus", "sonnet", "haiku"]
        .iter()
        .any(|name| identity.starts_with(name))
    {
        Some("claude-code")
    } else if identity.starts_with("gemini") {
        Some("gemini")
    } else {
        None
    }
}

fn route_agent(route: &ModelRoute) -> &str {
    match route {
        ModelRoute::Session { agent_name, .. } | ModelRoute::Agent { agent_name, .. } => agent_name,
    }
}

fn native_model_id(route: &ModelRoute) -> Option<String> {
    let value = match route {
        ModelRoute::Session { value, .. } => value,
        ModelRoute::Agent { model, .. } => model,
    };
    let model = value
        .rsplit_once('/')
        .map_or(value.as_str(), |(_, model)| model);
    (!model.is_empty()).then(|| model.to_owned())
}

pub(crate) fn current_routes(
    home: &Path,
    active_agent: &str,
    option: &acp::SessionConfigOption,
) -> Vec<ModelRoute> {
    collect_choices(
        home,
        Some(active_agent),
        Some(option),
        ConfigPickerOrigin::Direct,
    )
    .into_iter()
    .find(|choice| choice.current)
    .map_or_else(Vec::new, |choice| choice.routes)
}

pub(crate) fn preferred_current_routes(
    home: &Path,
    active_agent: &str,
    option: &acp::SessionConfigOption,
) -> Option<(String, Vec<ModelRoute>)> {
    preferred_choice(
        collect_choices(
            home,
            Some(active_agent),
            Some(option),
            ConfigPickerOrigin::Direct,
        ),
        active_agent,
    )
}

fn preferred_choice(
    choices: Vec<ModelChoice>,
    active_agent: &str,
) -> Option<(String, Vec<ModelRoute>)> {
    let choice = choices.into_iter().find(|choice| choice.current)?;
    matches!(choice.routes.first(), Some(ModelRoute::Agent { agent_name, .. }) if agent_name != active_agent)
        .then_some((choice.name, choice.routes))
}

pub(crate) fn params(
    home: &Path,
    active_agent: Option<&str>,
    option: Option<&acp::SessionConfigOption>,
    origin: ConfigPickerOrigin,
) -> SelectionViewParams {
    let choices = collect_choices(home, active_agent, option, origin);
    let items = choices
        .into_iter()
        .map(|choice| {
            let name = choice.name;
            let value_name = name.clone();
            let routes = choice.routes;
            let warm = match routes.first() {
                Some(ModelRoute::Agent {
                    agent_name,
                    display_name,
                    ..
                }) => Some((agent_name.clone(), display_name.clone())),
                Some(ModelRoute::Session { .. }) | None => None,
            };
            SelectionItem {
                name,
                is_current: choice.current,
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::SelectModelRoutes {
                        routes: routes.clone(),
                        value_name: value_name.clone(),
                        origin,
                    })
                })],
                on_highlight: warm.map(|(agent_name, display_name)| {
                    Box::new(move |tx: &crate::app_event_sender::AppEventSender| {
                        tx.send(AppEvent::HighlightAgentCandidate {
                            agent_name: agent_name.clone(),
                            display_name: display_name.clone(),
                        })
                    }) as crate::bottom_pane::SelectionAction
                }),
                dismiss_on_select: true,
                ..Default::default()
            }
        })
        .collect::<Vec<_>>();
    let initial_selected_idx = items.iter().position(|item| item.is_current);
    SelectionViewParams {
        title: Some("Select model".to_owned()),
        items,
        initial_selected_idx,
        is_searchable: true,
        on_dismiss: Some(Box::new(|tx| tx.send(AppEvent::DiscardWarmAgentCandidate))),
        ..Default::default()
    }
    .picker()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use tokio::sync::mpsc::unbounded_channel;

    #[test]
    fn same_model_routes_share_one_clean_row_and_preserve_versions() {
        let option = acp::SessionConfigOption::select(
            "model",
            "Model",
            "local/other",
            vec![
                acp::SessionConfigSelectOption::new("openai/gpt-6.1-terra", "OpenAI/GPT 6.1 Terra"),
                acp::SessionConfigSelectOption::new(
                    "gateway/gpt-6.1-terra",
                    "Gateway/GPT 6.1 Terra",
                ),
                acp::SessionConfigSelectOption::new("gateway/gpt-6-terra", "Gateway/GPT 6 Terra"),
            ],
        )
        .category(acp::SessionConfigOptionCategory::Model);
        let home = tempfile::tempdir().unwrap();
        let params = params(home.path(), None, Some(&option), ConfigPickerOrigin::Direct);
        assert_eq!(
            params
                .items
                .iter()
                .map(|row| row.name.as_str())
                .collect::<Vec<_>>(),
            ["GPT 6.1 Terra", "GPT 6 Terra"]
        );
        assert!(
            params
                .items
                .iter()
                .all(|row| row.description.is_none() && !row.is_header)
        );
        let (tx, mut rx) = unbounded_channel();
        (params.items[0].actions[0])(&crate::app_event_sender::AppEventSender::new(tx));
        let AppEvent::SelectModelRoutes { routes, .. } = rx.try_recv().unwrap() else {
            panic!("expected model routes");
        };
        let session_routes = routes
            .into_iter()
            .filter(|route| matches!(route, ModelRoute::Session { .. }))
            .collect::<Vec<_>>();
        assert_eq!(
            session_routes,
            vec![
                ModelRoute::Session {
                    agent_name: "".into(),
                    config_id: "model".into(),
                    value: "openai/gpt-6.1-terra".into(),
                    option_name: "Model".into()
                },
                ModelRoute::Session {
                    agent_name: "".into(),
                    config_id: "model".into(),
                    value: "gateway/gpt-6.1-terra".into(),
                    option_name: "Model".into()
                },
            ]
        );
    }

    #[test]
    fn ranked_choices_use_models_not_provider_names() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("agent-models.json"), r#"{"mock-model":{"models":[{"value":"gpt-6.1-terra","name":"GPT 6.1 Terra"},{"value":"gpt-6-astra","name":"GPT 6 Astra"},{"value":"claude-fable-5.2","name":"Fable 5.2"}],"settings":[],"refreshed_at":1}}"#).unwrap();
        let params = params(home.path(), None, None, ConfigPickerOrigin::Direct);
        assert_eq!(
            params
                .items
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            ["Fable 5.2", "GPT 6 Astra", "GPT 6.1 Terra"]
        );
    }

    #[test]
    fn current_choice_keeps_its_equivalent_routes_selectable() {
        let option = acp::SessionConfigOption::select(
            "model",
            "Model",
            "one/gpt-6",
            vec![
                acp::SessionConfigSelectOption::new("two/gpt-6", "GPT 6"),
                acp::SessionConfigSelectOption::new("one/gpt-6", "GPT 6"),
            ],
        )
        .category(acp::SessionConfigOptionCategory::Model);
        let home = tempfile::tempdir().unwrap();
        let params = params(home.path(), None, Some(&option), ConfigPickerOrigin::Direct);
        assert_eq!(params.items.len(), 1);
        assert!(params.items[0].is_current);
        let (tx, mut rx) = unbounded_channel();
        (params.items[0].actions[0])(&crate::app_event_sender::AppEventSender::new(tx));
        let AppEvent::SelectModelRoutes { routes, .. } = rx.try_recv().unwrap() else {
            panic!("expected model routes");
        };
        let session_routes = routes
            .iter()
            .filter(|route| matches!(route, ModelRoute::Session { .. }))
            .collect::<Vec<_>>();
        assert!(
            matches!(session_routes[0], ModelRoute::Session { value, .. } if value == "one/gpt-6")
        );
        assert!(
            matches!(session_routes[1], ModelRoute::Session { value, .. } if value == "two/gpt-6")
        );
    }

    #[test]
    fn unversioned_provider_aliases_share_a_floating_intent() {
        let mut choices = Vec::new();
        for agent in ["first", "second"] {
            add(
                &mut choices,
                "Default",
                "default",
                ModelRoute::Agent {
                    agent_name: agent.into(),
                    display_name: agent.into(),
                    model: "default".into(),
                },
                false,
            );
        }
        assert_eq!(choices.len(), 1);
        assert_eq!(choices[0].name, "Automatic");
        assert_eq!(choices[0].routes.len(), 2);
    }

    #[test]
    fn versioned_names_resolve_generic_ids_without_merging_other_versions() {
        let mut choices = Vec::new();
        for (name, value, agent) in [
            ("Sonnet 5.2", "sonnet", "first"),
            ("Sonnet 5.2", "sonnet-5-2", "second"),
            ("Sonnet 5.1", "sonnet-5-1", "third"),
            ("Sonnet 5.2 Fast", "sonnet-5-2-fast", "fourth"),
        ] {
            add(
                &mut choices,
                name,
                value,
                ModelRoute::Agent {
                    agent_name: agent.into(),
                    display_name: agent.into(),
                    model: value.into(),
                },
                false,
            );
        }
        assert_eq!(
            choices
                .iter()
                .map(|choice| (choice.name.as_str(), choice.routes.len()))
                .collect::<Vec<_>>(),
            [("Sonnet 5.2", 2), ("Sonnet 5.1", 1), ("Sonnet 5.2 Fast", 1)]
        );
    }

    #[test]
    fn native_openai_route_precedes_active_gateway_and_keeps_exact_model() {
        let mut choices = Vec::new();
        add(
            &mut choices,
            "GPT-6 Astra",
            "openai/gpt-6-astra",
            ModelRoute::Session {
                agent_name: "opencode".into(),
                config_id: "model".into(),
                value: "openai/gpt-6-astra".into(),
                option_name: "Model".into(),
            },
            true,
        );
        let choice = &mut choices[0];
        assert_eq!(native_agent(&choice.identity), Some("codex"));
        prefer_native(choice, "codex", "Codex", Some("opencode"));
        assert_eq!(
            choice.routes,
            vec![
                ModelRoute::Agent {
                    agent_name: "codex".into(),
                    display_name: "Codex".into(),
                    model: "gpt-6-astra".into()
                },
                ModelRoute::Session {
                    agent_name: "opencode".into(),
                    config_id: "model".into(),
                    value: "openai/gpt-6-astra".into(),
                    option_name: "Model".into()
                },
            ]
        );
        assert_eq!(native_agent("claudefable5.2"), Some("claude-code"));
        assert_eq!(native_agent("gemini3.5flash"), Some("gemini"));
    }

    #[test]
    fn startup_preference_uses_current_model_only_and_skips_native_session() {
        let mut choices = Vec::new();
        add(
            &mut choices,
            "GPT 6.1 Terra",
            "openai/gpt-6.1-terra",
            ModelRoute::Session {
                agent_name: "opencode".into(),
                config_id: "model".into(),
                value: "openai/gpt-6.1-terra".into(),
                option_name: "Model".into(),
            },
            true,
        );
        add(
            &mut choices,
            "GPT 6 Astra",
            "openai/gpt-6-astra",
            ModelRoute::Session {
                agent_name: "opencode".into(),
                config_id: "model".into(),
                value: "openai/gpt-6-astra".into(),
                option_name: "Model".into(),
            },
            false,
        );
        for choice in &mut choices {
            prefer_native(choice, "codex", "Codex", Some("opencode"));
        }
        let (name, routes) = preferred_choice(choices.clone(), "opencode").unwrap();
        assert_eq!(name, "GPT 6.1 Terra");
        assert!(
            matches!(&routes[0], ModelRoute::Agent { agent_name, model, .. } if agent_name == "codex" && model == "gpt-6.1-terra")
        );
        assert!(preferred_choice(choices, "codex").is_none());
    }

    #[test]
    fn explicit_variants_stay_distinct_even_when_labels_are_identical() {
        let mut choices = Vec::new();
        for value in ["gpt-6.1-terra", "gpt-6.1-terra-fast"] {
            add(
                &mut choices,
                "GPT 6.1 Terra",
                value,
                ModelRoute::Agent {
                    agent_name: "test".into(),
                    display_name: "Test".into(),
                    model: value.into(),
                },
                false,
            );
        }
        assert_eq!(
            choices
                .iter()
                .map(|choice| choice.name.as_str())
                .collect::<Vec<_>>(),
            ["GPT 6.1 Terra", "GPT 6.1 Terra Fast"]
        );
    }
}
