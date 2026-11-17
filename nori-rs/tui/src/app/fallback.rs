//! Ordered provider fallback for an exhausted subscription.

use std::collections::HashSet;
use std::collections::VecDeque;

use super::App;

fn ranked_fallback_candidates(
    mut models: Vec<(String, nori_harness::CachedAgentModel)>,
) -> Vec<String> {
    crate::nori::agent_config_state::sort_models(&mut models, |(_, model)| {
        (&model.value, &model.name)
    });
    models
        .into_iter()
        .map(|(agent, model)| format!("{agent}/{}", model.value))
        .collect()
}

pub(super) struct Fallback {
    remaining: VecDeque<String>,
    same_agent_routes: HashSet<String>,
    pub(super) context: Option<String>,
    pub(super) content: Vec<nori_protocol::acp::v1::ContentBlock>,
    pub(super) echo_id: Option<String>,
    pub(super) recorder: Option<std::sync::Arc<nori_harness::transcript::TranscriptRecorder>>,
    pub(super) original_error: String,
    tried: bool,
    current: Option<(String, String)>,
}

impl Fallback {
    fn next(&mut self, exhausted: &HashSet<String>, current: &str) -> Option<(String, String)> {
        while let Some(id) = self.remaining.pop_front() {
            let (provider, model) = id.split_once('/').unwrap_or((current, &id));
            if !exhausted.contains(provider) || self.same_agent_routes.remove(&id) {
                return Some((provider.to_owned(), model.to_owned()));
            }
        }
        None
    }
}

impl App {
    pub(super) async fn start_fallback(
        &mut self,
        event: nori_protocol::SubscriptionExhausted,
        recorder: Option<std::sync::Arc<nori_harness::transcript::TranscriptRecorder>>,
    ) {
        if self.manual_model_handoff.is_some() {
            self.discard_candidate();
            self.manual_model_handoff = None;
            self.model_route_fallback = None;
        }
        let failed_provider = self.config.active_agent.clone();
        if self.fallback.is_none() {
            let mut models = if event.ranked {
                let models: Vec<_> = nori_harness::list_available_agents()
                    .into_iter()
                    .filter(|agent| agent.is_installed)
                    .flat_map(|agent| {
                        nori_harness::cached_agent_models(&self.config.nori_home, &agent.agent_name)
                            .into_iter()
                            .map(move |model| (agent.agent_name.clone(), model))
                    })
                    .collect();
                ranked_fallback_candidates(models)
            } else {
                event.models
            };
            // Retry other routes for the selected model before the user's broader policy.
            let current_option = self
                .chat_widget
                .agent_config()
                .option_for_category(&nori_protocol::acp::v1::SessionConfigOptionCategory::Model)
                .map(|option| &option.raw);
            let current_value = current_option.and_then(|option| match &option.kind {
                nori_protocol::acp::v1::SessionConfigKind::Select(select) => {
                    Some(select.current_value.to_string())
                }
                _ => None,
            });
            let mut same_agent_routes = HashSet::new();
            let selected_matches = self.selected_model_routes.iter().any(|route| match route {
                crate::nori::unified_model_picker::ModelRoute::Session {
                    agent_name,
                    value,
                    ..
                } => {
                    agent_name == &failed_provider
                        && current_value.as_deref() == Some(value.as_str())
                }
                crate::nori::unified_model_picker::ModelRoute::Agent {
                    agent_name, model, ..
                } => {
                    agent_name == &failed_provider
                        && current_value.as_deref() == Some(model.as_str())
                }
            });
            let mut routes = if selected_matches {
                self.selected_model_routes.clone()
            } else {
                Vec::new()
            };
            if let Some(option) = current_option {
                for route in crate::nori::unified_model_picker::current_routes(
                    &self.config.nori_home,
                    &failed_provider,
                    option,
                ) {
                    if !routes.contains(&route) {
                        routes.push(route);
                    }
                }
            }
            for route in routes.iter().rev() {
                if let crate::nori::unified_model_picker::ModelRoute::Agent {
                    agent_name,
                    model,
                    ..
                } = route
                    && agent_name != &failed_provider
                {
                    let candidate = format!("{agent_name}/{model}");
                    models.retain(|model| model != &candidate);
                    models.insert(0, candidate);
                }
                if let crate::nori::unified_model_picker::ModelRoute::Session {
                    agent_name,
                    value,
                    ..
                } = route
                    && !(agent_name == &failed_provider
                        && current_value.as_deref() == Some(value.as_str()))
                {
                    let candidate = format!("{agent_name}/{value}");
                    if agent_name == &failed_provider {
                        same_agent_routes.insert(candidate.clone());
                    }
                    models.retain(|model| model != &candidate);
                    models.insert(0, candidate);
                }
            }
            self.fallback = Some(Fallback {
                remaining: models.into(),
                same_agent_routes,
                context: event.context,
                content: event.content,
                echo_id: event.echo_id,
                recorder,
                original_error: event.original_error,
                tried: false,
                current: None,
            });
        } else if let Some(fallback) = &mut self.fallback {
            fallback.context = event.context;
        }
        self.exhausted_providers.insert(failed_provider);
        self.advance_fallback().await;
    }

    pub(super) async fn note_fallback(&self, status: &str) {
        let Some((provider, model)) = self
            .fallback
            .as_ref()
            .and_then(|fallback| fallback.current.as_ref())
        else {
            return;
        };
        if let Some(handle) = self.chat_widget.harness_handle()
            && let Err(error) = handle
                .record_handoff_attempt(
                    serde_json::json!({
                        "status": status,
                        "provider": provider,
                        "model": model,
                    })
                    .to_string(),
                )
                .await
        {
            tracing::warn!(%error, "Failed to record provider handoff attempt");
        }
    }

    pub(super) async fn advance_fallback(&mut self) {
        self.note_fallback("failed").await;
        let Some(fallback) = &mut self.fallback else {
            return;
        };
        if let Some((provider, model)) =
            fallback.next(&self.exhausted_providers, &self.config.active_agent)
        {
            fallback.tried = true;
            fallback.current = Some((provider.clone(), model.clone()));
            let display_name = nori_harness::get_agent_display_name(&provider);
            self.note_fallback("requested").await;
            self.select_agent_model(provider, display_name, model).await;
            return;
        }
        if let Some(fallback) = self.fallback.take()
            && fallback.tried
        {
            self.chat_widget.add_error_message(fallback.original_error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn ranked_candidates_follow_model_scores_and_keep_unknown_order() {
        let models = vec![
            (
                "provider-first".to_owned(),
                nori_harness::CachedAgentModel {
                    value: "unranked-first".to_owned(),
                    name: "Unranked first".to_owned(),
                },
            ),
            (
                "provider-luna".to_owned(),
                nori_harness::CachedAgentModel {
                    value: "gpt-6-luna".to_owned(),
                    name: "GPT-6 Luna".to_owned(),
                },
            ),
            (
                "provider-astra".to_owned(),
                nori_harness::CachedAgentModel {
                    value: "gpt-6-astra".to_owned(),
                    name: "GPT-6 Astra".to_owned(),
                },
            ),
            (
                "provider-second".to_owned(),
                nori_harness::CachedAgentModel {
                    value: "unranked-second".to_owned(),
                    name: "Unranked second".to_owned(),
                },
            ),
            (
                "provider-fable".to_owned(),
                nori_harness::CachedAgentModel {
                    value: "fable-5.2".to_owned(),
                    name: "Fable 5.2".to_owned(),
                },
            ),
        ];

        assert_eq!(
            ranked_fallback_candidates(models),
            vec![
                "provider-fable/fable-5.2".to_owned(),
                "provider-astra/gpt-6-astra".to_owned(),
                "provider-luna/gpt-6-luna".to_owned(),
                "provider-first/unranked-first".to_owned(),
                "provider-second/unranked-second".to_owned(),
            ]
        );
    }

    #[test]
    fn ordered_candidates_skip_exhausted_providers_and_stop() {
        let mut fallback = Fallback {
            remaining: ["first/a", "second/b", "third/c"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            same_agent_routes: HashSet::new(),
            context: None,
            content: Vec::new(),
            echo_id: None,
            recorder: None,
            original_error: String::new(),
            tried: false,
            current: None,
        };
        let mut exhausted = HashSet::from(["first".to_string()]);
        assert_eq!(
            fallback.next(&exhausted, "first"),
            Some(("second".to_string(), "b".to_string()))
        );
        exhausted.insert("second".to_string());
        assert_eq!(
            fallback.next(&exhausted, "second"),
            Some(("third".to_string(), "c".to_string()))
        );
        assert_eq!(fallback.next(&exhausted, "third"), None);
    }

    #[test]
    fn exhausted_agent_can_retry_an_explicit_equivalent_route() {
        let mut fallback = Fallback {
            remaining: VecDeque::from([
                "opencode/backup/gpt-6".to_owned(),
                "other/gpt-6".to_owned(),
            ]),
            same_agent_routes: HashSet::from(["opencode/backup/gpt-6".to_owned()]),
            context: None,
            content: Vec::new(),
            echo_id: None,
            recorder: None,
            original_error: String::new(),
            tried: false,
            current: None,
        };
        let exhausted = HashSet::from(["opencode".to_owned()]);
        assert_eq!(
            fallback.next(&exhausted, "opencode"),
            Some(("opencode".to_owned(), "backup/gpt-6".to_owned()))
        );
        assert_eq!(
            fallback.next(&exhausted, "opencode"),
            Some(("other".to_owned(), "gpt-6".to_owned()))
        );
    }
}
