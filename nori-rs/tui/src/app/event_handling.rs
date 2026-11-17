use super::*;
use crossterm::event::KeyModifiers;

#[derive(Debug, PartialEq, Eq)]
enum CandidateActivationEvent {
    Forward,
    CapturedError,
    Ended(String),
}

fn classify_candidate_activation_event(
    event: &nori_protocol::SessionEvent,
    activation_error: &mut Option<String>,
) -> CandidateActivationEvent {
    match event {
        nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::Response {
            response: Err(error),
            ..
        }) => {
            tracing::warn!(
                code = i32::from(error.code),
                message = %error.message,
                detail = ChatWidget::acp_error_detail(error),
                "ACP candidate activation request failed"
            );
            if let Some(message) = ChatWidget::acp_error_message_with_detail(error) {
                *activation_error = Some(message);
            }
            CandidateActivationEvent::CapturedError
        }
        nori_protocol::SessionEvent::Nori(nori_protocol::NoriEvent::RequestFailed(failure)) => {
            tracing::warn!(
                message = %failure.message,
                kind = ?failure.kind,
                "candidate activation request failed"
            );
            if activation_error.is_none() {
                *activation_error = Some(failure.message.clone());
            }
            CandidateActivationEvent::CapturedError
        }
        nori_protocol::SessionEvent::Nori(nori_protocol::NoriEvent::SessionEnded(ended)) => {
            CandidateActivationEvent::Ended(activation_error.take().unwrap_or_else(|| {
                ended
                    .message
                    .clone()
                    .unwrap_or_else(|| "Candidate session ended during activation".to_string())
            }))
        }
        _ => CandidateActivationEvent::Forward,
    }
}

fn candidate_model_config(
    options: &[nori_protocol::acp::v1::SessionConfigOption],
    model: &str,
) -> Option<String> {
    use nori_protocol::acp::v1 as acp;
    options.iter().find_map(|option| {
        if option.category != Some(acp::SessionConfigOptionCategory::Model) {
            return None;
        }
        let acp::SessionConfigKind::Select(select) = &option.kind else {
            return None;
        };
        let offered = match &select.options {
            acp::SessionConfigSelectOptions::Ungrouped(options) => options
                .iter()
                .any(|option| option.value.to_string() == model),
            acp::SessionConfigSelectOptions::Grouped(groups) => groups
                .iter()
                .flat_map(|group| &group.options)
                .any(|option| option.value.to_string() == model),
            _ => false,
        };
        offered.then(|| option.id.to_string())
    })
}

impl App {
    /// Clear and report a due deferred agent spawn.
    ///
    /// Per-session startup defers the agent spawn until the chosen skillset's
    /// files are on disk. Returns `true` (clearing the pending flag) when a
    /// spawn is now due; `false` while agent preparation is still in
    /// flight or no spawn was deferred. Shared by the skillset-applied and
    /// picker-dismissed paths so their guard cannot drift.
    pub(super) fn take_deferred_spawn(&mut self) -> bool {
        if self.deferred_spawn_pending && self.primary_agent_preparation.is_none() {
            self.deferred_spawn_pending = false;
            true
        } else {
            false
        }
    }

    /// Resolve a deferred agent spawn and refresh system info after a skillset
    /// install or switch completes. Both `SkillsetInstallResult` and
    /// `SkillsetSwitchResult` route here: a per-session startup that lands on a
    /// non-worktree cwd installs (rather than switches), so the deferred spawn
    /// must resolve on either result or the agent never starts.
    fn on_skillset_applied(&mut self, success: bool) {
        if !success {
            return;
        }
        if self.take_deferred_spawn() {
            // The chosen skillset has written its workspace state, so activate
            // the session now instead of waiting for the first prompt.
            self.pending_session_activation = Some(PendingSessionActivation::New);
            self.begin_agent_preparation(crate::app_event::AgentPrepareIntent::Idle);
        }
        self.request_system_info_refresh(
            self.config.cwd.clone(),
            self.config.active_agent.clone().into(),
            self.chat_widget.first_prompt_text(),
        );
    }

    pub(crate) async fn handle_tui_event(
        &mut self,
        tui: &mut tui::Tui,
        event: TuiEvent,
    ) -> Result<bool> {
        if matches!(event, TuiEvent::Draw) {
            self.handle_resize_reflow_draw();
        }
        if matches!(&event, TuiEvent::Key(key) if key.kind == KeyEventKind::Press)
            || matches!(&event, TuiEvent::Paste(_))
        {
            self.program_status.acknowledge();
        }
        if let TuiEvent::Key(key) = &event {
            match self.chat_widget.quit_key_action(*key) {
                crate::chatwidget::QuitKeyAction::Exit => {
                    // Do not synchronously format or drain in-flight output on
                    // the immediate quit chord; the canonical journal retains it.
                    if self.native_history.cells().is_empty()
                        && !self.chat_widget.has_active_agent_stream()
                        && !tui.has_pending_history()
                    {
                        let _ = self.finish_native_history(tui)?;
                    }
                    self.chat_widget.begin_immediate_exit();
                    return Ok(false);
                }
                crate::chatwidget::QuitKeyAction::Interrupt => {
                    if self.chat_widget.external_cli_login_active() {
                        self.chat_widget.route_external_cli_login_key_event(*key);
                    } else if matches!(
                        self.session_view.focused(),
                        crate::subagent_switcher::Target::Subagent(_)
                    ) && !self.chat_widget.has_active_overlay_or_popup()
                    {
                        self.handle_key_event(
                            tui,
                            crossterm::event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                        )
                        .await;
                    } else {
                        self.chat_widget.interrupt_for_quit_key();
                    }
                    return Ok(true);
                }
                crate::chatwidget::QuitKeyAction::Continue => {}
            }
        }
        let event = if self.chat_widget.external_cli_login_active() {
            match event {
                TuiEvent::Key(key_event) => {
                    self.chat_widget
                        .route_external_cli_login_key_event(key_event);
                    return Ok(true);
                }
                TuiEvent::Paste(pasted) => {
                    self.chat_widget.route_external_cli_login_paste(pasted);
                    return Ok(true);
                }
                other => other,
            }
        } else {
            event
        };
        // Redraw works everywhere: with a popup open, with a pager overlay open, and while a turn
        // streams. It must not be left to a popup that would swallow the key.
        if let TuiEvent::Key(key_event) = &event
            && key_event.kind == KeyEventKind::Press
            && crate::nori::hotkey_match::matches_binding(
                self.hotkey_config
                    .binding_for(nori_config::HotkeyAction::Redraw),
                key_event,
            )
        {
            self.repaint_screen(tui);
            return Ok(true);
        }
        if let TuiEvent::Key(key) = &event
            && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Tab | KeyCode::BackTab)
            && (self.overlay.is_none() || matches!(self.overlay, Some(Overlay::Subagents(_))))
            && !self.chat_widget.has_active_overlay_or_popup()
        {
            let reverse =
                key.code == KeyCode::BackTab || key.modifiers.contains(KeyModifiers::SHIFT);
            self.cycle_subagent_session(tui, reverse);
            return Ok(true);
        }
        if self.overlay.is_none()
            && !self.chat_widget.has_active_overlay_or_popup()
            && let TuiEvent::Key(key) = &event
            && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
            && key.code == KeyCode::Tab
            && key.modifiers.is_empty()
        {
            if let crate::subagent_switcher::Target::Subagent(child) = self.session_view.focused() {
                let roster = self.chat_widget.subagent_roster(std::time::Instant::now());
                let target = self
                    .chat_widget
                    .subagent_parent(child)
                    .map(crate::subagent_switcher::Target::Subagent)
                    .map(|target| roster.resolve(target))
                    .filter(|target| roster.entry(*target).is_some())
                    .unwrap_or(crate::subagent_switcher::Target::Main);
                self.focus_session_view(tui, target);
            }
            return Ok(true);
        }
        if self.overlay.is_none()
            && !self.chat_widget.has_active_overlay_or_popup()
            && self.backtrack.response_focus.is_none()
            && let TuiEvent::Key(key) = &event
            && key.kind == KeyEventKind::Press
            && key.code == KeyCode::Char('e')
            && key.modifiers == KeyModifiers::CONTROL
        {
            let call_id = self.chat_widget.nearest_transcript_tool();
            if let Some(call_id) = call_id {
                self.toggle_tool(tui, call_id);
            } else if self.session_view.focused() == crate::subagent_switcher::Target::Main {
                self.open_latest_main_tool(tui);
            }
            return Ok(true);
        }
        if self.overlay.is_none()
            && !self.chat_widget.has_active_overlay_or_popup()
            && matches!(
                self.session_view.focused(),
                crate::subagent_switcher::Target::Subagent(_)
            )
            && let TuiEvent::Key(key) = &event
            && key.kind == KeyEventKind::Press
            && matches!(key.code, KeyCode::PageUp | KeyCode::PageDown)
        {
            let amount = isize::try_from(tui.terminal.size()?.height / 2).unwrap_or(isize::MAX);
            self.session_view.scroll(if key.code == KeyCode::PageUp {
                amount
            } else {
                -amount
            });
            tui.frame_requester().schedule_frame();
            return Ok(true);
        }
        if matches!(self.overlay, Some(Overlay::Subagents(_))) {
            tui.set_cursor_style(crossterm::cursor::SetCursorStyle::DefaultUserShape)?;
            self.handle_subagent_overlay_event(tui, event)?;
        } else if self.overlay.is_some() {
            tui.set_cursor_style(crossterm::cursor::SetCursorStyle::DefaultUserShape)?;
            let _ = self.handle_backtrack_overlay_event(tui, event).await?;
        } else {
            match event {
                TuiEvent::Key(key_event) => {
                    self.handle_key_event(tui, key_event).await;
                }
                TuiEvent::Paste(pasted) => {
                    self.chat_widget.handle_paste(pasted);
                }
                TuiEvent::Mouse(mouse) => {
                    if mouse.kind
                        == crossterm::event::MouseEventKind::Down(
                            crossterm::event::MouseButton::Left,
                        )
                    {
                        self.tool_gesture.cancel();
                    }
                    if self.chat_widget.has_active_overlay_or_popup() {
                        self.chat_widget.handle_popup_mouse(mouse);
                        return Ok(true);
                    }
                    if mouse.kind
                        == crossterm::event::MouseEventKind::Down(
                            crossterm::event::MouseButton::Left,
                        )
                        && let Some(key) = self
                            .chat_widget
                            .response_focus_action_at(mouse.column, mouse.row)
                    {
                        self.handle_response_focus(tui, key);
                        return Ok(true);
                    }
                    if mouse.kind
                        == crossterm::event::MouseEventKind::Down(
                            crossterm::event::MouseButton::Left,
                        )
                        && let Some(call_id) =
                            self.chat_widget.transcript_tool_at(mouse.column, mouse.row)
                    {
                        self.tool_gesture.press(
                            self.session_view.focused(),
                            call_id,
                            mouse.column,
                            mouse.row,
                        );
                        self.session_view.start_drag(mouse.row);
                        return Ok(true);
                    }
                    if mouse.kind
                        == crossterm::event::MouseEventKind::Drag(
                            crossterm::event::MouseButton::Left,
                        )
                    {
                        self.tool_gesture.drag(
                            self.session_view.focused(),
                            mouse.column,
                            mouse.row,
                        );
                    } else if mouse.kind
                        == crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left)
                    {
                        let hit = self.chat_widget.transcript_tool_at(mouse.column, mouse.row);
                        if let Some(call_id) = self
                            .tool_gesture
                            .release(self.session_view.focused(), hit.as_deref())
                        {
                            self.session_view.end_drag();
                            self.toggle_tool(tui, call_id);
                            return Ok(true);
                        }
                    }
                    if self
                        .chat_widget
                        .subagent_strip_contains(mouse.column, mouse.row)
                        && matches!(
                            mouse.kind,
                            crossterm::event::MouseEventKind::ScrollLeft
                                | crossterm::event::MouseEventKind::ScrollRight
                        )
                    {
                        self.chat_widget.scroll_subagent_strip(
                            if mouse.kind == crossterm::event::MouseEventKind::ScrollLeft {
                                -1
                            } else {
                                1
                            },
                        );
                        return Ok(true);
                    }
                    if mouse.kind
                        == crossterm::event::MouseEventKind::Down(
                            crossterm::event::MouseButton::Left,
                        )
                        && let Some(action) = self
                            .chat_widget
                            .subagent_strip_action(mouse.column, mouse.row)
                    {
                        use crate::chatwidget::subagents::StripAction;
                        match action {
                            StripAction::Focus(key) => {
                                self.session_view.start_strip_drag(mouse.column, Some(key))
                            }
                            StripAction::Previous => self.chat_widget.scroll_subagent_strip(-1),
                            StripAction::Next => self.chat_widget.scroll_subagent_strip(1),
                        }
                        return Ok(true);
                    }
                    if self.session_view.is_strip_dragging() {
                        match mouse.kind {
                            crossterm::event::MouseEventKind::Drag(
                                crossterm::event::MouseButton::Left,
                            ) => {
                                let delta = self.session_view.strip_drag(mouse.column);
                                if delta != 0 {
                                    self.chat_widget.scroll_subagent_strip(delta);
                                }
                                return Ok(true);
                            }
                            crossterm::event::MouseEventKind::Up(
                                crossterm::event::MouseButton::Left,
                            ) => {
                                if let Some(key) = self.session_view.end_strip_drag()
                                    && self
                                        .chat_widget
                                        .subagent_strip_action(mouse.column, mouse.row)
                                        == Some(crate::chatwidget::subagents::StripAction::Focus(
                                            key,
                                        ))
                                {
                                    self.focus_subagent_from_mouse(tui, key);
                                }
                                return Ok(true);
                            }
                            _ => {}
                        }
                    }
                    if matches!(
                        self.session_view.focused(),
                        crate::subagent_switcher::Target::Subagent(_)
                    ) && matches!(
                        mouse.kind,
                        crossterm::event::MouseEventKind::ScrollUp
                            | crossterm::event::MouseEventKind::ScrollDown
                    ) {
                        self.session_view.scroll(
                            if mouse.kind == crossterm::event::MouseEventKind::ScrollUp {
                                3
                            } else {
                                -3
                            },
                        );
                        tui.frame_requester().schedule_frame();
                    } else if self.session_view.focused() == crate::subagent_switcher::Target::Main
                        && matches!(
                            mouse.kind,
                            crossterm::event::MouseEventKind::ScrollUp
                                | crossterm::event::MouseEventKind::ScrollDown
                        )
                    {
                        self.scroll_main_session(
                            tui,
                            if mouse.kind == crossterm::event::MouseEventKind::ScrollUp {
                                3
                            } else {
                                -3
                            },
                        );
                    } else if mouse.kind
                        == crossterm::event::MouseEventKind::Down(
                            crossterm::event::MouseButton::Left,
                        )
                        && self
                            .chat_widget
                            .place_composer_cursor(mouse.column, mouse.row)
                    {
                        self.session_view.end_drag();
                    } else if mouse.kind
                        == crossterm::event::MouseEventKind::Down(
                            crossterm::event::MouseButton::Left,
                        )
                        && self.chat_widget.is_chat_content_row(mouse.row)
                    {
                        self.session_view.start_drag(mouse.row);
                    } else if mouse.kind
                        == crossterm::event::MouseEventKind::Down(
                            crossterm::event::MouseButton::Left,
                        )
                    {
                        self.session_view.end_drag();
                    } else if mouse.kind
                        == crossterm::event::MouseEventKind::Drag(
                            crossterm::event::MouseButton::Left,
                        )
                    {
                        let delta = self.session_view.drag(mouse.row);
                        if delta != 0 {
                            if self.session_view.focused() == crate::subagent_switcher::Target::Main
                            {
                                self.scroll_main_session(tui, delta);
                            } else {
                                self.session_view.scroll(delta);
                                tui.frame_requester().schedule_frame();
                            }
                        }
                    } else if mouse.kind
                        == crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left)
                    {
                        self.session_view.end_drag();
                    }
                }
                TuiEvent::Draw => {
                    // The main conversation belongs to native terminal scrollback. Mouse
                    // reporting also captures wheel events, so reserve it for projected views.
                    let projected = self.backtrack.response_focus.is_some()
                        || self.session_view.main_scrolling()
                        || self.session_view.focused() != crate::subagent_switcher::Target::Main;
                    if !projected
                        && !self.chat_widget.has_active_overlay_or_popup()
                        && !self.deferred_history_lines.is_empty()
                    {
                        tui.insert_history_lines(std::mem::take(&mut self.deferred_history_lines));
                    }
                    let _ = tui.set_mouse_capture(
                        projected || self.chat_widget.has_active_overlay_or_popup(),
                    );
                    tui.set_cursor_style(self.chat_widget.composer_cursor_style())?;
                    self.chat_widget.pre_draw_tick();
                    self.update_focused_session_view(tui)?;
                    self.chat_widget.maybe_post_pending_notification(tui);
                    if self
                        .chat_widget
                        .handle_paste_burst_tick(tui.frame_requester())
                    {
                        return Ok(true);
                    }
                    let screen = tui.terminal.size()?;
                    self.chat_widget.set_screen_rows(screen.height);
                    // Two rows above the viewport must stay free: history is flushed by
                    // scrolling them, and a taller viewport would leave it nowhere to go.
                    let maximum = screen.height.saturating_sub(2).max(1);
                    let viewport_rows = if projected {
                        maximum
                    } else {
                        self.chat_widget.desired_height(screen.width).min(maximum)
                    };
                    let popup_rows = if projected {
                        viewport_rows
                            .saturating_sub(self.chat_widget.primary_composer_height(screen.width))
                    } else {
                        self.chat_widget.popup_rows(screen.width)
                    };
                    tui.draw(viewport_rows, popup_rows, |frame| {
                        self.chat_widget.render(frame.area(), frame.buffer);
                        if let Some((x, y)) = self.chat_widget.cursor_pos(frame.area()) {
                            frame.set_cursor_position((x, y));
                        }
                    })?;
                }
            }
        }
        Ok(true)
    }

    pub(super) fn apply_approval_preset(
        &mut self,
        approval: AskForApproval,
        sandbox: SandboxPolicy,
    ) {
        self.config.approval_policy = approval;
        self.config.sandbox_policy = sandbox.clone();
        #[cfg(target_os = "windows")]
        if !matches!(sandbox, nori_config::SandboxPolicy::ReadOnly)
            || codex_sandbox::get_platform_sandbox().is_some()
        {
            self.config.forced_auto_mode_downgraded_on_windows = false;
        }
        self.chat_widget.set_approval_policy(approval);
        self.chat_widget.set_sandbox_policy(sandbox);
    }

    pub(super) async fn handle_event(
        &mut self,
        tui: &mut tui::Tui,
        event: AppEvent,
    ) -> Result<bool> {
        if self.waiting_model_presses.is_waiting()
            && self.session_view.focused() == crate::subagent_switcher::Target::Main
            && !self.chat_widget.agent_config().is_empty()
            && let Some((step, count)) = self.waiting_model_presses.take(std::time::Instant::now())
        {
            for _ in 0..count {
                self.chat_widget.cycle_model(step);
            }
        }
        match event {
            AppEvent::ChildHistoryReady {
                request_id,
                key,
                focus_responses,
                result,
            } => {
                let keys = if focus_responses {
                    let Some(pending) = self.backtrack.pending_child_focus.take() else {
                        return Ok(true);
                    };
                    if pending.request_id != request_id {
                        self.backtrack.pending_child_focus = Some(pending);
                        return Ok(true);
                    }
                    if pending.key != key
                        || self.session_view.focused()
                            != crate::subagent_switcher::Target::Subagent(key)
                        || pending.generation != self.chat_widget.session_generation()
                        || self.chat_widget.subagent_session(key).as_ref()
                            != Some(&pending.session_id)
                    {
                        return Ok(true);
                    }
                    pending.keys
                } else {
                    Vec::new()
                };
                match result {
                    Ok(events) => {
                        self.show_child_history(tui, key, focus_responses, events);
                        for key in keys {
                            if !self.handle_response_focus(tui, key) {
                                break;
                            }
                        }
                    }
                    Err(error) => self.show_child_history_error(tui, key, error),
                }
            }
            AppEvent::ChildHistoryActionFinished {
                key,
                rewound,
                result,
            } => match result {
                Ok(events) if rewound => {
                    self.chat_widget.replace_subagent_history(key, &events);
                    tui.frame_requester().schedule_frame();
                }
                Ok(_) => {}
                Err(error) => self.show_child_history_error(tui, key, error),
            },
            AppEvent::SubagentControlsReady {
                key,
                revision,
                handle,
                config_options,
                error,
            } => {
                if self.session_view.controls_revision(key) != revision {
                    return Ok(true);
                }
                if let Some(error) = error {
                    tracing::warn!(%error, "failed to load child session controls");
                    return Ok(true);
                }
                self.session_view.set_controls(key, handle, &config_options);
                if self.session_view.focused() == crate::subagent_switcher::Target::Subagent(key) {
                    let roster = self.chat_widget.subagent_roster(std::time::Instant::now());
                    let name = roster
                        .entry(crate::subagent_switcher::Target::Subagent(key))
                        .and_then(|entry| entry.agent.as_deref());
                    self.chat_widget.set_focused_session_config(
                        Some(
                            self.session_view
                                .controlled_config(key)
                                .cloned()
                                .unwrap_or_default(),
                        ),
                        name,
                    );
                }
            }
            AppEvent::SubagentConfigSetResult {
                key,
                result,
                config_id,
                value,
                route_fallback_eligible,
                route_token,
            } => {
                if self.stale_model_route_token(route_token) {
                    return Ok(true);
                }
                if self.is_model_child_route(route_token, key, &config_id, &value) {
                    if result.is_err()
                        && route_fallback_eligible
                        && self.advance_model_route().await
                    {
                        return Ok(true);
                    }
                    self.model_route_fallback = None;
                }
                let roster = self.chat_widget.subagent_roster(std::time::Instant::now());
                let crate::subagent_switcher::Target::Subagent(key) =
                    roster.resolve(crate::subagent_switcher::Target::Subagent(key))
                else {
                    return Ok(true);
                };
                match result {
                    Ok(config_options) => {
                        let handle = self.session_view.controlled_handle(key);
                        self.session_view.set_controls(key, handle, &config_options);
                        if self.session_view.focused()
                            == crate::subagent_switcher::Target::Subagent(key)
                        {
                            let roster =
                                self.chat_widget.subagent_roster(std::time::Instant::now());
                            let name = roster
                                .entry(crate::subagent_switcher::Target::Subagent(key))
                                .and_then(|entry| entry.agent.as_deref());
                            self.chat_widget.set_focused_session_config(
                                self.session_view.controlled_config(key).cloned(),
                                name,
                            );
                        }
                    }
                    Err(error) => tui.show_toast(error),
                }
            }
            AppEvent::ContextTimelineReady {
                generation,
                conversation_id,
                result,
            } => {
                self.chat_widget
                    .handle_context_timeline(generation, conversation_id, result);
            }
            AppEvent::NewSession => {
                if matches!(self.candidate_agent, Some(CandidateAgent::Prepared { .. })) {
                    let Some(CandidateAgent::Prepared {
                        agent_name,
                        display_name,
                        mut agent,
                        ..
                    }) = self.candidate_agent.take()
                    else {
                        unreachable!("candidate variant checked above")
                    };
                    let config = self.config_for_agent(&agent_name);
                    let mut prepare_spec = crate::chatwidget::agent::agent_prepare_spec(
                        config.clone(),
                        self.fallback
                            .as_ref()
                            .and_then(|fallback| fallback.context.clone())
                            .or_else(|| {
                                self.manual_model_handoff
                                    .as_ref()
                                    .map(|handoff| handoff.context.clone())
                            }),
                    );
                    prepare_spec.handoff = self
                        .fallback
                        .as_ref()
                        .and_then(|fallback| fallback.recorder.clone())
                        .or_else(|| {
                            self.manual_model_handoff
                                .as_ref()
                                .map(|handoff| handoff.recorder.clone())
                        });
                    if let Err(error) =
                        nori_harness::runtime::refresh_prepared_agent(&mut agent, prepare_spec)
                    {
                        tokio::spawn((*agent).shutdown());
                        self.pending_candidate_model = None;
                        if self.fallback.is_some() {
                            self.advance_fallback().await;
                            return Ok(true);
                        }
                        if self.manual_model_handoff.is_some() {
                            if self.advance_model_route().await {
                                return Ok(true);
                            }
                            self.manual_model_handoff = None;
                            tui.show_toast(format!("Couldn't activate model: {error}"));
                            return Ok(true);
                        }
                        self.manual_model_handoff = None;
                        self.chat_widget.set_login_agent_override(Some(agent_name));
                        self.chat_widget.add_error_message(format!(
                            "Couldn't activate {display_name}: {error}"
                        ));
                        return Ok(true);
                    }
                    let mut init = self.chat_widget_init(
                        tui.frame_requester(),
                        None,
                        Vec::new(),
                        Some(config.active_agent.clone()),
                        false,
                        None,
                    );
                    init.config = config;
                    init.prepared_agent = Some(*agent);
                    let mut widget = ChatWidget::new_candidate(init);
                    if self.session_card_shown {
                        widget.suppress_welcome_card();
                    }
                    self.candidate_agent = Some(CandidateAgent::Activating {
                        agent_name,
                        widget: Box::new(widget),
                        activation_error: None,
                        activation_route_eligible: false,
                    });
                    tui.frame_requester().schedule_frame();
                    return Ok(true);
                }

                self.fallback = None;
                self.exhausted_providers.clear();
                self.discard_candidate();
                if let Some(preparation) = &mut self.primary_agent_preparation {
                    preparation.intent = crate::app_event::AgentPrepareIntent::Idle;
                    self.pending_session_activation = Some(PendingSessionActivation::New);
                    return Ok(true);
                }
                if self.prepared_agent.is_some() {
                    self.pending_session_activation = Some(PendingSessionActivation::New);
                    let agent = match self.take_refreshed_prepared_agent() {
                        Ok(Some(agent)) => agent,
                        Ok(None) => return Ok(true),
                        Err(error) => {
                            self.chat_widget.add_error_message(format!(
                                "Couldn't activate prepared agent: {error}"
                            ));
                            return Ok(true);
                        }
                    };
                    self.pending_session_activation = None;
                    self.deferred_spawn_pending = false;
                    let (initial_prompt, initial_images) = self.chat_widget.take_initial_input();
                    let composer_draft = self.chat_widget.take_composer_draft();
                    let loop_state = self.chat_widget.loop_state();
                    self.shutdown_current_conversation();
                    let mut init = self.chat_widget_init(
                        tui.frame_requester(),
                        initial_prompt,
                        initial_images,
                        None,
                        false,
                        None,
                    );
                    init.prepared_agent = Some(agent);
                    self.chat_widget = ChatWidget::new(init);
                    self.configure_new_chat_widget();
                    self.chat_widget.restore_composer_draft(composer_draft);
                    if let Some((remaining, total)) = loop_state {
                        self.chat_widget.set_loop_state(remaining, total);
                    }
                    tui.frame_requester().schedule_frame();
                    return Ok(true);
                }

                if !self.chat_widget.has_harness_session() {
                    self.pending_session_activation = Some(PendingSessionActivation::New);
                    self.begin_agent_preparation(crate::app_event::AgentPrepareIntent::Idle);
                    return Ok(true);
                }

                self.pending_session_activation = Some(PendingSessionActivation::New);
                self.deferred_spawn_pending = false;
                let summary = session_summary(
                    self.chat_widget.token_usage(),
                    self.chat_widget.conversation_id(),
                    self.chat_widget.session_stats().has_activity(),
                );
                let (initial_prompt, initial_images) = self.chat_widget.take_initial_input();
                self.shutdown_current_conversation();
                let init = self.chat_widget_init(
                    tui.frame_requester(),
                    initial_prompt,
                    initial_images,
                    None,
                    true,
                    None,
                );
                self.chat_widget = ChatWidget::new(init);
                self.configure_new_chat_widget();
                self.begin_agent_preparation(crate::app_event::AgentPrepareIntent::Idle);
                if let Some(summary) = summary {
                    let mut lines: Vec<Line<'static>> = Vec::new();
                    if let Some(usage_line) = summary.usage_line {
                        lines.push(usage_line.into());
                    }
                    if let Some(command) = summary.resume_command {
                        lines.push(RESUME_HINT_LEAD.into());
                        lines.push(command.cyan().into());
                    }
                    self.chat_widget.add_plain_history_lines(lines);
                }
                tui.frame_requester().schedule_frame();
            }
            AppEvent::SessionCloseFailed { message } => {
                self.chat_widget.on_session_close_failed(message);
            }
            AppEvent::SessionClosed => {
                self.program_status.clear();
                self.program_status.idle();
                // The session was released; land back on the session picker
                // with a fresh deferred widget — never auto-claim a new
                // session (on a cloud agent that would boot a new VM).
                self.cancel_primary_agent_preparation();
                self.discard_candidate();
                self.pending_session_activation = None;
                if let Some(agent) = self.prepared_agent.take() {
                    tokio::spawn(agent.shutdown());
                }
                self.prepared_agent_initial_context = None;
                let init = self.chat_widget_init(
                    tui.frame_requester(),
                    None,
                    Vec::new(),
                    None,
                    true,
                    None,
                );
                self.chat_widget = ChatWidget::new(init);
                self.configure_new_chat_widget();
                self.deferred_spawn_pending = true;
                self.begin_agent_preparation(crate::app_event::AgentPrepareIntent::Picker {
                    fallback_to_spawn: false,
                });
                tui.frame_requester().schedule_frame();
            }
            AppEvent::ShowToast(text) => {
                tui.show_toast(text);
            }
            AppEvent::CommitChanges { automatic } => {
                self.request_commit(automatic);
            }
            AppEvent::CommitChangesReady { automatic, changes } => {
                self.submit_commit_request(automatic, changes);
            }
            AppEvent::RemoteControlRequested(request) => {
                let tailnet = match request {
                    crate::remote_control::RemoteControlRequest::EnableLocal
                    | crate::remote_control::RemoteControlRequest::EnableTailnet
                    | crate::remote_control::RemoteControlRequest::Status => {
                        crate::remote_control::detect_tailnet_ipv4().await
                    }
                    crate::remote_control::RemoteControlRequest::EnableExplicit(_)
                    | crate::remote_control::RemoteControlRequest::Disable => {
                        Err("Tailscale was not queried.".to_string())
                    }
                };
                let outcome = self
                    .remote_control
                    .execute_request_with_detection(request, tailnet)
                    .await;
                match outcome {
                    crate::remote_control::RemoteControlOutcome::Report(report) => {
                        self.chat_widget.add_boxed_history(Box::new(
                            crate::history_cell::new_remote_control_event(report.lines()),
                        ));
                    }
                    crate::remote_control::RemoteControlOutcome::ConfirmationRequired(addr) => {
                        self.chat_widget.open_remote_control_confirmation(addr);
                    }
                    crate::remote_control::RemoteControlOutcome::Error(message) => {
                        self.chat_widget.add_error_message(message);
                    }
                }
            }
            AppEvent::ConfirmRemoteControlExplicit(addr) => {
                let outcome = self.remote_control.confirm_explicit(addr).await;
                match outcome {
                    crate::remote_control::RemoteControlOutcome::Report(report) => {
                        self.chat_widget.add_boxed_history(Box::new(
                            crate::history_cell::new_remote_control_event(report.lines()),
                        ));
                    }
                    crate::remote_control::RemoteControlOutcome::Error(message) => {
                        self.chat_widget.add_error_message(message);
                    }
                    crate::remote_control::RemoteControlOutcome::ConfirmationRequired(_) => {
                        unreachable!("a confirmed address cannot request confirmation again")
                    }
                }
            }
            AppEvent::AgentPrepared {
                generation,
                agent,
                intent,
            } => {
                if let crate::app_event::AgentPrepareIntent::Candidate {
                    agent_name,
                    display_name,
                } = intent
                {
                    let is_current = matches!(
                        &self.candidate_agent,
                        Some(CandidateAgent::Preparing {
                            generation: current,
                            agent_name: current_agent,
                            ..
                        }) if *current == generation && *current_agent == agent_name
                    );
                    if !is_current {
                        if let Ok(agent) = agent {
                            tokio::spawn(agent.shutdown());
                        }
                        return Ok(true);
                    }

                    match agent {
                        Ok(agent) => {
                            let Some(CandidateAgent::Preparing {
                                agent_name,
                                speculative,
                                ..
                            }) = self.candidate_agent.take()
                            else {
                                unreachable!("candidate generation checked above")
                            };
                            self.candidate_agent = Some(CandidateAgent::Prepared {
                                agent_name,
                                display_name,
                                agent: Box::new(agent),
                                speculative,
                            });
                            // A warmed agent waits to be chosen; it starts nothing on its own.
                            if !speculative {
                                if self.pending_candidate_model.is_some() {
                                    self.app_event_tx.send(AppEvent::NewSession);
                                } else {
                                    self.follow_up_prepared_candidate();
                                }
                            }
                        }
                        // Warming that failed says nothing yet: choosing the agent tries again
                        // and reports what goes wrong.
                        Err(_)
                            if matches!(
                                &self.candidate_agent,
                                Some(CandidateAgent::Preparing {
                                    speculative: true,
                                    ..
                                })
                            ) =>
                        {
                            self.candidate_agent = None;
                        }
                        Err(error) => {
                            self.candidate_agent = None;
                            self.pending_candidate_model = None;
                            let model_selection = self.model_route_fallback.is_some();
                            if crate::app::model_routes::eligible_error(&error)
                                && self.advance_model_route().await
                            {
                                return Ok(true);
                            }
                            self.model_route_fallback = None;
                            if self.fallback.is_some() {
                                self.advance_fallback().await;
                                return Ok(true);
                            }
                            if model_selection {
                                self.manual_model_handoff = None;
                                tui.show_toast(format!("Couldn't select model: {error}"));
                                return Ok(true);
                            }
                            self.chat_widget.set_login_agent_override(Some(agent_name));
                            self.chat_widget.add_error_message(format!(
                                "Couldn't prepare {display_name}: {error}"
                            ));
                        }
                    }
                    tui.frame_requester().schedule_frame();
                    return Ok(true);
                }

                let is_current = matches!(
                    &self.primary_agent_preparation,
                    Some(preparation) if preparation.generation == generation
                );
                if !is_current {
                    if let Ok(agent) = agent {
                        tokio::spawn(agent.shutdown());
                    }
                    return Ok(true);
                }
                let Some(primary_preparation) = self.primary_agent_preparation.as_ref() else {
                    return Ok(true);
                };
                let prepare_intent = primary_preparation.intent.clone();
                let prepared_initial_context = primary_preparation.initial_context.clone();
                self.primary_agent_preparation = None;

                match agent {
                    Ok(agent) => {
                        let automatic_resume = super::session_setup::automatic_resume_event(
                            agent.automatic_session_id(),
                        );
                        // Seed the deferred widget with the prepared agent
                        // capabilities so capability-gated behavior (e.g. the
                        // detach wording on quit) is right before any session
                        // exists.
                        self.chat_widget.handle_client_event(
                            crate::presentation::ClientEvent::SessionCapabilitiesChanged(
                                crate::presentation::SessionCapabilitiesView {
                                    agent: crate::presentation::AgentCapabilitiesView {
                                        http_mcp: agent.capabilities().mcp_capabilities.http,
                                        load_session: agent.capabilities().load_session,
                                        session_list: agent
                                            .capabilities()
                                            .session_capabilities
                                            .list
                                            .is_some(),
                                        session_resume: agent
                                            .capabilities()
                                            .session_capabilities
                                            .resume
                                            .is_some(),
                                        session_close: agent
                                            .capabilities()
                                            .session_capabilities
                                            .close
                                            .is_some(),
                                        session_fork: agent
                                            .capabilities()
                                            .session_capabilities
                                            .fork
                                            .is_some(),
                                    },
                                    ..Default::default()
                                },
                            ),
                        );
                        let can_resume_catalog = agent.capabilities().load_session
                            || agent.capabilities().session_capabilities.resume.is_some();
                        let sessions = match agent.catalog() {
                            nori_harness::runtime::SessionCatalog::Unsupported => None,
                            nori_harness::runtime::SessionCatalog::Listed(sessions) => {
                                Some(sessions.clone())
                            }
                        };
                        self.prepared_agent = Some(agent);
                        self.prepared_agent_initial_context = prepared_initial_context;
                        if let Some(event) = automatic_resume {
                            self.pending_session_activation = None;
                            self.app_event_tx.send(event);
                        } else {
                            if let Some(activation) = self.pending_session_activation.take() {
                                match activation {
                                    PendingSessionActivation::New => {
                                        self.app_event_tx.send(AppEvent::NewSession)
                                    }
                                    // The prepared agent waits for the transcript, which
                                    // activates it when it arrives.
                                    PendingSessionActivation::ResumeLoading => {
                                        self.pending_session_activation =
                                            Some(PendingSessionActivation::ResumeLoading);
                                    }
                                    PendingSessionActivation::Resume {
                                        acp_session_id,
                                        title,
                                        transcript,
                                    } => self.app_event_tx.send(AppEvent::ActivatePreparedResume {
                                        acp_session_id,
                                        title,
                                        transcript: transcript.map(|transcript| *transcript),
                                    }),
                                }
                                tui.frame_requester().schedule_frame();
                                return Ok(true);
                            }
                            match prepare_intent {
                                crate::app_event::AgentPrepareIntent::Idle => {}
                                crate::app_event::AgentPrepareIntent::Onboarding => {
                                    if let Some(event) = sessions
                                        .and_then(super::session_setup::onboarding_resume_event)
                                    {
                                        self.app_event_tx.send(event);
                                    } else {
                                        self.app_event_tx.send(AppEvent::NewSession);
                                    }
                                }
                                crate::app_event::AgentPrepareIntent::Picker {
                                    fallback_to_spawn,
                                } => match sessions {
                                    Some(sessions) => self
                                        .chat_widget
                                        .show_acp_resume_session_picker(sessions, false),
                                    None if fallback_to_spawn => {
                                        self.app_event_tx.send(AppEvent::NewSession)
                                    }
                                    None => self
                                        .chat_widget
                                        .show_acp_resume_session_picker(Vec::new(), false),
                                },
                                crate::app_event::AgentPrepareIntent::ResumePicker => {
                                    if can_resume_catalog && let Some(sessions) = sessions {
                                        self.chat_widget
                                            .show_acp_resume_session_picker(sessions, false);
                                    } else {
                                        self.chat_widget.open_local_resume_session_picker();
                                    }
                                }
                                crate::app_event::AgentPrepareIntent::Candidate { .. } => {
                                    unreachable!("candidate handled above")
                                }
                            }
                        }
                    }
                    Err(error) => {
                        self.prepared_agent_initial_context = None;
                        self.deferred_spawn_pending = false;
                        self.chat_widget
                            .add_error_message(format!("Couldn't prepare agent: {error}"));
                        self.chat_widget.add_info_message(
                            "No session is active - /resume retries the picker, /new starts a \
                             fresh session."
                                .to_string(),
                            None,
                        );
                        if !self.cloud_mode && self.candidate_agent.is_none() {
                            self.chat_widget
                                .open_agent_recovery_popup(&format!("{error:#}"));
                        }
                    }
                }
                tui.frame_requester().schedule_frame();
            }
            AppEvent::ActivatePreparedResume {
                acp_session_id,
                title,
                transcript,
            } => {
                if self.prepared_agent.is_none() {
                    self.pending_session_activation = Some(PendingSessionActivation::Resume {
                        acp_session_id,
                        title,
                        transcript: transcript.map(Box::new),
                    });
                    self.begin_agent_preparation(crate::app_event::AgentPrepareIntent::Idle);
                    return Ok(true);
                }
                // The activation stays pending, transcript and all, unless the agent is ready.
                self.pending_session_activation = Some(PendingSessionActivation::Resume {
                    acp_session_id: acp_session_id.clone(),
                    title: title.clone(),
                    transcript: transcript.map(Box::new),
                });
                let agent = match self.take_refreshed_prepared_agent() {
                    Ok(Some(agent)) => agent,
                    Ok(None) => return Ok(true),
                    Err(error) => {
                        self.chat_widget
                            .add_error_message(format!("Couldn't resume prepared agent: {error}"));
                        return Ok(true);
                    }
                };
                let transcript =
                    take_pending_resume_transcript(&mut self.pending_session_activation);
                let (initial_prompt, initial_images) = self.chat_widget.take_initial_input();
                self.shutdown_current_conversation();
                let mut init = self.chat_widget_init(
                    tui.frame_requester(),
                    initial_prompt,
                    initial_images,
                    None,
                    false,
                    None,
                );
                init.prepared_agent = Some(agent);
                self.chat_widget =
                    ChatWidget::new_resumed_acp(init, acp_session_id, title, transcript);
                self.configure_new_chat_widget();
                tui.frame_requester().schedule_frame();
            }
            AppEvent::OpenAgentSessionPicker => {
                self.discard_candidate();
                self.pending_session_activation = None;
                if let Some(agent) = &self.prepared_agent {
                    let can_resume_catalog = agent.capabilities().load_session
                        || agent.capabilities().session_capabilities.resume.is_some();
                    match agent.catalog() {
                        nori_harness::runtime::SessionCatalog::Listed(sessions)
                            if can_resume_catalog =>
                        {
                            self.chat_widget
                                .show_acp_resume_session_picker(sessions.clone(), false);
                        }
                        _ => self.chat_widget.open_local_resume_session_picker(),
                    }
                } else {
                    self.begin_agent_preparation(
                        crate::app_event::AgentPrepareIntent::ResumePicker,
                    );
                }
            }
            AppEvent::BeginExit => {
                self.cancel_primary_agent_preparation();
                if let Some(agent) = self.prepared_agent.take() {
                    tokio::spawn(agent.shutdown());
                }
                self.prepared_agent_initial_context = None;
                self.discard_candidate();
                self.chat_widget.begin_exit();
            }
            AppEvent::RevealUserQuestion => {
                self.overlay = None;
                self.backtrack.response_focus = None;
                self.chat_widget.set_response_focus(false);
                self.end_response_view(tui);
                tui.request_full_repaint();
                tui.frame_requester().schedule_frame();
            }
            AppEvent::UpdateHistoryToolSnapshot {
                generation,
                snapshot,
            } => {
                let call_id = snapshot.call_id.clone();
                if generation == self.chat_widget.session_generation()
                    && let Some(index) =
                        super::tool_history::update(&mut self.transcript_cells, *snapshot)
                {
                    let hidden = self
                        .hidden_transcript_len
                        .and_then(|length| usize::try_from(length).ok())
                        .unwrap_or(0);
                    let visible_index = if self.native_history.cells().is_empty() {
                        index.checked_sub(hidden)
                    } else {
                        self.native_history.tool_index(&call_id)
                    };
                    if let Some(visible_index) = visible_index {
                        self.session_view
                            .invalidate_main_transcript_cell(visible_index);
                    }
                    let ready = self
                        .native_history
                        .replace_tool(&call_id, self.transcript_cells[index].clone());
                    self.emit_native_history_cells(tui, ready);
                    tui.frame_requester().schedule_frame();
                }
            }
            AppEvent::FinalizeHistoryTools {
                generation,
                call_ids,
            } => {
                if generation != self.chat_widget.session_generation() {
                    return Ok(true);
                }
                for call_id in call_ids {
                    if let Some(index) =
                        super::tool_history::fail_active(&mut self.transcript_cells, &call_id)
                    {
                        let hidden = self
                            .hidden_transcript_len
                            .and_then(|length| usize::try_from(length).ok())
                            .unwrap_or(0);
                        let visible_index = if self.native_history.cells().is_empty() {
                            index.checked_sub(hidden)
                        } else {
                            self.native_history.tool_index(&call_id)
                        };
                        if let Some(visible_index) = visible_index {
                            self.session_view
                                .invalidate_main_transcript_cell(visible_index);
                        }
                        let ready = self
                            .native_history
                            .replace_tool(&call_id, self.transcript_cells[index].clone());
                        self.emit_native_history_cells(tui, ready);
                    }
                }
                tui.frame_requester().schedule_frame();
            }
            AppEvent::InsertHistoryCell(cell) => {
                let cell: Arc<dyn HistoryCell> = cell.into();
                if let Some(Overlay::Transcript(t)) = &mut self.overlay {
                    t.insert_cell(cell.clone());
                    tui.frame_requester().schedule_frame();
                }
                self.transcript_cells.push(cell.clone());
                if self.replay_painted_from.is_none() {
                    let ready = self.native_history.push(cell);
                    self.emit_native_history_cells(tui, ready);
                }
                tui.frame_requester().schedule_frame();
            }
            AppEvent::ResponseRewindFinished {
                generation,
                cell,
                result,
            } => {
                if self.chat_widget.session_generation() == generation {
                    match result {
                        Ok(()) => {
                            if let Some(index) = self
                                .transcript_cells
                                .iter()
                                .position(|candidate| Arc::ptr_eq(candidate, &cell))
                            {
                                self.transcript_cells.truncate(index + 1);
                                let ready = self
                                    .native_history
                                    .retain_transcript(&self.transcript_cells);
                                self.emit_native_history_cells(tui, ready);
                                self.response_boundaries.retain(|(candidate, _)| {
                                    self.transcript_cells
                                        .iter()
                                        .any(|cell| Arc::ptr_eq(candidate, cell))
                                });
                                self.hidden_transcript_len = None;
                                // Existing primary-screen scrollback is terminal history. Keep it
                                // intact; subsequent responses continue from the rewound head.
                                tui.frame_requester().schedule_frame();
                            }
                        }
                        Err(error) => self
                            .chat_widget
                            .add_error_message(format!("Could not rewind conversation: {error}")),
                    }
                }
            }
            AppEvent::ResponseBoundary(boundary) => {
                if let Some(cell) = self.transcript_cells.iter().rev().find(|cell| {
                    (cell.as_any().is::<crate::history_cell::AgentMessageCell>()
                        && !cell.is_stream_continuation())
                        || cell.as_any().is::<crate::history_cell::AgentMarkdownCell>()
                }) {
                    self.response_boundaries.push((cell.clone(), boundary));
                }
            }
            AppEvent::HistoryReplay(crate::app_event::HistoryReplay::Started(source)) => {
                self.replay_painted_from = Some((self.transcript_cells.len(), source));
            }
            AppEvent::HistoryReplay(crate::app_event::HistoryReplay::Finished) => {
                self.paint_replayed_history(tui);
            }
            AppEvent::ConsolidateAgentMessage { source, cwd } => {
                let consolidation = crate::transcript_reflow::consolidate_agent_message_cells(
                    &mut self.transcript_cells,
                    source,
                    &cwd,
                );
                if consolidation.is_some() {
                    let ready = self
                        .native_history
                        .retain_transcript(&self.transcript_cells);
                    self.emit_native_history_cells(tui, ready);
                }
                if let Some((range, replacement)) = consolidation
                    && let Some(Overlay::Transcript(transcript)) = &mut self.overlay
                {
                    transcript.replace_cells(range, replacement);
                    tui.frame_requester().schedule_frame();
                }
                if self.transcript_reflow.take_stream_finish_reflow_needed() {
                    self.transcript_reflow.schedule_immediate();
                    tui.frame_requester().schedule_frame();
                }
            }
            AppEvent::SessionEvent { generation, event } => {
                if self.chat_widget.session_generation() == generation {
                    self.program_status.event(&event);
                }
                if self.chat_widget.session_generation() == generation
                    && let nori_protocol::SessionEvent::Acp(
                        nori_protocol::AcpEvent::ChildNotification(notification),
                    ) = &event
                    && matches!(
                        &notification.update,
                        nori_protocol::acp::v1::SessionUpdate::ConfigOptionUpdate(_)
                    )
                    && let crate::subagent_switcher::Target::Subagent(key) =
                        self.session_view.focused()
                    && self.chat_widget.subagent_session(key).as_ref()
                        == Some(&notification.session_id)
                {
                    self.refresh_subagent_controls(key);
                }
                if self.chat_widget.session_generation() == generation
                    && let nori_protocol::SessionEvent::Nori(
                        nori_protocol::NoriEvent::SessionStarted(started),
                    ) = &event
                {
                    self.active_transcript_id = started.transcript_id.clone();
                }
                if self.chat_widget.session_generation() == generation
                    && let nori_protocol::SessionEvent::Nori(
                        nori_protocol::NoriEvent::SubscriptionExhausted(exhausted),
                    ) = &event
                {
                    let already_retrying = self.fallback.is_some();
                    let recorder = if let Some(handle) = self.chat_widget.harness_handle() {
                        handle.conversation_recorder().await.ok().flatten()
                    } else {
                        None
                    };
                    let mut handoff = exhausted.clone();
                    if let Some(recorder) = &recorder {
                        match recorder.visible_context().await {
                            Ok(context) => handoff.context = Some(context),
                            Err(error) => {
                                tracing::warn!(%error, "Cannot project fallback conversation context");
                                return Ok(true);
                            }
                        }
                    }
                    if recorder.is_some() || already_retrying {
                        self.start_fallback(handoff, recorder).await;
                    }
                    if self.fallback.is_some() || already_retrying {
                        self.suppressed_fallback_errors
                            .insert(exhausted.request_id.clone());
                    }
                    return Ok(true);
                }
                if let nori_protocol::SessionEvent::Nori(nori_protocol::NoriEvent::RequestFailed(
                    failure,
                )) = &event
                    && let Some(id) = &failure.request_id
                    && self.suppressed_fallback_errors.remove(id)
                {
                    return Ok(true);
                }
                if self.chat_widget.session_generation() == generation
                    && matches!(
                        &event,
                        nori_protocol::SessionEvent::Nori(nori_protocol::NoriEvent::RequestFailed(
                            _
                        ))
                    )
                    && let Some(fallback) = self.fallback.take()
                {
                    self.chat_widget.add_error_message(fallback.original_error);
                }
                let candidate_matches = matches!(
                    &self.candidate_agent,
                    Some(CandidateAgent::Activating { widget, .. })
                        if widget.session_generation() == generation
                );
                if candidate_matches {
                    let started = match &event {
                        nori_protocol::SessionEvent::Nori(
                            nori_protocol::NoriEvent::SessionStarted(started),
                        ) => Some(started.clone()),
                        _ => None,
                    };
                    let ended_message = if let Some(CandidateAgent::Activating {
                        widget,
                        activation_error,
                        activation_route_eligible,
                        ..
                    }) = self.candidate_agent.as_mut()
                    {
                        if let nori_protocol::SessionEvent::Acp(
                            nori_protocol::AcpEvent::Response {
                                response: Err(error),
                                ..
                            },
                        ) = &event
                        {
                            *activation_route_eligible |=
                                crate::app::model_routes::eligible_acp_error(error);
                        }
                        match classify_candidate_activation_event(&event, activation_error) {
                            CandidateActivationEvent::Forward => {
                                widget.handle_session_event(generation, event);
                                None
                            }
                            CandidateActivationEvent::CapturedError => None,
                            CandidateActivationEvent::Ended(message) => {
                                widget.handle_session_event(generation, event);
                                Some(message)
                            }
                        }
                    } else {
                        None
                    };

                    if let Some(started) = started {
                        if let Some(handoff) = &self.manual_model_handoff {
                            let still_current = self.chat_widget.session_generation()
                                == handoff.source_generation
                                && !self.chat_widget.is_task_running();
                            let context = handoff.recorder.visible_context().await;
                            if !still_current
                                || context.as_deref().ok() != Some(handoff.context.as_str())
                            {
                                self.discard_candidate();
                                self.manual_model_handoff = None;
                                self.model_route_fallback = None;
                                if self.native_preference_in_progress {
                                    self.native_preference_in_progress = false;
                                    if let Some(journal) = &self.active_transcript_id {
                                        self.native_preference_attempted.remove(journal);
                                    }
                                    self.prefer_native_current_model().await;
                                } else {
                                    tui.show_toast(
                                        "Conversation changed; choose the model again".to_owned(),
                                    );
                                }
                                return Ok(true);
                            }
                        }
                        if let Some((agent, model)) = self.pending_candidate_model.take() {
                            let candidate = match &self.candidate_agent {
                                Some(CandidateAgent::Activating {
                                    agent_name, widget, ..
                                }) if *agent_name == agent => widget.harness_handle(),
                                _ => None,
                            };
                            let config_id = candidate_model_config(&started.config_options, &model);
                            let result = match (candidate.as_ref(), config_id) {
                                (Some(handle), Some(config_id)) => handle
                                    .set_session_config_option(config_id, model.clone())
                                    .await
                                    .map_err(|error| {
                                        (
                                            crate::app::model_routes::eligible_error(&error),
                                            error.to_string(),
                                        )
                                    }),
                                _ => Err((
                                    true,
                                    format!("{agent} no longer advertises model {model}"),
                                )),
                            };
                            match result {
                                Ok(options) => {
                                    if let Some(effort) = self
                                        .manual_model_handoff
                                        .as_ref()
                                        .and_then(|handoff| handoff.effort.clone())
                                    {
                                        let effort_option = options.iter().find(|option| {
                                            crate::nori::agent_config_state::is_effort_option(
                                                option,
                                            )
                                        });
                                        let result = match (candidate.as_ref(), effort_option) {
                                            (Some(handle), Some(option)) => {
                                                handle
                                                    .set_session_config_option(
                                                        option.id.to_string(),
                                                        effort,
                                                    )
                                                    .await
                                            }
                                            _ => Err(anyhow::anyhow!(
                                                "The selected model does not expose the current effort setting"
                                            )),
                                        };
                                        match result {
                                            Ok(options) => {
                                                if let Some(CandidateAgent::Activating {
                                                    widget,
                                                    ..
                                                }) = self.candidate_agent.as_mut()
                                                {
                                                    widget
                                                        .sync_acp_session_config_snapshot(&options);
                                                }
                                            }
                                            Err(error) => {
                                                self.discard_candidate();
                                                if self.advance_model_route().await {
                                                    return Ok(true);
                                                }
                                                self.manual_model_handoff = None;
                                                tui.show_toast(format!(
                                                    "Couldn't retain effort setting: {error}"
                                                ));
                                                return Ok(true);
                                            }
                                        }
                                    } else if let Some(CandidateAgent::Activating {
                                        widget, ..
                                    }) = self.candidate_agent.as_mut()
                                    {
                                        widget.sync_acp_session_config_snapshot(&options);
                                    }
                                }
                                Err((eligible, error)) => {
                                    self.discard_candidate();
                                    let model_selection = self.model_route_fallback.is_some();
                                    if eligible && self.advance_model_route().await {
                                        return Ok(true);
                                    }
                                    self.model_route_fallback = None;
                                    if self.fallback.is_some() {
                                        self.advance_fallback().await;
                                        return Ok(true);
                                    }
                                    if model_selection {
                                        self.manual_model_handoff = None;
                                        tui.show_toast(format!("Couldn't select model: {error}"));
                                        return Ok(true);
                                    }
                                    self.chat_widget.add_error_message(format!(
                                        "Couldn't select model {model} for {agent}: {error}"
                                    ));
                                    return Ok(true);
                                }
                            }
                        }
                        if self.fallback.is_some() || self.manual_model_handoff.is_some() {
                            self.note_fallback("prepared").await;
                            let handle = match &self.candidate_agent {
                                Some(CandidateAgent::Activating { widget, .. }) => {
                                    widget.harness_handle()
                                }
                                _ => None,
                            };
                            let result = match handle {
                                Some(handle) => handle.record_backend_bound().await,
                                None => {
                                    Err(anyhow::anyhow!("Fallback candidate has no active session"))
                                }
                            };
                            if let Err(error) = result {
                                tracing::warn!(%error, "Failed to record fallback backend binding");
                                self.discard_candidate();
                                if self.fallback.is_some() {
                                    self.advance_fallback().await;
                                } else {
                                    self.manual_model_handoff = None;
                                    self.model_route_fallback = None;
                                    tui.show_toast("Couldn't save the model switch".to_owned());
                                }
                                return Ok(true);
                            }
                            if let Some(handle) = self.chat_widget.harness_handle()
                                && let Err(error) = handle.retire_handoff().await
                            {
                                tracing::warn!(%error, "Failed to retire previous backend recorder");
                                if self.manual_model_handoff.is_some() {
                                    if let Err(error) = handle.record_backend_bound().await {
                                        tracing::error!(%error, "Failed to restore the original backend binding");
                                    }
                                    self.discard_candidate();
                                    self.manual_model_handoff = None;
                                    self.model_route_fallback = None;
                                    tui.show_toast("Couldn't finish the model switch".to_owned());
                                    return Ok(true);
                                }
                            }
                        }
                        let Some(CandidateAgent::Activating {
                            agent_name, widget, ..
                        }) = self.candidate_agent.take()
                        else {
                            unreachable!("candidate generation checked above")
                        };
                        self.model_route_fallback = None;
                        let manual_handoff = self.manual_model_handoff.take().is_some();
                        self.native_preference_session_ready = false;
                        self.native_preference_session = None;
                        self.native_preference_in_progress = false;
                        if self.fallback.is_some() || manual_handoff {
                            self.chat_widget.flush_active_cell();
                        }
                        let mut old_widget = std::mem::replace(&mut self.chat_widget, *widget);
                        let draft = manual_handoff.then(|| old_widget.take_composer_draft());
                        self.session_card_shown = true;
                        self.config.active_agent = agent_name.clone();
                        self.config.agent = agent_name;
                        self.configure_new_chat_widget();
                        if let Some(draft) = draft {
                            self.chat_widget.restore_composer_draft(draft);
                        }
                        self.program_status
                            .event(&nori_protocol::SessionEvent::Nori(
                                nori_protocol::NoriEvent::SessionStarted(started.clone()),
                            ));
                        if let Some(handle) = self.chat_widget.harness_handle()
                            && let Err(error) = self
                                .remote_control
                                .attach_started(handle, self.config.nori_home.clone(), started)
                                .await
                        {
                            tracing::warn!(%error, "failed to attach committed candidate to remote ACP host");
                        }
                        if !manual_handoff {
                            let (initial_prompt, initial_images) = old_widget.take_initial_input();
                            self.chat_widget
                                .submit_launch_input(initial_prompt, initial_images);
                        }
                        self.cancel_primary_agent_preparation();
                        if let Some(agent) = self.prepared_agent.take() {
                            tokio::spawn(agent.shutdown());
                        }
                        self.prepared_agent_initial_context = None;
                        old_widget.shutdown_harness_session();
                        if let Some(fallback) = &self.fallback {
                            self.chat_widget.retry_echoed_prompt(
                                fallback.content.clone(),
                                fallback.echo_id.clone(),
                            );
                        }
                        if self.fallback.is_none()
                            && let Err(error) = ConfigEditsBuilder::new(&self.config.nori_home)
                                .set_agent(&self.config.active_agent)
                                .apply()
                                .await
                        {
                            tracing::error!(%error, "failed to persist activated agent selection");
                        }
                        self.chat_widget.show_agent_in_footer();
                    } else if let Some(message) = ended_message {
                        self.pending_candidate_model = None;
                        let model_selection = self.model_route_fallback.is_some();
                        let candidate = self.candidate_agent.take();
                        if let Some(CandidateAgent::Activating {
                            agent_name,
                            widget,
                            activation_route_eligible,
                            ..
                        }) = candidate
                        {
                            widget.shutdown_harness_session();
                            if activation_route_eligible && self.advance_model_route().await {
                                return Ok(true);
                            }
                            self.model_route_fallback = None;
                            self.chat_widget.set_login_agent_override(Some(agent_name));
                        }
                        if self.fallback.is_some() {
                            self.advance_fallback().await;
                            return Ok(true);
                        }
                        if model_selection {
                            self.manual_model_handoff = None;
                            tui.show_toast(format!("Couldn't select model: {message}"));
                            return Ok(true);
                        }
                        self.chat_widget.add_error_message(message);
                    }
                } else {
                    let current_session = self.chat_widget.session_generation() == generation;
                    let session_started = current_session
                        && matches!(
                            &event,
                            nori_protocol::SessionEvent::Nori(
                                nori_protocol::NoriEvent::SessionStarted(_)
                            )
                        );
                    if session_started {
                        self.native_preference_waiting_for_prompt =
                            self.chat_widget.has_initial_input();
                        if let nori_protocol::SessionEvent::Nori(
                            nori_protocol::NoriEvent::SessionStarted(started),
                        ) = &event
                        {
                            self.native_preference_session =
                                Some((generation, started.acp_session_id.clone()));
                        }
                        self.native_preference_session_ready = false;
                    }
                    let session_ready = current_session
                        && matches!(&event,
                        nori_protocol::SessionEvent::Nori(nori_protocol::NoriEvent::SessionReady(id))
                            if self.native_preference_session.as_ref() == Some(&(generation, id.clone())));
                    if session_ready {
                        self.native_preference_session_ready = true;
                    }
                    let prompt_finished = current_session
                        && matches!(
                            &event,
                            nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::Response {
                                response: Ok(
                                    nori_protocol::acp::v1::AgentResponse::PromptResponse(_)
                                ),
                                ..
                            }) | nori_protocol::SessionEvent::Nori(
                                nori_protocol::NoriEvent::RequestFailed(_)
                            )
                        );
                    if prompt_finished {
                        self.native_preference_waiting_for_prompt = false;
                    }
                    let check_native_preference = session_ready
                        || prompt_finished
                        || current_session
                            && matches!(&event,
                        nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::Notification(nori_protocol::acp::v1::AgentNotification::SessionNotification(notification)))
                            if matches!(&notification.update, nori_protocol::acp::v1::SessionUpdate::ConfigOptionUpdate(_)));
                    if self.chat_widget.session_generation() == generation
                        && matches!(
                            &event,
                            nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::Response {
                                response: Ok(
                                    nori_protocol::acp::v1::AgentResponse::PromptResponse(_)
                                ),
                                ..
                            })
                        )
                    {
                        self.fallback = None;
                    }
                    let started = match &event {
                        nori_protocol::SessionEvent::Nori(
                            nori_protocol::NoriEvent::SessionStarted(started),
                        ) if self.chat_widget.session_generation() == generation => {
                            Some(started.clone())
                        }
                        _ => None,
                    };
                    self.chat_widget.handle_session_event(generation, event);
                    // That session has written its card, if it was to; later ones do not repeat it.
                    self.session_card_shown |= started.is_some();
                    if check_native_preference {
                        self.prefer_native_current_model().await;
                    }
                    if let Some(started) = started
                        && let Some(handle) = self.chat_widget.harness_handle()
                        && let Err(error) = self
                            .remote_control
                            .attach_started(handle, self.config.nori_home.clone(), started)
                            .await
                    {
                        tracing::warn!(%error, "failed to attach session to remote ACP host");
                    }
                }
            }
            AppEvent::ConversationHistory(ev) => {
                self.on_conversation_history_for_backtrack(tui, ev)?;
            }
            AppEvent::ExitRequest => {
                let history_complete = self.finish_native_history(tui)?;
                self.overlay = None;
                self.chat_widget.clear_main_history_view();
                self.cancel_primary_agent_preparation();
                if let Some(agent) = self.prepared_agent.take() {
                    agent.shutdown().await;
                }
                self.prepared_agent_initial_context = None;
                self.discard_candidate();
                // Create and insert exit message cell before exiting
                let exit_cell = self.chat_widget.create_exit_message_cell();

                // Insert the cell directly (inline the InsertHistoryCell logic to avoid recursion)
                let cell: Arc<dyn HistoryCell> = exit_cell.into();
                if let Some(Overlay::Transcript(t)) = &mut self.overlay {
                    t.insert_cell(cell.clone());
                }
                self.transcript_cells.push(cell.clone());
                let mut display = if history_complete {
                    cell.native_lines(tui.terminal.last_known_screen_size.width)
                } else {
                    Vec::new()
                };
                if !display.is_empty() {
                    if !cell.is_stream_continuation() {
                        if self.has_emitted_history_lines {
                            display.insert(0, Line::from(""));
                        } else {
                            self.has_emitted_history_lines = true;
                        }
                    }
                    if self.overlay.is_some() {
                        self.deferred_history_lines.extend(display);
                    } else {
                        tui.insert_history_lines(display);
                    }
                }

                // Force immediate synchronous draw to flush all history lines to scrollback
                // This will temporarily show the bottom pane in the viewport
                tui.draw(
                    self.chat_widget.desired_height(tui.terminal.size()?.width),
                    0,
                    |frame| {
                        self.chat_widget.render(frame.area(), frame.buffer);
                        if let Some((x, y)) = self.chat_widget.cursor_pos(frame.area()) {
                            frame.set_cursor_position((x, y));
                        }
                    },
                )?;

                // Clear the viewport to remove the bottom pane, but keep scrollback intact
                tui.terminal.clear()?;

                // Exit the application
                return Ok(false);
            }
            AppEvent::HarnessAction(action) => {
                if let crate::app_event::HarnessAction::RespondToAgent { request_id, .. } = &action
                {
                    self.program_status.answered(request_id);
                }
                self.chat_widget.submit_harness_action(action);
            }
            AppEvent::HistoryEntryLoaded {
                log_id,
                offset,
                entry,
            } => self
                .chat_widget
                .on_history_entry_loaded(log_id, offset, entry),
            AppEvent::HistorySearchLoaded(entries) => {
                self.chat_widget.on_history_search_loaded(entries);
            }
            AppEvent::CustomPromptsLoaded(prompts) => {
                self.chat_widget.on_custom_prompts_loaded(prompts);
            }
            AppEvent::UndoSnapshotsLoaded(snapshots) => {
                self.chat_widget.on_undo_snapshots_loaded(snapshots);
            }
            AppEvent::GoalLoaded(Some(goal)) => self.chat_widget.handle_thread_goal_updated(goal),
            AppEvent::GoalLoaded(None) => self.chat_widget.handle_thread_goal_cleared(),
            AppEvent::HarnessActionFailed(message) => self.chat_widget.add_error_message(message),
            AppEvent::PromptSubmitFailed(message) => {
                if let Some(fallback) = self.fallback.take() {
                    self.chat_widget.add_error_message(fallback.original_error);
                }
                self.chat_widget.on_prompt_submit_failed(message);
            }
            AppEvent::DiffResult(text) => {
                // Clear the in-progress state in the bottom pane
                self.chat_widget.on_diff_complete();
                // Build the pager lines on the primary screen.
                let pager_lines: Vec<ratatui::text::Line<'static>> = if text.trim().is_empty() {
                    vec!["No changes detected.".italic().into()]
                } else {
                    text.lines().map(ansi_escape_line).collect()
                };
                self.overlay = Some(Overlay::new_static_with_lines(
                    pager_lines,
                    "D I F F".to_string(),
                ));
                tui.frame_requester().schedule_frame();
            }
            AppEvent::StartFileSearch(query) => {
                if !query.is_empty() {
                    self.file_search.on_user_query(query);
                }
            }
            AppEvent::FileSearchResult { query, matches } => {
                self.chat_widget.apply_file_search_result(query, matches);
            }
            AppEvent::SystemInfoRefreshed(info) => {
                self.chat_widget.apply_system_info_refresh(info);
            }
            AppEvent::RefreshSystemInfoForDirectory { dir, agent } => {
                self.request_system_info_refresh(dir, agent, self.chat_widget.first_prompt_text());
            }
            AppEvent::OpenFullAccessConfirmation { preset } => {
                self.chat_widget.open_full_access_confirmation(preset);
            }
            AppEvent::OpenWorldWritableWarningConfirmation {
                preset,
                sample_paths,
                extra_count,
                failed_scan,
            } => {
                self.chat_widget.open_world_writable_warning_confirmation(
                    preset,
                    sample_paths,
                    extra_count,
                    failed_scan,
                );
            }
            AppEvent::OpenWindowsSandboxEnablePrompt { preset } => {
                self.chat_widget.open_windows_sandbox_enable_prompt(preset);
            }
            AppEvent::EnableWindowsSandboxForAgentMode { preset } => {
                #[cfg(target_os = "windows")]
                {
                    match ConfigEditsBuilder::new(&self.config.nori_home)
                        .set_path(&["features", "enable_experimental_windows_sandbox"], true)
                        .apply()
                        .await
                    {
                        Ok(()) => {
                            codex_sandbox::set_windows_sandbox_enabled(true);
                            self.config.windows_sandbox_enabled = true;
                            self.config.forced_auto_mode_downgraded_on_windows = false;
                            self.chat_widget.clear_forced_auto_mode_downgrade();
                            if let Some((sample_paths, extra_count, failed_scan)) =
                                self.chat_widget.world_writable_warning_details()
                            {
                                self.app_event_tx.send(
                                    AppEvent::OpenWorldWritableWarningConfirmation {
                                        preset: Some(preset.clone()),
                                        sample_paths,
                                        extra_count,
                                        failed_scan,
                                    },
                                );
                            } else {
                                self.app_event_tx.send(AppEvent::ApplyApprovalPreset {
                                    approval: preset.approval,
                                    sandbox: preset.sandbox.clone(),
                                });
                                self.chat_widget.add_info_message(
                                    "Enabled experimental Windows sandbox.".to_string(),
                                    None,
                                );
                            }
                        }
                        Err(err) => {
                            tracing::error!(
                                error = %err,
                                "failed to enable Windows sandbox feature"
                            );
                            self.chat_widget.add_error_message(format!(
                                "Failed to enable the Windows sandbox feature: {err}"
                            ));
                        }
                    }
                }
                #[cfg(not(target_os = "windows"))]
                {
                    let _ = preset;
                }
            }
            AppEvent::ApplyApprovalPreset { approval, sandbox } => {
                #[cfg(target_os = "windows")]
                let sandbox_is_workspace_write_or_ro = matches!(
                    sandbox,
                    nori_config::SandboxPolicy::WorkspaceWrite { .. }
                        | nori_config::SandboxPolicy::ReadOnly
                );

                self.apply_approval_preset(approval, sandbox);

                // If sandbox policy becomes workspace-write or read-only, run the Windows world-writable scan.
                #[cfg(target_os = "windows")]
                {
                    // One-shot suppression if the user just confirmed continue.
                    if self.skip_world_writable_scan_once {
                        self.skip_world_writable_scan_once = false;
                        return Ok(true);
                    }

                    let should_check = codex_sandbox::get_platform_sandbox().is_some()
                        && sandbox_is_workspace_write_or_ro
                        && !self.chat_widget.world_writable_warning_hidden();
                    if should_check {
                        let cwd = self.config.cwd.clone();
                        let env_map: std::collections::HashMap<String, String> =
                            std::env::vars().collect();
                        let tx = self.app_event_tx.clone();
                        let logs_base_dir = self.config.nori_home.clone();
                        let sandbox_policy = self.config.sandbox_policy.clone();
                        Self::spawn_world_writable_scan(
                            cwd,
                            env_map,
                            logs_base_dir,
                            sandbox_policy,
                            tx,
                        );
                    }
                }
            }
            AppEvent::SkipNextWorldWritableScan => {
                self.skip_world_writable_scan_once = true;
            }
            AppEvent::UpdateFullAccessWarningAcknowledged(ack) => {
                self.chat_widget.set_full_access_warning_acknowledged(ack);
            }
            AppEvent::UpdateWorldWritableWarningAcknowledged(ack) => {
                self.chat_widget
                    .set_world_writable_warning_acknowledged(ack);
            }
            AppEvent::PersistFullAccessWarningAcknowledged => {
                if let Err(err) = ConfigEditsBuilder::new(&self.config.nori_home)
                    .set_path(&["notice", "hide_full_access_warning"], true)
                    .apply()
                    .await
                {
                    tracing::error!(
                        error = %err,
                        "failed to persist full access warning acknowledgement"
                    );
                    self.chat_widget.add_error_message(format!(
                        "Failed to save full access confirmation preference: {err}"
                    ));
                }
            }
            AppEvent::PersistWorldWritableWarningAcknowledged => {
                if let Err(err) = ConfigEditsBuilder::new(&self.config.nori_home)
                    .set_path(&["notice", "hide_world_writable_warning"], true)
                    .apply()
                    .await
                {
                    tracing::error!(
                        error = %err,
                        "failed to persist world-writable warning acknowledgement"
                    );
                    self.chat_widget.add_error_message(format!(
                        "Failed to save Agent mode warning preference: {err}"
                    ));
                }
            }
            AppEvent::OpenApprovalsPopup => {
                self.chat_widget.open_approvals_popup();
            }
            AppEvent::FullScreenApprovalRequest(request) => {
                let ApprovalRequest {
                    kind,
                    cwd,
                    snapshot,
                    ..
                } = request;
                let edit_changes = if matches!(
                    kind,
                    crate::presentation::ToolKind::Create
                        | crate::presentation::ToolKind::Edit
                        | crate::presentation::ToolKind::Delete
                        | crate::presentation::ToolKind::Move
                ) {
                    let mut changes =
                        client_tool_cell::diff_changes_from_artifacts(&snapshot.artifacts, &cwd);
                    if changes.is_empty() {
                        changes =
                            client_tool_cell::changes_from_invocation(&snapshot.invocation, &cwd);
                    }
                    if changes.is_empty() {
                        None
                    } else {
                        Some(changes)
                    }
                } else {
                    None
                };

                if let Some(changes) = edit_changes {
                    let diff_summary = DiffSummary::new(changes, cwd);
                    self.overlay = Some(Overlay::new_static_with_renderables(
                        vec![diff_summary.into()],
                        "P A T C H".to_string(),
                    ));
                } else {
                    let cell = client_tool_cell::ClientToolCell::new(*snapshot, cwd, false);
                    let lines = cell
                        .expanded_lines(tui.terminal.size().map(|size| size.width).unwrap_or(80));
                    self.overlay =
                        Some(Overlay::new_static_with_lines(lines, "T O O L".to_string()));
                }
            }
            AppEvent::LoginProvider { agent_name } => {
                self.chat_widget
                    .handle_login_command_with_agent(&agent_name);
            }
            AppEvent::SelectModelRoutes {
                routes,
                value_name,
                origin,
            } => {
                self.select_model_routes(routes, value_name, origin).await;
            }
            AppEvent::HighlightAgentCandidate {
                agent_name,
                display_name,
            } => self.highlight_agent_candidate(agent_name, display_name),
            AppEvent::WarmAgentCandidate {
                agent_name,
                display_name,
            } => self.warm_agent_candidate(agent_name, display_name),
            AppEvent::DiscardWarmAgentCandidate => self.discard_warm_candidate(),
            AppEvent::CancelAgentCandidate => {
                self.discard_candidate();
                self.manual_model_handoff = None;
                self.model_route_fallback = None;
                self.chat_widget
                    .add_info_message("Agent switch cancelled.".to_string(), None);
            }
            AppEvent::AgentSpawnFailed { agent_name, error } => {
                tracing::warn!(
                    agent = %agent_name,
                    error = %error,
                    "Agent failed to spawn, opening agent picker"
                );

                self.chat_widget.on_agent_spawn_failed(&agent_name, &error);
            }
            AppEvent::AgentConnecting { display_name } => {
                tracing::info!(
                    display_name = %display_name,
                    "Agent connecting, showing status indicator"
                );
                self.chat_widget.show_connecting_status(&display_name);
            }
            AppEvent::OpenAcpModelPickerUnsupported => {
                self.chat_widget.open_model_unsupported_popup();
            }
            AppEvent::OpenAcpSessionConfigPicker {
                config_options,
                focus_config_id,
            } => {
                self.chat_widget
                    .open_acp_session_config_picker(config_options, focus_config_id);
            }
            AppEvent::OpenAcpSessionConfigValuePicker { option, origin } => {
                self.chat_widget
                    .open_acp_session_config_value_picker(option, origin);
            }
            AppEvent::OpenCustomModelInput {
                config_id,
                option_name,
                origin,
            } => {
                self.chat_widget
                    .open_custom_model_input(config_id, option_name, origin);
            }
            AppEvent::SetAcpSessionConfigOption {
                config_id,
                value,
                option_name,
                value_name,
                is_custom_model,
                origin,
                route_token,
            } => {
                if self.stale_model_route_token(route_token) {
                    return Ok(true);
                }
                if let crate::nori::session_config_picker::ConfigPickerOrigin::Child(key) = origin {
                    let roster = self.chat_widget.subagent_roster(std::time::Instant::now());
                    let target = roster.resolve(crate::subagent_switcher::Target::Subagent(key));
                    let crate::subagent_switcher::Target::Subagent(resolved_key) = target else {
                        return Ok(true);
                    };
                    if let (Some(parent), Some(session_id)) = (
                        self.chat_widget.harness_handle(),
                        self.chat_widget.subagent_session(resolved_key),
                    ) {
                        self.session_view.advance_controls_revision(resolved_key);
                        let tx = self.app_event_tx.clone();
                        tokio::spawn(async move {
                            let result = parent
                                .set_child_config_option(
                                    session_id,
                                    config_id.clone(),
                                    value.clone(),
                                )
                                .await;
                            let route_fallback_eligible = result
                                .as_ref()
                                .err()
                                .is_some_and(crate::app::model_routes::eligible_error);
                            let result = result.map_err(|error| error.to_string());
                            tx.send(AppEvent::SubagentConfigSetResult {
                                key,
                                result,
                                config_id,
                                value,
                                route_fallback_eligible,
                                route_token,
                            });
                        });
                    } else {
                        self.model_route_fallback = None;
                        tui.show_toast(
                            "Model controls are unavailable for this child session.".to_owned(),
                        );
                    }
                } else {
                    self.chat_widget.set_acp_session_config_option(
                        config_id,
                        value,
                        option_name,
                        value_name,
                        is_custom_model,
                        origin,
                        route_token,
                    );
                }
            }
            AppEvent::CycleModel(step) => {
                if self.session_view.focused() != crate::subagent_switcher::Target::Main {
                    tui.show_toast(
                        "Model controls are unavailable for this child session.".to_owned(),
                    );
                } else if !self.chat_widget.cycle_model(step)
                    && self
                        .waiting_model_presses
                        .press(step, std::time::Instant::now())
                {
                    self.chat_widget.add_info_message(
                        "The agent hasn't listed its models yet; this applies as soon as it does."
                            .to_owned(),
                        None,
                    );
                }
            }
            AppEvent::CycleEffort(step) => {
                if self.session_view.focused() != crate::subagent_switcher::Target::Main {
                    tui.show_toast(
                        "Effort controls are unavailable for this child session.".to_owned(),
                    );
                } else if !self.chat_widget.cycle_effort(step) {
                    self.chat_widget.add_info_message(
                        "The agent hasn't listed its options yet.".to_owned(),
                        None,
                    );
                }
            }
            AppEvent::AcpSessionConfigSetResult {
                success,
                agent,
                config_id,
                value,
                option_name,
                value_name: _,
                is_custom_model,
                origin,
                config_options,
                error,
                route_fallback_eligible,
                route_token,
            } => {
                if self.stale_model_route_token(route_token) {
                    return Ok(true);
                }
                if self.is_model_session_route(route_token, &config_id, &value) {
                    if !success && route_fallback_eligible && self.advance_model_route().await {
                        return Ok(true);
                    }
                    self.model_route_fallback = None;
                    self.manual_model_handoff = None;
                    self.native_preference_in_progress = false;
                }
                let cycled =
                    origin == crate::nori::session_config_picker::ConfigPickerOrigin::Cycle;
                if cycled {
                    if !success {
                        tui.show_toast(format!(
                            "Couldn't update {option_name}: {}",
                            error.as_deref().unwrap_or("Unknown error")
                        ));
                    }
                    self.chat_widget.on_model_cycle_result(if success {
                        Ok(config_options.clone())
                    } else {
                        Err(error.unwrap_or_else(|| "Unknown error".to_string()))
                    });
                    if success
                        && let Err(err) = self
                            .persist_default_model_selection(
                                &agent,
                                &config_id,
                                &value,
                                config_options.as_deref().unwrap_or_default(),
                            )
                            .await
                    {
                        tracing::error!(error = %err, "failed to persist default model selection");
                    }
                } else if success {
                    if let Some(config_options) = &config_options {
                        self.chat_widget
                            .sync_acp_session_config_snapshot(config_options);
                    }
                    if let Err(err) = self
                        .persist_default_model_selection(
                            &agent,
                            &config_id,
                            &value,
                            config_options.as_deref().unwrap_or_default(),
                        )
                        .await
                    {
                        tracing::error!(error = %err, "failed to persist default model selection");
                    }
                } else if is_custom_model
                    && nori_harness::get_agent_config(&agent)
                        .map(|config| config.supports_model_injection())
                        .unwrap_or(false)
                {
                    // The agent rejected this model from its live picker, but we
                    // can still run it: persist it as the default and restart so
                    // it is forced through the agent's spawn-time channel.
                    if self.persist_custom_default_model(&agent, &value).await {
                        self.app_event_tx.send(AppEvent::NewSession);
                    } else {
                        tui.show_toast("Couldn't save the model selection".to_owned());
                    }
                } else {
                    let error_msg = error.unwrap_or_else(|| "Unknown error".to_string());
                    tui.show_toast(format!("Couldn't update {option_name}: {error_msg}"));
                }
            }
            AppEvent::AcpSessionConfigSnapshot {
                generation,
                config_options,
            } => {
                self.chat_widget
                    .handle_acp_session_config_snapshot(generation, &config_options);
            }
            AppEvent::AcpModeConfigSnapshot { generation, mode } => {
                self.chat_widget
                    .apply_acp_mode_config_snapshot(generation, mode);
            }
            AppEvent::LoginStarted { target } => match target {
                Some(key) => {
                    if let Some(session) = self.chat_widget.subagent_session(key) {
                        self.program_status.auth(Some(&session));
                    }
                }
                None => self.program_status.auth(None),
            },
            AppEvent::LoginCancelled => self.program_status.finish_auth(),
            AppEvent::LoginComplete { success } => {
                self.program_status.finish_auth();
                self.chat_widget.handle_login_complete(success);
            }
            AppEvent::ExternalCliLoginOutput { data } => {
                self.chat_widget.handle_external_cli_login_output(data);
            }
            AppEvent::ExternalCliLoginComplete {
                success,
                agent_name,
            } => {
                self.program_status.finish_auth();
                self.chat_widget
                    .handle_external_cli_login_complete(success, agent_name);
            }
            AppEvent::SetConfigVerticalFooter(enabled) => {
                self.persist_config_setting("vertical_footer", enabled)
                    .await;
            }
            AppEvent::SetConfigTerminalNotifications(enabled) => {
                self.persist_notification_setting("terminal_notifications", enabled)
                    .await;
            }
            AppEvent::SetConfigOsNotifications(enabled) => {
                self.persist_notification_setting("os_notifications", enabled)
                    .await;
            }
            AppEvent::SetConfigHotkey { action, binding } => {
                self.persist_hotkey_setting(action, binding).await;
            }
            AppEvent::OpenHotkeyPicker => {
                self.chat_widget
                    .open_hotkey_picker(self.hotkey_config.clone());
            }
            AppEvent::OpenNotifyAfterIdlePicker => {
                self.chat_widget
                    .open_notify_after_idle_picker(self.config.notify_after_idle);
            }
            AppEvent::SetConfigNotifyAfterIdle(value) => {
                self.persist_notify_after_idle_setting(value).await;
            }
            AppEvent::OpenScriptTimeoutPicker => {
                self.chat_widget
                    .open_script_timeout_picker(self.config.script_timeout.clone());
            }
            AppEvent::SetConfigScriptTimeout(value) => {
                self.persist_script_timeout_setting(value).await;
            }
            AppEvent::OpenLoopCountPicker => {
                let current = match self.loop_count_override {
                    Some(overridden) => overridden,
                    None => self.config.loop_count,
                };
                self.chat_widget.open_loop_count_picker(current);
            }
            AppEvent::SetConfigLoopCount(value) => {
                self.set_session_loop_count(value);
            }
            AppEvent::OpenVimModePicker => {
                self.chat_widget
                    .open_vim_mode_picker(self.config.vim_mode, true);
            }
            AppEvent::OpenAutoWorktreePicker => {
                self.chat_widget
                    .open_auto_worktree_picker(self.config.auto_worktree);
            }
            AppEvent::SetConfigAutoWorktree(value) => {
                self.persist_auto_worktree_setting(value).await;
            }
            AppEvent::SetConfigSkillsetPerSession(enabled) => {
                self.persist_skillset_per_session_setting(enabled).await;
            }
            AppEvent::SetConfigPinnedPlanDrawer(enabled) => {
                self.persist_pinned_plan_drawer_setting(enabled).await;
            }
            AppEvent::SetConfigResizeReflow(enabled) => {
                self.persist_resize_reflow_setting(enabled, tui).await;
            }
            AppEvent::SetConfigAcpWireRecording(enabled) => {
                self.persist_acp_wire_recording_setting(enabled).await;
            }
            AppEvent::SetConfigCustomWorkingMessages(enabled) => {
                self.persist_custom_working_messages_setting(enabled).await;
            }
            AppEvent::OpenSkillsetPerSessionWorktreeChoice => {
                self.chat_widget.open_skillset_worktree_choice_picker();
            }
            AppEvent::OpenFooterSegmentsPicker => {
                self.chat_widget
                    .open_footer_segments_picker(&self.footer_segment_config);
            }
            AppEvent::SetConfigFooterSegment(segment, enabled) => {
                self.persist_footer_segment_setting(segment, enabled).await;
            }
            AppEvent::BrowseFiles(fm) => {
                self.browse_files(fm, tui);
            }
            AppEvent::SetConfigFileManager(value) => {
                self.persist_file_manager_setting(value).await;
            }
            AppEvent::OpenFileManagerPicker => {
                self.chat_widget
                    .open_file_manager_picker(self.config.file_manager);
            }
            AppEvent::LoopIteration {
                prompt,
                remaining,
                total,
            } => {
                let iteration = total - remaining;
                tracing::info!("Loop iteration {iteration}/{total} (remaining: {remaining})");

                self.shutdown_current_conversation();

                let init = self.chat_widget_init(
                    tui.frame_requester(),
                    Some(prompt),
                    Vec::new(),
                    None,
                    true,
                    None,
                );
                self.chat_widget = ChatWidget::new(init);
                self.configure_new_chat_widget();
                self.chat_widget.set_loop_state(remaining, total);
                self.pending_session_activation = Some(PendingSessionActivation::New);
                self.begin_agent_preparation(crate::app_event::AgentPrepareIntent::Idle);

                self.chat_widget
                    .add_info_message(format!("Loop iteration {iteration} of {total}"), None);
            }
            AppEvent::SetConfigVimMode {
                value,
                from_settings,
            } => {
                self.persist_vim_mode_setting(value, from_settings).await;
            }
            AppEvent::SkillsetListResult {
                names,
                error,
                install_dir,
            } => {
                self.chat_widget
                    .on_skillset_list_result(names, error, install_dir);
            }
            AppEvent::InstallSkillset { name } => {
                self.chat_widget.on_install_skillset_request(&name);
            }
            AppEvent::SwitchSkillset { name, install_dir } => {
                self.chat_widget
                    .on_switch_skillset_request(&name, &install_dir);
            }
            AppEvent::SkillsetInstallResult {
                name,
                success,
                message,
            } => {
                self.chat_widget
                    .on_skillset_install_result(&name, success, &message);
                self.on_skillset_applied(success);
            }
            AppEvent::SkillsetSwitchResult {
                name,
                success,
                message,
            } => {
                self.chat_widget
                    .on_skillset_switch_result(&name, success, &message);
                self.on_skillset_applied(success);
            }
            AppEvent::SkillsetPickerDismissed => {
                // The skillset picker was dismissed without selection. If the
                // agent spawn was deferred, spawn it now without a skillset
                // (behaves as if skillset_per_session is disabled) and activate
                // the session, matching plain local startup.
                if self.take_deferred_spawn() {
                    self.pending_session_activation = Some(PendingSessionActivation::New);
                    self.begin_agent_preparation(crate::app_event::AgentPrepareIntent::Idle);
                }
            }
            AppEvent::ExecuteScript { prompt, args } => {
                let tx = self.app_event_tx.clone();
                let timeout = self.config.script_timeout.as_duration();
                let name = prompt.name.clone();
                self.chat_widget
                    .add_info_message(format!("Running script '{name}'..."), None);
                tokio::spawn(async move {
                    let result =
                        nori_harness::custom_prompts::execute_script(&prompt, &args, timeout).await;
                    tx.send(AppEvent::ScriptExecutionComplete {
                        name: prompt.name.clone(),
                        result,
                    });
                });
            }
            AppEvent::ScriptExecutionComplete { name, result } => match result {
                Ok(stdout) => {
                    if stdout.trim().is_empty() {
                        self.chat_widget.add_info_message(
                            format!("Script '{name}' completed with no output."),
                            None,
                        );
                    } else {
                        let message = format!("Output from script '{name}':\n{stdout}");
                        self.chat_widget.queue_text_as_user_message(message);
                    }
                }
                Err(err) => {
                    self.chat_widget
                        .add_error_message(format!("Script '{name}' failed: {err}"));
                    let error_context =
                        format!("Script '{name}' failed with the following error:\n{err}");
                    self.chat_widget.queue_text_as_user_message(error_context);
                }
            },
            AppEvent::ShowReferenceSessionPicker { sessions, target } => {
                self.chat_widget
                    .show_reference_session_picker(sessions, target);
            }
            AppEvent::ReferenceSessionPreview {
                project_id,
                session_id,
                agent,
                preview,
            } => {
                self.chat_widget.update_reference_session_preview(
                    &project_id,
                    &session_id,
                    &agent,
                    &preview,
                );
            }
            AppEvent::LoadSessionReference {
                project_id,
                session_id,
                agent,
                target,
            } => {
                self.load_session_reference(project_id, session_id, agent, target);
            }
            AppEvent::InsertSessionReference { text, target } => {
                let current = self.session_view.focused();
                if target != current {
                    self.session_view
                        .save_draft(current, self.chat_widget.take_composer_draft());
                    if let Some(draft) = self.session_view.take_draft(target) {
                        self.chat_widget.restore_composer_draft(draft);
                    }
                }
                self.chat_widget.insert_str(&text);
                if target != current {
                    self.session_view
                        .save_draft(target, self.chat_widget.take_composer_draft());
                    if let Some(draft) = self.session_view.take_draft(current) {
                        self.chat_widget.restore_composer_draft(draft);
                    }
                }
            }
            AppEvent::ShowViewonlySessionPicker {
                sessions,
                nori_home,
            } => {
                let params = crate::nori::viewonly_session_picker::viewonly_session_picker_params(
                    sessions,
                    nori_home,
                    self.app_event_tx.clone(),
                );
                self.chat_widget.show_selection_view(params);
            }
            AppEvent::LoadViewonlyTranscript {
                nori_home,
                project_id,
                session_id,
            } => {
                let tx = self.app_event_tx.clone();
                tokio::spawn(async move {
                    let loader = nori_harness::transcript::TranscriptLoader::new(nori_home);
                    match loader.load_transcript(&project_id, &session_id).await {
                        Ok(transcript) => {
                            let entries =
                                crate::viewonly_transcript::transcript_to_entries(&transcript);
                            tx.send(AppEvent::DisplayViewonlyTranscript { entries });
                        }
                        Err(e) => {
                            tx.send(AppEvent::InsertHistoryCell(Box::new(
                                crate::history_cell::new_error_event(format!(
                                    "Failed to load transcript: {e}"
                                )),
                            )));
                        }
                    }
                });
            }
            AppEvent::DisplayViewonlyTranscript { entries } => {
                self.display_viewonly_transcript(entries);
            }
            AppEvent::ShowResumeSessionPicker {
                sessions,
                nori_home,
                generation,
            } => {
                let params =
                    crate::nori::resume_session_picker::resume_session_component_picker_params(
                        sessions, nori_home,
                    );
                self.chat_widget
                    .show_resume_session_picker(params, generation);
            }
            AppEvent::ShowAcpResumeSessionPicker { sessions } => {
                self.chat_widget
                    .show_acp_resume_session_picker(sessions, false);
            }
            AppEvent::ResumeSessionSummaryReady {
                generation,
                session_id,
                started_at,
                first_message_preview,
                user_turn_count,
            } => {
                self.chat_widget.update_resume_session_picker_item(
                    generation,
                    &session_id,
                    &started_at,
                    first_message_preview.as_deref(),
                    user_turn_count,
                );
            }
            AppEvent::ResumeSession {
                nori_home,
                project_id,
                session_id,
            } => {
                // Reading and parsing a long transcript must not hold up the UI: it loads on a
                // task and the resume continues when it arrives.
                self.chat_widget
                    .add_info_message("Loading session…".to_string(), None);
                let tx = self.app_event_tx.clone();
                tokio::spawn(async move {
                    let loader = nori_harness::transcript::TranscriptLoader::new(nori_home);
                    let transcript = loader
                        .load_transcript(&project_id, &session_id)
                        .await
                        .map(Box::new)
                        .map_err(|error| error.to_string());
                    tx.send(AppEvent::ResumeTranscriptLoaded { transcript });
                });
                tui.frame_requester().schedule_frame();
            }
            AppEvent::ResumeTranscriptLoaded { transcript } => match transcript {
                Ok(transcript) => {
                    let transcript = *transcript;
                    let acp_session_id = transcript.latest_acp_session_id();
                    let display_name =
                        crate::nori::agent_picker::get_agent_info(&self.config.active_agent)
                            .map(|info| info.display_name)
                            .unwrap_or_else(|| self.config.active_agent.clone());
                    let loaded_at_startup = matches!(
                        self.pending_session_activation,
                        Some(PendingSessionActivation::ResumeLoading)
                    );
                    if loaded_at_startup {
                        self.pending_session_activation = None;
                    }

                    if self.prepared_agent.is_none()
                        && loaded_at_startup
                        && self.primary_agent_preparation.is_some()
                    {
                        // The agent is still starting; it activates the resume once ready.
                        self.pending_session_activation = Some(PendingSessionActivation::Resume {
                            acp_session_id,
                            title: None,
                            transcript: Some(Box::new(transcript)),
                        });
                        return Ok(true);
                    }
                    if self.prepared_agent.is_none() {
                        self.defer_resume_activation(
                            tui.frame_requester(),
                            acp_session_id,
                            None,
                            Some(transcript),
                        );
                        self.chat_widget.add_info_message(
                            format!("Resuming session with {display_name}..."),
                            None,
                        );
                        tui.frame_requester().schedule_frame();
                        return Ok(true);
                    }

                    // The activation stays pending, transcript and all, unless the agent is
                    // ready.
                    self.pending_session_activation = Some(PendingSessionActivation::Resume {
                        acp_session_id: acp_session_id.clone(),
                        title: None,
                        transcript: Some(Box::new(transcript)),
                    });
                    let agent = match self.take_refreshed_prepared_agent() {
                        Ok(Some(agent)) => agent,
                        Ok(None) => return Ok(true),
                        Err(error) => {
                            self.chat_widget.add_error_message(format!(
                                "Couldn't resume prepared agent: {error}"
                            ));
                            return Ok(true);
                        }
                    };
                    let transcript =
                        take_pending_resume_transcript(&mut self.pending_session_activation);

                    let (initial_prompt, initial_images) = self.chat_widget.take_initial_input();
                    self.shutdown_current_conversation();

                    let mut init = self.chat_widget_init(
                        tui.frame_requester(),
                        initial_prompt,
                        initial_images,
                        None,
                        false,
                        None,
                    );
                    init.prepared_agent = Some(agent);
                    self.chat_widget =
                        ChatWidget::new_resumed_acp(init, acp_session_id, None, transcript);
                    self.configure_new_chat_widget();

                    self.chat_widget
                        .add_info_message(format!("Resuming session with {display_name}..."), None);
                    tui.frame_requester().schedule_frame();
                }
                Err(error) => {
                    self.chat_widget
                        .add_error_message(format!("Failed to load session transcript: {error}"));
                    // A resume chosen at startup falls back to a fresh session.
                    if matches!(
                        self.pending_session_activation,
                        Some(PendingSessionActivation::ResumeLoading)
                    ) {
                        if self.prepared_agent.is_some() {
                            self.pending_session_activation = None;
                            self.app_event_tx.send(AppEvent::NewSession);
                        } else {
                            self.pending_session_activation = Some(PendingSessionActivation::New);
                        }
                    }
                }
            },
            AppEvent::ResumeAcpSession {
                acp_session_id,
                title,
            } => {
                if matches!(self.candidate_agent, Some(CandidateAgent::Prepared { .. })) {
                    let Some(CandidateAgent::Prepared {
                        agent_name,
                        display_name,
                        mut agent,
                        ..
                    }) = self.candidate_agent.take()
                    else {
                        unreachable!("candidate variant checked above")
                    };
                    let config = self.config_for_agent(&agent_name);
                    if let Err(error) = nori_harness::runtime::refresh_prepared_agent(
                        &mut agent,
                        crate::chatwidget::agent::agent_prepare_spec(config.clone(), None),
                    ) {
                        tokio::spawn((*agent).shutdown());
                        self.chat_widget.set_login_agent_override(Some(agent_name));
                        self.chat_widget.add_error_message(format!(
                            "Couldn't activate {display_name}: {error}"
                        ));
                        return Ok(true);
                    }
                    let mut init = self.chat_widget_init(
                        tui.frame_requester(),
                        None,
                        Vec::new(),
                        Some(config.active_agent.clone()),
                        false,
                        None,
                    );
                    init.config = config;
                    init.prepared_agent = Some(*agent);
                    let widget = ChatWidget::new_resumed_acp_candidate(
                        init,
                        Some(acp_session_id),
                        title,
                        None,
                    );
                    self.candidate_agent = Some(CandidateAgent::Activating {
                        agent_name,
                        widget: Box::new(widget),
                        activation_error: None,
                        activation_route_eligible: false,
                    });
                    tui.frame_requester().schedule_frame();
                    return Ok(true);
                }

                self.discard_candidate();
                self.cancel_primary_agent_preparation();
                let display_name =
                    crate::nori::agent_picker::get_agent_info(&self.config.active_agent)
                        .map(|info| info.display_name)
                        .unwrap_or_else(|| self.config.active_agent.clone());
                if self.prepared_agent.is_none() {
                    self.defer_resume_activation(
                        tui.frame_requester(),
                        Some(acp_session_id.clone()),
                        title.clone(),
                        None,
                    );
                    self.chat_widget.add_info_message(
                        reattach_info_message(
                            &acp_session_id,
                            title.as_deref(),
                            self.cloud_mode,
                            &display_name,
                        ),
                        None,
                    );
                    tui.frame_requester().schedule_frame();
                    return Ok(true);
                }
                self.pending_session_activation = Some(PendingSessionActivation::Resume {
                    acp_session_id: Some(acp_session_id.clone()),
                    title: title.clone(),
                    transcript: None,
                });
                let agent = match self.take_refreshed_prepared_agent() {
                    Ok(Some(agent)) => agent,
                    Ok(None) => return Ok(true),
                    Err(error) => {
                        self.chat_widget
                            .add_error_message(format!("Couldn't resume prepared agent: {error}"));
                        return Ok(true);
                    }
                };
                self.pending_session_activation = None;
                let (initial_prompt, initial_images) = self.chat_widget.take_initial_input();
                self.deferred_spawn_pending = false;
                self.shutdown_current_conversation();

                let mut init = self.chat_widget_init(
                    tui.frame_requester(),
                    initial_prompt,
                    initial_images,
                    None,
                    false,
                    None,
                );
                init.prepared_agent = Some(agent);
                self.chat_widget = ChatWidget::new_resumed_acp(
                    init,
                    Some(acp_session_id.clone()),
                    title.clone(),
                    None,
                );
                self.configure_new_chat_widget();

                self.chat_widget.add_info_message(
                    reattach_info_message(
                        &acp_session_id,
                        title.as_deref(),
                        self.cloud_mode,
                        &display_name,
                    ),
                    None,
                );
                tui.frame_requester().schedule_frame();
            }
            #[cfg(unix)]
            AppEvent::BrowserLaunched { ws_url, cdp_port } => {
                let prompt =
                    nori_harness::backend::browser_session::compose_agent_prompt(&ws_url, cdp_port);
                self.chat_widget.add_info_message(
                    format!("Browser launched (CDP port {cdp_port}). Notifying agent..."),
                    None,
                );
                self.chat_widget.submit_user_message_text(prompt);
            }
            AppEvent::BrowserLaunchFailed(err) => {
                self.chat_widget
                    .add_error_message(format!("Failed to launch browser: {err}"));
            }
            #[cfg(unix)]
            AppEvent::SetBrowserProfile(mode) => {
                // Persist the choice as the new default, then launch with it.
                self.persist_browser_profile_setting(mode).await;
                self.chat_widget.launch_browser_session(mode);
            }
            #[cfg(not(unix))]
            AppEvent::SetBrowserProfile(_mode) => {}
            AppEvent::OpenForkPicker => {
                let messages =
                    crate::app_backtrack::collect_all_user_messages(&self.transcript_cells);
                // Only agents that advertise ACP `session/fork` can branch at the
                // current head; without it, the picker only rewinds to a message.
                let supports_fork = self.chat_widget.agent_capabilities().session_fork;
                if !supports_fork && messages.is_empty() {
                    self.chat_widget
                        .add_info_message("No messages to fork from.".to_string(), None);
                } else {
                    let params = crate::nori::fork_picker::fork_picker_params(
                        messages,
                        supports_fork,
                        self.app_event_tx.clone(),
                    );
                    self.chat_widget.show_selection_view(params);
                }
                tui.frame_requester().schedule_frame();
            }
            AppEvent::SendSubagentMessage {
                key,
                text,
                image_paths,
                steer,
            } => {
                if let Some(session_id) = self.chat_widget.subagent_session(key) {
                    self.chat_widget.submit_harness_action(
                        crate::app_event::HarnessAction::SendSubagentMessage {
                            key,
                            session_id,
                            text,
                            image_paths,
                            steer,
                        },
                    );
                } else {
                    self.restore_subagent_draft(tui, key, &text, image_paths);
                    tui.show_toast(
                        "This subagent has no session available for follow-up.".to_owned(),
                    );
                }
            }
            AppEvent::SubagentMessageFailed {
                key,
                text,
                image_paths,
                error,
            } => {
                self.restore_subagent_draft(tui, key, &text, image_paths);
                tui.show_toast(error);
            }
            AppEvent::TransientNotice(message) => tui.show_toast(message),
            AppEvent::ChildSessionOutput { key, text } => {
                let roster = self.chat_widget.subagent_roster(std::time::Instant::now());
                let crate::subagent_switcher::Target::Subagent(key) =
                    roster.resolve(crate::subagent_switcher::Target::Subagent(key))
                else {
                    unreachable!("child target remains a child")
                };
                self.session_view.append_child_output(key, &text);
                tui.frame_requester().schedule_frame();
            }
            AppEvent::ForkResponseSubagent { context, source } => {
                self.chat_widget.submit_harness_action(
                    crate::app_event::HarnessAction::ForkHistory { context, source },
                );
            }
            AppEvent::BranchFromCurrent => {
                self.chat_widget
                    .submit_harness_action(crate::app_event::HarnessAction::Branch);
            }
            AppEvent::ForkToMessage {
                cell_index,
                prefill,
            } => {
                let summary =
                    crate::app_backtrack::build_fork_summary(&self.transcript_cells, cell_index);
                let fork_context = if summary.is_empty() {
                    None
                } else {
                    Some(summary)
                };

                self.shutdown_current_conversation();
                let init = self.chat_widget_init(
                    tui.frame_requester(),
                    None,
                    Vec::new(),
                    None,
                    true,
                    None,
                );
                self.chat_widget = ChatWidget::new(init);
                self.configure_new_chat_widget();
                self.pending_session_activation = Some(PendingSessionActivation::New);
                self.begin_agent_preparation_with_context(
                    crate::app_event::AgentPrepareIntent::Idle,
                    fork_context,
                );

                // Trim transcript to preserve history before the fork point
                self.transcript_cells
                    .truncate(cell_index.min(self.transcript_cells.len()));
                self.render_transcript_once(tui);

                if !prefill.is_empty() {
                    self.chat_widget.set_composer_text(prefill);
                }
                tui.frame_requester().schedule_frame();
            }
            AppEvent::SaveMcpServers(servers) => {
                self.persist_mcp_servers(servers).await;
            }
            AppEvent::McpOAuthLogin {
                server_name,
                server_url,
                http_headers,
                env_http_headers,
                client_id,
                client_secret_env_var,
            } => {
                self.program_status.auth(None);
                self.perform_mcp_oauth_login(
                    tui,
                    server_name,
                    server_url,
                    http_headers,
                    env_http_headers,
                    client_id,
                    client_secret_env_var,
                )
                .await;
            }
            AppEvent::McpOAuthLoginCancel { server_name } => {
                self.program_status.finish_auth();
                tracing::info!("MCP OAuth login cancelled for {server_name}");
                self.cancel_mcp_oauth_login();
                self.chat_widget.add_info_message(
                    format!("OAuth cancelled for `{server_name}`. Press `l` in /mcp to try again."),
                    None,
                );
            }
            AppEvent::McpOAuthLoginComplete {
                server_name,
                success,
                error,
            } => {
                self.program_status.finish_auth();
                if success {
                    self.chat_widget.add_info_message(
                        format!(
                            "Successfully authenticated with `{server_name}`. Restart to apply."
                        ),
                        None,
                    );
                } else {
                    let msg = match error {
                        Some(err) => {
                            format!("OAuth login for `{server_name}` failed: {err}")
                        }
                        None => format!("OAuth login for `{server_name}` failed."),
                    };
                    self.chat_widget.add_error_message(msg);
                }
                self.chat_widget
                    .handle_mcp_oauth_complete(&server_name, success);
            }
            AppEvent::ComputeMcpAuthStatuses => {
                let servers = self.chat_widget.config_ref().mcp_servers.clone();
                let tx = self.app_event_tx.clone();
                tokio::spawn(async move {
                    let mut statuses = std::collections::HashMap::new();
                    for (name, config) in servers {
                        let status = match config.transport {
                            nori_config::McpServerTransportConfig::Stdio {
                                ..
                            } => codex_rmcp_client::McpAuthStatus::Unsupported,
                            nori_config::McpServerTransportConfig::StreamableHttp {
                                url,
                                bearer_token_env_var,
                                http_headers,
                                env_http_headers,
                                ..
                            } => codex_rmcp_client::determine_streamable_http_auth_status(
                                &name,
                                &url,
                                bearer_token_env_var.as_deref(),
                                http_headers,
                                env_http_headers,
                                codex_rmcp_client::OAuthCredentialsStoreMode::Auto,
                            )
                            .await
                            .unwrap_or_else(|err| {
                                tracing::warn!(
                                    "failed to determine auth status for MCP server `{name}`: {err:?}"
                                );
                                codex_rmcp_client::McpAuthStatus::Unsupported
                            }),
                        };
                        statuses.insert(name, status);
                    }
                    tx.send(AppEvent::McpAuthStatusesReady(statuses));
                });
            }
            AppEvent::McpAuthStatusesReady(statuses) => {
                self.chat_widget.update_mcp_auth_statuses(&statuses);
                tui.frame_requester().schedule_frame();
            }
        }
        Ok(true)
    }

    pub(super) async fn handle_key_event(&mut self, tui: &mut tui::Tui, key_event: KeyEvent) {
        use crate::nori::hotkey_match::matches_binding;
        use nori_config::HotkeyAction;

        if let Some(pending) = self.backtrack.pending_child_focus.as_mut() {
            if self.session_view.focused()
                != crate::subagent_switcher::Target::Subagent(pending.key)
                || pending.generation != self.chat_widget.session_generation()
                || self.chat_widget.subagent_session(pending.key).as_ref()
                    != Some(&pending.session_id)
            {
                self.backtrack.pending_child_focus = None;
            } else if key_event.kind == KeyEventKind::Press {
                match (key_event.code, key_event.modifiers) {
                    (KeyCode::Esc, _) => {
                        self.backtrack.pending_child_focus = None;
                        return;
                    }
                    (KeyCode::Char('h' | 'l'), KeyModifiers::CONTROL)
                    | (KeyCode::Char(' '), KeyModifiers::NONE)
                    | (KeyCode::Enter, KeyModifiers::NONE) => {
                        pending.keys.push(key_event);
                        return;
                    }
                    _ => self.backtrack.pending_child_focus = None,
                }
            }
        }
        if !self.chat_widget.has_active_overlay_or_popup()
            && self.handle_response_focus(tui, key_event)
        {
            return;
        }
        // Check configurable hotkeys first (before the structural match),
        // but only when no popup/view is active — otherwise the popup should
        // capture the key (e.g. the hotkey picker in rebinding mode).
        if key_event.kind == KeyEventKind::Press && !self.chat_widget.has_active_popup() {
            let transcript_binding = self.hotkey_config.binding_for(HotkeyAction::OpenTranscript);
            if matches_binding(transcript_binding, &key_event) {
                if let crate::subagent_switcher::Target::Subagent(key) = self.session_view.focused()
                {
                    self.request_child_history(tui, key, false);
                    return;
                }
                self.open_transcript_overlay(tui);
                return;
            }

            let editor_binding = self.hotkey_config.binding_for(HotkeyAction::OpenEditor);
            if matches_binding(editor_binding, &key_event) {
                self.open_external_editor(tui);
                return;
            }

            let commit_binding = self.hotkey_config.binding_for(HotkeyAction::CommitChanges);
            if matches_binding(commit_binding, &key_event) {
                self.app_event_tx
                    .send(AppEvent::CommitChanges { automatic: false });
                return;
            }

            let switch_binding = self.hotkey_config.binding_for(HotkeyAction::SwitchSubagent);
            if matches_binding(switch_binding, &key_event) {
                self.open_subagent_switcher(tui);
                return;
            }

            let plan_binding = self
                .hotkey_config
                .binding_for(HotkeyAction::TogglePlanDrawer);
            if matches_binding(plan_binding, &key_event) {
                self.chat_widget.toggle_plan_drawer();
                self.plan_drawer_mode = self.chat_widget.plan_drawer_mode();
                tui.frame_requester().schedule_frame();
                return;
            }
        }

        if key_event.kind == KeyEventKind::Press
            && key_event.modifiers == KeyModifiers::CONTROL
            && matches!(key_event.code, KeyCode::Char('h' | 'l'))
            && !self.chat_widget.has_active_overlay_or_popup()
        {
            if key_event.code == KeyCode::Char('l') {
                return;
            }
            if let crate::subagent_switcher::Target::Subagent(key) = self.session_view.focused() {
                self.request_child_history(tui, key, true);
                return;
            }
            self.begin_response_focus(tui, self.transcript_cells.clone());
            return;
        }

        match key_event {
            // Esc primes/advances backtracking only in normal (not working) mode
            // with the composer focused and empty. In any other state, forward
            // Esc so the active UI (e.g. status indicator, modals, popups)
            // handles it.
            KeyEvent {
                code: KeyCode::Esc,
                kind: KeyEventKind::Press | KeyEventKind::Repeat,
                ..
            } => {
                if self.chat_widget.has_active_overlay_or_popup() {
                    self.chat_widget.handle_key_event(key_event);
                } else if self.session_view.tool_detail() {
                    self.session_view.close_tool_detail();
                    if !self.deferred_history_lines.is_empty() {
                        tui.insert_history_lines(std::mem::take(&mut self.deferred_history_lines));
                    }
                    tui.frame_requester().schedule_frame();
                } else {
                    match self.session_view.focused() {
                        crate::subagent_switcher::Target::Main
                            if self.chat_widget.is_task_running() =>
                        {
                            self.chat_widget
                                .submit_harness_action(crate::app_event::HarnessAction::Cancel);
                        }
                        crate::subagent_switcher::Target::Subagent(key) => {
                            let roster =
                                self.chat_widget.subagent_roster(std::time::Instant::now());
                            let running = roster
                                .entry(crate::subagent_switcher::Target::Subagent(key))
                                .is_some_and(|entry| {
                                    entry.status == crate::subagent_switcher::Status::Running
                                });
                            if running {
                                if let (Some(handle), Some(session_id)) = (
                                    self.chat_widget.harness_handle(),
                                    self.chat_widget.subagent_session(key),
                                ) {
                                    let tx = self.app_event_tx.clone();
                                    tokio::spawn(async move {
                                        if let Err(error) = handle.cancel_subagent(session_id).await
                                        {
                                            tx.send(AppEvent::TransientNotice(error.to_string()));
                                        }
                                    });
                                } else {
                                    tui.show_toast(
                                        "This child has no session available to stop.".to_owned(),
                                    );
                                }
                            } else if self.chat_widget.composer_cursor_style()
                                == crossterm::cursor::SetCursorStyle::SteadyBar
                            {
                                self.chat_widget.handle_key_event(key_event);
                            } else {
                                let target = self
                                    .chat_widget
                                    .subagent_parent(key)
                                    .map(crate::subagent_switcher::Target::Subagent)
                                    .map(|target| roster.resolve(target))
                                    .filter(|target| roster.entry(*target).is_some())
                                    .unwrap_or(crate::subagent_switcher::Target::Main);
                                self.focus_session_view(tui, target);
                            }
                        }
                        crate::subagent_switcher::Target::Main => {
                            self.chat_widget.handle_key_event(key_event)
                        }
                    }
                }
            }
            // Enter confirms backtrack when primed + count > 0. Otherwise pass to widget.
            KeyEvent {
                code: KeyCode::Enter,
                kind: KeyEventKind::Press,
                ..
            } if self.backtrack.primed
                && self.backtrack.nth_user_message != usize::MAX
                && self.chat_widget.composer_is_empty() =>
            {
                // Delegate to helper for clarity; preserves behavior.
                self.confirm_backtrack_from_main();
            }
            KeyEvent {
                kind: KeyEventKind::Press | KeyEventKind::Repeat,
                ..
            } => {
                // Any non-Esc key press should cancel a primed backtrack.
                // This avoids stale "Esc-primed" state after the user starts typing
                // (even if they later backspace to empty).
                if key_event.code != KeyCode::Esc && self.backtrack.primed {
                    self.reset_backtrack_state();
                }
                self.chat_widget.handle_key_event(key_event);
            }
            _ => {
                // Ignore Release key events.
            }
        };
    }

    pub(super) fn should_handle_backtrack_esc(&self, key_event: KeyEvent) -> bool {
        self.chat_widget.is_normal_backtrack_mode()
            && self.chat_widget.composer_is_empty()
            && !self.chat_widget.should_handle_vim_insert_escape(key_event)
    }
}

/// Compose the info-cell message shown when resuming/reattaching to a session.
///
/// Cloud reattach wording names the selected broker session. It deliberately
/// makes no claim about replay: that depends on the facade's capabilities.
pub(super) fn reattach_info_message(
    acp_session_id: &str,
    title: Option<&str>,
    cloud_mode: bool,
    display_name: &str,
) -> String {
    if cloud_mode {
        let session_label = match title {
            Some(title) => format!("{acp_session_id} ({title})"),
            None => acp_session_id.to_string(),
        };
        format!("Reattaching to {session_label}...")
    } else {
        format!("Resuming session with {display_name}...")
    }
}

/// Take back the transcript of a resume left pending in case the agent was not ready, now that it
/// is: moving it out, rather than having kept a copy, holds one transcript in memory at a time.
fn take_pending_resume_transcript(
    pending: &mut Option<PendingSessionActivation>,
) -> Option<nori_harness::transcript::Transcript> {
    match pending.take() {
        Some(PendingSessionActivation::Resume { transcript, .. }) => {
            transcript.map(|transcript| *transcript)
        }
        Some(PendingSessionActivation::New | PendingSessionActivation::ResumeLoading) | None => {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::CandidateActivationEvent;
    use super::classify_candidate_activation_event;
    use super::reattach_info_message;

    #[test]
    fn candidate_model_must_still_be_advertised_by_the_activated_agent() {
        use nori_protocol::acp::v1 as acp;
        use pretty_assertions::assert_eq;
        let option = acp::SessionConfigOption::select(
            "model",
            "Model",
            "first",
            vec![acp::SessionConfigSelectOption::new("first", "First")],
        )
        .category(acp::SessionConfigOptionCategory::Model);
        assert_eq!(
            super::candidate_model_config(std::slice::from_ref(&option), "first"),
            Some("model".to_string())
        );
        assert_eq!(super::candidate_model_config(&[option], "stale"), None);
    }

    #[test]
    fn cloud_reattach_info_message_snapshot() {
        insta::assert_snapshot!(
            "cloud_reattach_info_message",
            reattach_info_message("session-123", Some("Fix reconnect"), true, "Nori Cloud")
        );
    }

    #[test]
    fn candidate_activation_prefers_structured_acp_error_detail() {
        let request_id = nori_protocol::acp::v1::RequestId::Str("session-new".to_string());
        let error = nori_protocol::acp::v1::Error::new(-32010, "broker unreachable").data(
            serde_json::json!({
                "detail": "connection reset by broker",
                "trace_id": "internal-only"
            }),
        );
        let mut raw_error = None;

        let response = nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::Response {
            request_id,
            response: Err(error),
        });
        assert_eq!(
            classify_candidate_activation_event(&response, &mut raw_error),
            CandidateActivationEvent::CapturedError
        );
        assert_eq!(
            raw_error.as_deref(),
            Some("broker unreachable: connection reset by broker")
        );

        let ended = nori_protocol::SessionEvent::Nori(nori_protocol::NoriEvent::SessionEnded(
            nori_protocol::SessionEnded {
                reason: nori_protocol::SessionEndReason::SpawnFailed,
                message: Some("Failed to create session".to_string()),
            },
        ));
        assert_eq!(
            classify_candidate_activation_event(&ended, &mut raw_error),
            CandidateActivationEvent::Ended(
                "broker unreachable: connection reset by broker".to_string()
            )
        );
        assert_eq!(raw_error, None);
    }

    #[test]
    fn candidate_activation_defers_request_failure_until_session_end() {
        let mut activation_error = None;
        let failure_message = "Connection timed out. The agent did not respond.";
        let request_failed = nori_protocol::SessionEvent::Nori(
            nori_protocol::NoriEvent::RequestFailed(nori_protocol::RequestFailure {
                request_id: None,
                message: failure_message.to_string(),
                kind: nori_protocol::RequestFailureKind::Retryable,
            }),
        );

        assert_eq!(
            classify_candidate_activation_event(&request_failed, &mut activation_error),
            CandidateActivationEvent::CapturedError
        );
        assert_eq!(activation_error.as_deref(), Some(failure_message));

        let ended = nori_protocol::SessionEvent::Nori(nori_protocol::NoriEvent::SessionEnded(
            nori_protocol::SessionEnded {
                reason: nori_protocol::SessionEndReason::TimedOut,
                message: Some(failure_message.to_string()),
            },
        ));
        assert_eq!(
            classify_candidate_activation_event(&ended, &mut activation_error),
            CandidateActivationEvent::Ended(failure_message.to_string())
        );
        assert_eq!(activation_error, None);
    }
}
