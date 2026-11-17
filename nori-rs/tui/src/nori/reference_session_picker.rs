//! Search past project conversations and insert a grounded reference into the draft.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;

use nori_harness::transcript::TranscriptLoader;
use nori_tui_components::PickerColumn;
use nori_tui_components::PickerDensity;
use nori_tui_components::PickerDetail;
use nori_tui_components::PickerItem;
use nori_tui_components::PickerState;
use nori_tui_components::SearchMode;

use super::viewonly_session_picker::SessionPickerInfo;
use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::bottom_pane::ComponentPickerParams;
use crate::bottom_pane::SelectionAction;

#[derive(Debug, Clone)]
pub(crate) struct ReferenceSession {
    pub(crate) session: SessionPickerInfo,
    pub(crate) agent: Option<String>,
}

pub(crate) async fn load(nori_home: &Path, cwd: &Path) -> std::io::Result<Vec<ReferenceSession>> {
    let metadata = TranscriptLoader::new(nori_home.to_path_buf())
        .find_session_metadata_for_cwd(cwd)
        .await?;
    Ok(metadata
        .into_iter()
        .map(|item| {
            let agent = item.agent.clone();
            ReferenceSession {
                session: SessionPickerInfo::from(item),
                agent,
            }
        })
        .collect())
}

pub(crate) fn picker(
    sessions: Vec<ReferenceSession>,
    target: crate::subagent_switcher::Target,
) -> ComponentPickerParams {
    let mut actions = BTreeMap::new();
    let items: Vec<_> = sessions
        .into_iter()
        .map(|item| {
            let session = item.session;
            let agent = item.agent.unwrap_or_else(|| "Unknown agent".to_owned());
            let preview = session
                .first_message_preview
                .unwrap_or_else(|| "Loading preview…".to_owned());
            let key = format!("reference:{}:{}", session.project_id, session.session_id);
            let project_id = session.project_id.clone();
            let session_id = session.session_id.clone();
            let reference_agent = agent.clone();
            actions.insert(
                key.clone(),
                Box::new(move |tx: &AppEventSender| {
                    tx.send(AppEvent::LoadSessionReference {
                        project_id: project_id.clone(),
                        session_id: session_id.clone(),
                        agent: reference_agent.clone(),
                        target,
                    })
                }) as SelectionAction,
            );
            PickerItem::new(key, "session", session.session_id.clone())
                .cell("agent", agent.clone())
                .cell("preview", preview.clone())
                .search_text(format!(
                    "{agent} {} {} {preview}",
                    session.project_id, session.session_id
                ))
                .details([
                    PickerDetail::new("Agent", agent),
                    PickerDetail::new("Session", session.session_id),
                    PickerDetail::new("Project", session.project_id),
                    PickerDetail::new("First message", preview),
                ])
        })
        .collect();
    let mut state = PickerState::new(
        "Reference a past conversation",
        vec![
            PickerColumn::flexible("session", "Session"),
            PickerColumn::flexible("agent", "Agent"),
            PickerColumn::flexible("preview", "First message"),
        ],
        items,
    )
    .subtitle("Search past project conversations; Enter inserts a reference into your draft")
    .search_mode(SearchMode::Fuzzy)
    .search_placeholder("Search agent, session or first message");
    state.search_active = true;
    ComponentPickerParams {
        state,
        actions,
        highlights: BTreeMap::new(),
        on_dismiss: None,
        on_shift_tab: None,
        primary_column: "session".to_string(),
        detail_column: Some("preview".to_string()),
        density: PickerDensity::Compact,
        show_title: true,
        show_details: true,
        keep_open: BTreeSet::new(),
        footer_hints: None,
    }
}

/// Include a bounded, explicitly labelled tail of actual user and assistant messages.
pub(crate) fn format_reference(
    project_id: &str,
    session_id: &str,
    agent: &str,
    transcript: &nori_harness::transcript::Transcript,
) -> String {
    let entries = crate::viewonly_transcript::transcript_to_entries(transcript);
    format_reference_entries(project_id, session_id, agent, &entries)
}

fn format_reference_entries(
    project_id: &str,
    session_id: &str,
    agent: &str,
    entries: &[crate::viewonly_transcript::ViewonlyEntry],
) -> String {
    use crate::viewonly_transcript::ViewonlyEntry;
    const MAX_CHARS: usize = 12_000;
    let mut remaining = MAX_CHARS;
    let mut parts = Vec::new();
    let mut truncated = false;
    for entry in entries.iter().rev() {
        let (role, content) = match entry {
            ViewonlyEntry::User { content } => ("User", content),
            ViewonlyEntry::Assistant { content } => ("Assistant", content),
            ViewonlyEntry::Thinking { .. } | ViewonlyEntry::Info { .. } => continue,
        };
        if remaining == 0 {
            truncated = true;
            break;
        }
        let length = content.chars().count();
        let selected = if length > remaining {
            truncated = true;
            content.chars().skip(length - remaining).collect::<String>()
        } else {
            content.clone()
        };
        remaining = remaining.saturating_sub(selected.chars().count());
        parts.push(format!("{role}: {selected}"));
    }
    parts.reverse();
    let notice = if truncated {
        " (earlier content omitted)"
    } else {
        ""
    };
    let body = if parts.is_empty() {
        "No user or assistant text was recorded.".to_owned()
    } else {
        parts.join("\n\n")
    };
    format!(
        "@session[project={project_id}, id={session_id}, agent={agent}]\n--- Conversation excerpt{notice} ---\n{body}\n--- End conversation excerpt ---\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use nori_tui_components::Picker;
    use nori_tui_components::PickerAction;
    use pretty_assertions::assert_eq;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::widgets::Widget;

    fn session(id: &str, agent: &str, preview: &str) -> ReferenceSession {
        ReferenceSession {
            session: SessionPickerInfo {
                session_id: id.to_owned(),
                project_id: "project-1".to_owned(),
                started_at: "2026-01-01".to_owned(),
                user_turn_count: Some(1),
                first_message_preview: Some(preview.to_owned()),
            },
            agent: Some(agent.to_owned()),
        }
    }

    #[test]
    fn reference_contains_bounded_actual_conversation_tail() {
        use crate::viewonly_transcript::ViewonlyEntry;
        let entries = vec![
            ViewonlyEntry::User {
                content: "earlier question".to_owned(),
            },
            ViewonlyEntry::Assistant {
                content: "é".repeat(12_005),
            },
        ];
        let text = format_reference_entries("p", "s", "codex", &entries);
        assert!(text.starts_with("@session[project=p, id=s, agent=codex]\n--- Conversation excerpt (earlier content omitted) ---\nAssistant: "));
        assert!(!text.contains("earlier question"));
        assert_eq!(text.matches('é').count(), 12_000);
        assert!(text.ends_with("--- End conversation excerpt ---\n"));
    }

    #[test]
    fn fuzzy_search_previews_and_inserts_real_session_reference() {
        let mut params = picker(
            vec![
                session("session-1", "claude", "Investigate build errors"),
                session("session-2", "codex", "Improve search"),
            ],
            crate::subagent_switcher::Target::Main,
        );
        for ch in "codex".chars() {
            params.state.handle(PickerAction::AppendQuery(ch));
        }
        assert_eq!(params.state.visible_indices(), vec![1]);
        let area = Rect::new(0, 0, 100, 12);
        let mut buffer = Buffer::empty(area);
        Picker::new(&params.state)
            .show_title(true)
            .show_details(true)
            .render(area, &mut buffer);
        let rendered = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buffer[(x, y)].symbol().chars().next().unwrap_or(' '))
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n");
        insta::assert_snapshot!("reference_session_search", rendered);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        params.actions.get("reference:project-1:session-2").unwrap()(&AppEventSender::new(tx));
        let AppEvent::LoadSessionReference {
            project_id,
            session_id,
            agent,
            target: _,
        } = rx.try_recv().unwrap()
        else {
            panic!("expected reference")
        };
        assert_eq!(
            (project_id, session_id, agent),
            (
                "project-1".to_owned(),
                "session-2".to_owned(),
                "codex".to_owned()
            )
        );
    }
}
