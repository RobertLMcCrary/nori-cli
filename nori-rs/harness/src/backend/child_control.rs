//! Resolve a visible child to its independent harness.

impl super::AcpBackend {
    pub(crate) async fn controlled_subagent_handle(
        &self,
        session_id: &nori_protocol::acp::v1::SessionId,
    ) -> Option<crate::runtime::HarnessHandle> {
        let _child_gate = self.native_children.control_gate(session_id).await;
        if self.native_children.owns(session_id).await {
            return None;
        }
        self.orchestrator
            .controlled_subagent_handle(session_id)
            .await
    }
}

impl super::AcpBackend {
    pub(crate) async fn steer_subagent_content(
        &self,
        session_id: nori_protocol::acp::v1::SessionId,
        content: Vec<nori_protocol::acp::v1::ContentBlock>,
    ) -> anyhow::Result<()> {
        let _child_gate = self.native_children.control_gate(&session_id).await;
        let generation = self.orchestrator.admission_generation();
        let _admission = self.program_effects.read().await;
        anyhow::ensure!(
            generation == self.orchestrator.admission_generation(),
            "The conversation was rewound before the child message started"
        );
        if self.orchestrator.owns_view(&session_id).await {
            self.orchestrator
                .steer_user_content_message_locked(session_id, content)
                .await
                .map_err(anyhow::Error::msg)
        } else if self.native_children.owns(&session_id).await {
            self.native_children.steer(&session_id, content).await
        } else if let Some(next) = self.orchestrator.descendant_next_hop(&session_id).await {
            next.handle
                .steer_subagent_content(next.session_id, content)
                .await
        } else {
            Err(anyhow::anyhow!("Unknown child session"))
        }
    }

    pub(crate) async fn promote_subagent_queue(
        &self,
        session_id: nori_protocol::acp::v1::SessionId,
    ) -> anyhow::Result<()> {
        let _child_gate = self.native_children.control_gate(&session_id).await;
        let generation = self.orchestrator.admission_generation();
        let _admission = self.program_effects.read().await;
        anyhow::ensure!(
            generation == self.orchestrator.admission_generation(),
            "The conversation was rewound before the child operation started"
        );
        if self.orchestrator.owns_view(&session_id).await {
            self.orchestrator
                .promote_user_queue(&session_id)
                .await
                .map_err(anyhow::Error::msg)
        } else if self.native_children.owns(&session_id).await {
            self.native_children.promote(&session_id).await
        } else if let Some(next) = self.orchestrator.descendant_next_hop(&session_id).await {
            next.handle.promote_subagent_queue(next.session_id).await
        } else {
            Err(anyhow::anyhow!("Unknown child session"))
        }
    }

    pub(crate) async fn cancel_subagent(
        &self,
        session_id: nori_protocol::acp::v1::SessionId,
    ) -> anyhow::Result<()> {
        let _child_gate = self.native_children.control_gate(&session_id).await;
        let generation = self.orchestrator.admission_generation();
        let _admission = self.program_effects.read().await;
        anyhow::ensure!(
            generation == self.orchestrator.admission_generation(),
            "The conversation was rewound before the child operation started"
        );
        if self.orchestrator.owns_view(&session_id).await {
            self.orchestrator
                .cancel_user_turn(&session_id)
                .await
                .map_err(anyhow::Error::msg)
        } else if self.native_children.owns(&session_id).await {
            self.native_children.cancel(&session_id).await
        } else if let Some(next) = self.orchestrator.descendant_next_hop(&session_id).await {
            next.handle.cancel_subagent(next.session_id).await
        } else {
            Err(anyhow::anyhow!("Unknown child session"))
        }
    }
}

impl super::AcpBackend {
    pub(crate) async fn child_config_options(
        &self,
        session_id: nori_protocol::acp::v1::SessionId,
    ) -> anyhow::Result<Vec<nori_protocol::acp::v1::SessionConfigOption>> {
        let _child_gate = self.native_children.control_gate(&session_id).await;
        if self.orchestrator.owns_view(&session_id).await {
            let child = self
                .orchestrator
                .controlled_subagent_handle(&session_id)
                .await
                .ok_or_else(|| anyhow::anyhow!("Child session is unavailable"))?;
            child
                .get_session_config()
                .await
                .ok_or_else(|| anyhow::anyhow!("Child session config is unavailable"))
        } else if self.native_children.owns(&session_id).await {
            self.native_children.config_options(&session_id).await
        } else if let Some(next) = self.orchestrator.descendant_next_hop(&session_id).await {
            next.handle.child_config_options(next.session_id).await
        } else {
            Err(anyhow::anyhow!("Unknown child session"))
        }
    }

    pub(crate) async fn set_child_config_option(
        &self,
        session_id: nori_protocol::acp::v1::SessionId,
        config_id: String,
        value: String,
    ) -> anyhow::Result<Vec<nori_protocol::acp::v1::SessionConfigOption>> {
        let _child_gate = self.native_children.control_gate(&session_id).await;
        let generation = self.orchestrator.admission_generation();
        let _admission = self.program_effects.read().await;
        anyhow::ensure!(
            generation == self.orchestrator.admission_generation(),
            "The conversation was rewound before the child configuration changed"
        );
        if self.orchestrator.owns_view(&session_id).await {
            let child = self
                .orchestrator
                .controlled_subagent_handle(&session_id)
                .await
                .ok_or_else(|| anyhow::anyhow!("Child session is unavailable"))?;
            let updated = child.set_session_config_option(config_id, value).await?;
            if self.native_children.is_suppressed(&session_id).await
                && let Some(mut binding) = self.native_children.rebound_binding(&session_id).await?
            {
                let (model, options) = child_model_and_options(updated.clone())?;
                let recorder = child
                    .conversation_recorder()
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("This child has no conversation journal"))?;
                binding.project = recorder.project_id().to_owned();
                binding.session = recorder.session_id().to_owned();
                binding.model = model;
                binding.options = options;
                self.native_children
                    .bind_rebound(&session_id, recorder, binding)
                    .await?;
            }
            if let Some(parent) = self.transcript_recorder.read().await.clone() {
                let key = format!("managed-child-journal:{session_id}");
                if let Some(payload) = parent.recorded_command_payload(&key).await
                    && payload != "null"
                {
                    let mut binding: super::native_child_history::ReboundBinding =
                        serde_json::from_str(&payload)?;
                    let (model, options) = child_model_and_options(updated.clone())?;
                    let recorder = child
                        .conversation_recorder()
                        .await?
                        .ok_or_else(|| anyhow::anyhow!("This child has no conversation journal"))?;
                    binding.project = recorder.project_id().to_owned();
                    binding.session = recorder.session_id().to_owned();
                    binding.model = model;
                    binding.options = options;
                    let payload = serde_json::to_string(&binding)?;
                    parent.record_command(&key, payload.clone()).await?;
                    anyhow::ensure!(
                        parent.recorded_command_payload(&key).await.as_deref()
                            == Some(payload.as_str()),
                        "Managed child configuration was not persisted"
                    );
                }
            }
            Ok(updated)
        } else if self.native_children.owns(&session_id).await {
            self.native_children
                .set_config_option(&session_id, config_id.into(), value.into())
                .await
        } else if let Some(next) = self.orchestrator.descendant_next_hop(&session_id).await {
            next.handle
                .set_child_config_option(next.session_id, config_id, value)
                .await
        } else {
            Err(anyhow::anyhow!("Unknown child session"))
        }
    }
}

impl super::AcpBackend {
    pub(crate) async fn child_history(
        &self,
        session_id: nori_protocol::acp::v1::SessionId,
    ) -> anyhow::Result<Vec<nori_protocol::SessionEvent>> {
        let _child_gate = self.native_children.control_gate(&session_id).await;
        if self.orchestrator.owns_view(&session_id).await {
            let child = self
                .orchestrator
                .controlled_subagent_handle(&session_id)
                .await
                .ok_or_else(|| anyhow::anyhow!("Child session is unavailable"))?;
            let recorder = child
                .conversation_recorder()
                .await?
                .ok_or_else(|| anyhow::anyhow!("This child has no conversation journal"))?;
            Ok(recorder.replay_child_session_events().await?)
        } else if self.native_children.owns(&session_id).await {
            Ok(self
                .native_children
                .history(&session_id)
                .await?
                .replay_child_session_events()
                .await?)
        } else if let Some(next) = self.orchestrator.descendant_next_hop(&session_id).await {
            next.handle.child_history(next.session_id).await
        } else {
            Err(anyhow::anyhow!("Unknown child session"))
        }
    }

    pub(crate) async fn fork_child_history(
        &self,
        session_id: nori_protocol::acp::v1::SessionId,
        context: String,
        source: (String, i64, i64),
    ) -> anyhow::Result<()> {
        let _child_gate = self.native_children.control_gate(&session_id).await;
        if self.orchestrator.owns_view(&session_id).await {
            let child = self
                .orchestrator
                .controlled_subagent_handle(&session_id)
                .await
                .ok_or_else(|| anyhow::anyhow!("Child session is unavailable"))?;
            child.fork_history(context, Some(source)).await
        } else if !self.native_children.owns(&session_id).await {
            let next = self
                .orchestrator
                .descendant_next_hop(&session_id)
                .await
                .ok_or_else(|| anyhow::anyhow!("Unknown child session"))?;
            next.handle
                .fork_child_history(next.session_id, context, source)
                .await
        } else {
            let generation = self.orchestrator.admission_generation();
            let _admission = self.program_effects.read().await;
            anyhow::ensure!(
                generation == self.orchestrator.admission_generation(),
                "The conversation was rewound before the child fork started"
            );
            let recorder = self.native_children.history(&session_id).await?;
            let records = recorder
                .projected_fork_records(&source.0, source.1, source.2)
                .await?;
            let entries =
                crate::transcript::fork_context::replay_entries_from_records(&records, &source.0);
            let context = crate::transcript::fork_context::context_from_records(records)?;
            let (model, options) = self.native_child_model_and_options(&session_id).await?;
            self.orchestrator
                .fork_native_history(
                    session_id,
                    self.agent_name().to_owned(),
                    Some(model),
                    options,
                    context,
                    source,
                    entries,
                )
                .await
                .map_err(anyhow::Error::msg)
        }
    }

    pub(crate) async fn rewind_child_history(
        &self,
        session_id: nori_protocol::acp::v1::SessionId,
        source: (String, i64, i64),
    ) -> anyhow::Result<()> {
        let _child_gate = self.native_children.control_gate(&session_id).await;
        if self.orchestrator.owns_view(&session_id).await {
            let child = self
                .orchestrator
                .controlled_subagent_handle(&session_id)
                .await
                .ok_or_else(|| anyhow::anyhow!("Child session is unavailable"))?;
            child.rewind_and_rebind(source.0, Some(source.2)).await
        } else if !self.native_children.owns(&session_id).await {
            let next = self
                .orchestrator
                .descendant_next_hop(&session_id)
                .await
                .ok_or_else(|| anyhow::anyhow!("Unknown child session"))?;
            next.handle
                .rewind_child_history(next.session_id, source)
                .await
        } else {
            let generation = self.orchestrator.admission_generation();
            let _admission = self.program_effects.read().await;
            anyhow::ensure!(
                generation == self.orchestrator.admission_generation(),
                "The conversation was rewound before the child operation started"
            );
            self.native_children.reserve(&session_id).await?;
            let subtree = self.native_children.subtree(&session_id).await;
            let mut descendant_ids = subtree
                .iter()
                .filter(|id| *id != &session_id)
                .cloned()
                .collect::<Vec<_>>();
            descendant_ids.sort_by_key(ToString::to_string);
            let mut descendant_gates = Vec::new();
            let mut reserved = Vec::new();
            for id in &descendant_ids {
                descendant_gates.push(self.native_children.control_gate(id).await);
                if let Err(error) = self.native_children.reserve(id).await {
                    for reserved_id in &reserved {
                        self.native_children.release(reserved_id).await;
                    }
                    self.native_children.release(&session_id).await;
                    return Err(error);
                }
                reserved.push(id.clone());
            }
            let result = async {
                let recorder = self.native_children.history(&session_id).await?;
                let records = recorder
                    .projected_fork_records(&source.0, source.1, source.2)
                    .await?;
                let context =
                    crate::transcript::fork_context::context_from_records(records.clone())?;
                let entries =
                    crate::transcript::fork_context::replay_entries_with_children_from_records(
                        &records, &source.0,
                    );
                let nested_bindings =
                    crate::transcript::fork_context::child_bindings_from_records(&records);
                let external_managed = self.managed_child_ids().await?;
                let (model, options) = self.native_child_model_and_options(&session_id).await?;
                let parent_id = self.native_children.parent_id(&session_id).await?;
                let agent = self.agent_name().to_owned();
                let staged = self
                    .orchestrator
                    .prepare_native_rebind(super::orchestrator::NativeRebindSpec {
                        public_id: session_id.clone(),
                        parent_id: parent_id.clone(),
                        agent: agent.clone(),
                        model: Some(model.clone()),
                        options: options.clone(),
                        context,
                        source,
                        entries,
                        nested_bindings,
                        external_managed,
                    })
                    .await
                    .map_err(anyhow::Error::msg)?;
                let staged_recorder = match staged.handle().conversation_recorder().await {
                    Ok(Some(recorder)) => recorder,
                    Ok(None) => {
                        let _ = staged.handle().shutdown().await;
                        return Err(anyhow::anyhow!(
                            "The staged child has no conversation journal"
                        ));
                    }
                    Err(error) => {
                        let _ = staged.handle().shutdown().await;
                        return Err(error);
                    }
                };
                let _event_gate = self.native_children.adoption_gate().await;
                if let Err(error) = self.native_children.check_reservation(&session_id).await {
                    let _ = staged.handle().shutdown().await;
                    return Err(error);
                }
                anyhow::ensure!(
                    self.native_children.subtree(&session_id).await.len() == subtree.len(),
                    "The child hierarchy changed during rewind"
                );
                for id in &descendant_ids {
                    self.native_children.check_reservation(id).await?;
                }
                for id in &subtree {
                    self.native_children.suppress(id, true).await;
                }
                if let Err(error) = self.orchestrator.commit_native_rebind(staged).await {
                    for id in &subtree {
                        self.native_children.suppress(id, false).await;
                    }
                    return Err(anyhow::Error::msg(error));
                }
                let binding = super::native_child_history::ReboundBinding {
                    project: staged_recorder.project_id().to_owned(),
                    session: staged_recorder.session_id().to_owned(),
                    parent: parent_id.to_string(),
                    agent,
                    model,
                    options,
                    rebound: true,
                };
                if let Err(error) = self
                    .native_children
                    .bind_rebound(&session_id, staged_recorder, binding)
                    .await
                {
                    let _ = self.orchestrator.rollback_native_rebind(&session_id).await;
                    return Err(error);
                }
                if let Err(error) = self.orchestrator.activate_native_rebind(&session_id).await {
                    let rolled_back = self
                        .orchestrator
                        .rollback_native_rebind(&session_id)
                        .await
                        .is_ok();
                    if rolled_back
                        && self
                            .native_children
                            .bind_native(&session_id, recorder)
                            .await
                            .is_ok()
                    {
                        for id in &subtree {
                            self.native_children.suppress(id, false).await;
                        }
                    }
                    return Err(anyhow::Error::msg(error));
                }
                for id in &subtree {
                    self.native_children.retire(id).await;
                }
                Ok(())
            }
            .await;
            for id in &reserved {
                self.native_children.release(id).await;
            }
            if result.is_err() {
                self.native_children.release(&session_id).await;
            }
            result
        }
    }
    async fn native_child_model_and_options(
        &self,
        session_id: &nori_protocol::acp::v1::SessionId,
    ) -> anyhow::Result<(String, Vec<(String, String)>)> {
        child_model_and_options(self.native_children.config_options(session_id).await?)
    }
}

pub(super) fn child_model_and_options(
    snapshot: Vec<nori_protocol::acp::v1::SessionConfigOption>,
) -> anyhow::Result<(String, Vec<(String, String)>)> {
    let mut model = None;
    let mut options = Vec::new();
    for option in snapshot {
        if let nori_protocol::acp::v1::SessionConfigKind::Select(select) = option.kind {
            let value = select.current_value.to_string();
            if option.category == Some(nori_protocol::acp::v1::SessionConfigOptionCategory::Model)
                || option.id.to_string() == "model"
            {
                model = Some(value);
            } else {
                options.push((option.id.to_string(), value));
            }
        } else {
            return Err(anyhow::anyhow!(
                "Cannot preserve an unsupported child configuration option"
            ));
        }
    }
    Ok((
        model.ok_or_else(|| {
            anyhow::anyhow!("The child has not advertised its model configuration")
        })?,
        options,
    ))
}
