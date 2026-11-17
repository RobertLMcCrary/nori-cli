//! Interactive answers to an agent's user-input request.

use std::cell::Cell;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use nori_protocol::acp::v1 as acp;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use serde::Deserialize;

use super::BottomPaneView;
use super::CancellationEvent;
use crate::app_event::AppEvent;
use crate::app_event::HarnessAction;
use crate::app_event_sender::AppEventSender;
use crate::render::renderable::Renderable;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UserQuestionRequest {
    #[serde(skip)]
    pub(crate) request_id: Option<acp::RequestId>,
    #[serde(skip)]
    pub(crate) label: Option<String>,
    pub(crate) session_id: acp::SessionId,
    questions: Vec<Question>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Question {
    id: String,
    header: String,
    question: String,
    #[serde(default)]
    is_other: bool,
    #[serde(default)]
    is_secret: bool,
    #[serde(default)]
    multiple: bool,
    #[serde(default)]
    options: Option<Vec<OptionItem>>,
}

#[derive(Debug, Deserialize)]
struct OptionItem {
    label: String,
    description: String,
}

impl UserQuestionRequest {
    pub(crate) fn validation_error(&self) -> Option<&'static str> {
        if self.questions.is_empty() {
            return Some("the question list is empty");
        }
        let mut ids = BTreeSet::new();
        for question in &self.questions {
            if question.id.is_empty() || !ids.insert(&question.id) {
                return Some("question IDs are missing or duplicated");
            }
        }
        None
    }
}

pub(super) struct UserQuestionView {
    request: UserQuestionRequest,
    tx: AppEventSender,
    current: usize,
    selected: usize,
    marked: BTreeSet<usize>,
    text: String,
    answers: BTreeMap<String, Vec<String>>,
    done: bool,
    hits: RefCell<Vec<(Rect, QuestionHit)>>,
    pointer: Cell<Option<QuestionHit>>,
    drag_row: Cell<Option<u16>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum QuestionHit {
    Option(usize),
    Submit,
    Cancel,
}

impl UserQuestionView {
    pub(super) fn new(request: UserQuestionRequest, tx: AppEventSender) -> Self {
        Self {
            request,
            tx,
            current: 0,
            selected: 0,
            marked: BTreeSet::new(),
            text: String::new(),
            answers: BTreeMap::new(),
            done: false,
            hits: RefCell::new(Vec::new()),
            pointer: Cell::new(None),
            drag_row: Cell::new(None),
        }
    }

    fn respond(&mut self, cancel: bool) {
        let answers: serde_json::Map<String, serde_json::Value> = if cancel {
            Default::default()
        } else {
            self.answers
                .iter()
                .map(|(key, value)| (key.clone(), serde_json::json!({"answers": value})))
                .collect()
        };
        let response = if cancel {
            serde_json::json!({"cancelled": true})
        } else {
            serde_json::json!({"answers": answers})
        };
        if let Some(request_id) = self.request.request_id.clone()
            && let Ok(raw) = serde_json::value::to_raw_value(&response)
        {
            self.tx
                .send(AppEvent::HarnessAction(HarnessAction::RespondToAgent {
                    request_id,
                    response: Box::new(Ok(acp::ClientResponse::ExtMethodResponse(
                        acp::ExtResponse::new(Arc::from(raw)),
                    ))),
                }));
        }
        self.done = true;
    }

    fn lines(&self, width: u16) -> Vec<Line<'static>> {
        let Some(question) = self.request.questions.get(self.current) else {
            return vec!["No questions supplied".into()];
        };
        let mode = if question.multiple {
            " · select multiple"
        } else {
            ""
        };
        let mut lines = vec![
            Line::from(format!(
                "{} · {}{mode}",
                self.request.label.as_deref().unwrap_or("Agent"),
                question.header
            ))
            .bold(),
        ];
        lines.extend(
            textwrap::wrap(&question.question, usize::from(width).max(1))
                .into_iter()
                .map(|line| Line::from(line.into_owned())),
        );
        if let Some(options) = &question.options {
            for (index, option) in options.iter().enumerate() {
                let marker = if question.multiple {
                    if self.marked.contains(&index) {
                        "☑ "
                    } else {
                        "☐ "
                    }
                } else {
                    ""
                };
                lines.push(Line::from(format!(
                    "{} {marker}{}",
                    if self.text.is_empty() && index == self.selected {
                        "›"
                    } else {
                        " "
                    },
                    option.label
                )));
                if !option.description.is_empty() {
                    lines.extend(
                        textwrap::wrap(
                            &option.description,
                            usize::from(width.saturating_sub(2)).max(1),
                        )
                        .into_iter()
                        .map(|line| Line::from(format!("  {line}")).dim()),
                    );
                }
            }
        }
        if question.options.as_ref().is_none_or(Vec::is_empty) || question.is_other {
            let visible = if question.is_secret {
                "•".repeat(self.text.chars().count())
            } else {
                self.text.clone()
            };
            lines.push(Line::from(format!("› {visible}")));
        }
        lines.push(Line::from("[ Submit ]  [ Cancel ]"));
        lines
    }

    fn scroll_offset(&self, area: Rect) -> usize {
        let Some(question) = self.request.questions.get(self.current) else {
            return 0;
        };
        let lines = self.lines(area.width);
        let focus = if !self.text.is_empty() || question.options.as_ref().is_none_or(Vec::is_empty)
        {
            lines.len().saturating_sub(2)
        } else {
            let preceding = question
                .options
                .as_ref()
                .into_iter()
                .flatten()
                .take(self.selected)
                .map(|option| {
                    1 + if option.description.is_empty() {
                        0
                    } else {
                        textwrap::wrap(
                            &option.description,
                            usize::from(area.width.saturating_sub(2)).max(1),
                        )
                        .len()
                    }
                })
                .sum::<usize>();
            1 + textwrap::wrap(&question.question, usize::from(area.width).max(1)).len() + preceding
        };
        focus
            .saturating_add(1)
            .saturating_sub(usize::from(area.height))
    }
}

impl Renderable for UserQuestionView {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        let content = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(1));
        let scroll = u16::try_from(self.scroll_offset(content)).unwrap_or(u16::MAX);
        let mut lines = self.lines(area.width);
        let actions = lines.pop().unwrap_or_default();
        Paragraph::new(lines)
            .scroll((scroll, 0))
            .render(content, buf);
        if area.height > 0 {
            Paragraph::new(actions)
                .render(Rect::new(area.x, area.bottom() - 1, area.width, 1), buf);
        }
        let mut hits = self.hits.borrow_mut();
        hits.clear();
        if let Some(question) = self.request.questions.get(self.current) {
            let mut row =
                1 + textwrap::wrap(&question.question, usize::from(area.width).max(1)).len();
            for (index, option) in question.options.as_ref().into_iter().flatten().enumerate() {
                if let Ok(visible) = u16::try_from(row.saturating_sub(usize::from(scroll)))
                    && row >= usize::from(scroll)
                    && visible < content.height
                {
                    hits.push((
                        Rect::new(area.x, area.y + visible, area.width, 1),
                        QuestionHit::Option(index),
                    ));
                }
                row += 1 + if option.description.is_empty() {
                    0
                } else {
                    textwrap::wrap(
                        &option.description,
                        usize::from(area.width.saturating_sub(2)).max(1),
                    )
                    .len()
                };
            }
            if area.height > 0 {
                hits.push((
                    Rect::new(area.x, area.bottom() - 1, area.width.min(10), 1),
                    QuestionHit::Submit,
                ));
                if area.width > 12 {
                    hits.push((
                        Rect::new(area.x + 12, area.bottom() - 1, (area.width - 12).min(10), 1),
                        QuestionHit::Cancel,
                    ));
                }
            }
        }
    }
    fn desired_height(&self, width: u16) -> u16 {
        u16::try_from(self.lines(width).len())
            .unwrap_or(u16::MAX)
            .max(1)
    }
    fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        let question = self.request.questions.get(self.current)?;
        if question
            .options
            .as_ref()
            .is_some_and(|options| !options.is_empty())
            && !question.is_other
        {
            return None;
        }
        let content = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(1));
        let row = u16::try_from(
            self.lines(area.width)
                .len()
                .saturating_sub(2)
                .saturating_sub(self.scroll_offset(content)),
        )
        .ok()?;
        if row >= content.height {
            return None;
        }
        let text_width = if question.is_secret {
            self.text.chars().count()
        } else {
            unicode_width::UnicodeWidthStr::width(self.text.as_str())
        };
        let column = u16::try_from(text_width.saturating_add(2))
            .unwrap_or(u16::MAX)
            .min(area.width.saturating_sub(1));
        Some((area.x.saturating_add(column), area.y.saturating_add(row)))
    }
}

impl BottomPaneView for UserQuestionView {
    fn handle_mouse_event(&mut self, mouse: crossterm::event::MouseEvent) -> bool {
        use crossterm::event::MouseButton;
        use crossterm::event::MouseEventKind;
        let hit = self
            .hits
            .borrow()
            .iter()
            .find(|(area, _)| {
                area.contains(ratatui::layout::Position::new(mouse.column, mouse.row))
            })
            .map(|(_, hit)| *hit);
        match mouse.kind {
            MouseEventKind::Moved => return false,
            MouseEventKind::Down(MouseButton::Left) => {
                self.pointer.set(hit);
                self.drag_row.set(Some(mouse.row));
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.pointer.set(None);
                if let Some(previous) = self.drag_row.replace(Some(mouse.row)) {
                    let delta = i32::from(previous) - i32::from(mouse.row);
                    let count = self
                        .request
                        .questions
                        .get(self.current)
                        .and_then(|question| question.options.as_ref())
                        .map_or(0, Vec::len);
                    if count > 0 {
                        self.selected = usize::try_from(
                            (self.selected as i64 + i64::from(delta)).clamp(0, (count - 1) as i64),
                        )
                        .unwrap_or(0);
                    }
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.drag_row.set(None);
                let pressed = self.pointer.take();
                if pressed.is_some() && pressed == hit {
                    match hit {
                        Some(QuestionHit::Option(index)) => {
                            self.selected = index;
                            self.text.clear();
                            if self.request.questions[self.current].multiple
                                && !self.marked.insert(index)
                            {
                                self.marked.remove(&index);
                            }
                        }
                        Some(QuestionHit::Submit) => {
                            self.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                        }
                        Some(QuestionHit::Cancel) => self.respond(true),
                        None => {}
                    }
                }
            }
            MouseEventKind::ScrollUp => {
                self.handle_key_event(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
            }
            MouseEventKind::ScrollDown => {
                self.handle_key_event(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            }
            _ => {}
        }
        true
    }
    fn handle_key_event(&mut self, key: KeyEvent) {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return;
        }
        let Some(question) = self.request.questions.get(self.current) else {
            self.respond(true);
            return;
        };
        let options = question.options.as_deref().unwrap_or_default();
        match key.code {
            KeyCode::Up if !options.is_empty() && self.text.is_empty() => {
                self.selected = self.selected.saturating_sub(1);
            }
            KeyCode::Down if !options.is_empty() && self.text.is_empty() => {
                self.selected = (self.selected + 1).min(options.len() - 1);
            }
            KeyCode::Backspace => {
                self.text.pop();
            }
            KeyCode::Char(' ')
                if question.multiple && !options.is_empty() && self.text.is_empty() =>
            {
                if !self.marked.insert(self.selected) {
                    self.marked.remove(&self.selected);
                }
            }
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    && (options.is_empty() || question.is_other) =>
            {
                self.text.push(ch)
            }
            KeyCode::Enter => {
                let answers = if question.multiple {
                    let mut answers = self
                        .marked
                        .iter()
                        .filter_map(|index| options.get(*index).map(|option| option.label.clone()))
                        .collect::<Vec<_>>();
                    if !self.text.is_empty() {
                        answers.push(self.text.clone());
                    }
                    if answers.is_empty()
                        && let Some(option) = options.get(self.selected)
                    {
                        answers.push(option.label.clone());
                    }
                    answers
                } else if !self.text.is_empty() {
                    vec![self.text.clone()]
                } else {
                    options
                        .get(self.selected)
                        .map(|option| vec![option.label.clone()])
                        .unwrap_or_default()
                };
                if !answers.is_empty() {
                    self.answers.insert(question.id.clone(), answers);
                    self.current += 1;
                    self.selected = 0;
                    self.marked.clear();
                    self.text.clear();
                    if self.current == self.request.questions.len() {
                        self.respond(false);
                    }
                }
            }
            _ => {}
        }
    }
    fn handle_paste(&mut self, text: String) -> bool {
        if self
            .request
            .questions
            .get(self.current)
            .is_some_and(|q| q.is_other || q.options.as_ref().is_none_or(Vec::is_empty))
        {
            self.text.push_str(&text);
            true
        } else {
            false
        }
    }
    fn on_ctrl_c(&mut self) -> CancellationEvent {
        self.respond(true);
        CancellationEvent::Handled
    }
    fn is_complete(&self) -> bool {
        self.done
    }
    fn preserve_previous_views(&self) -> bool {
        true
    }
    fn withdraw_approval_request(&mut self, request_id: &acp::RequestId) -> bool {
        if self.request.request_id.as_ref() == Some(request_id) {
            self.done = true;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn request() -> UserQuestionRequest {
        let mut request: UserQuestionRequest = serde_json::from_value(serde_json::json!({
            "sessionId": "child", "callId": "call", "turnId": "turn",
            "questions": [
                {"id":"choice", "header":"Plan", "question":"Which option?", "isOther":true, "isSecret":false,
                 "options":[{"label":"First", "description":"First option"}, {"label":"Second", "description":"Second option"}]},
                {"id":"secret", "header":"Secret", "question":"Value?", "isOther":true, "isSecret":true, "options":null}
            ]
        })).unwrap();
        request.request_id = Some(acp::RequestId::Str("child/request".to_owned()));
        request.label = Some("Child".to_owned());
        request
    }

    #[test]
    fn multi_question_answers_and_secret_masking() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut view = UserQuestionView::new(request(), AppEventSender::new(tx));
        let rendered = view
            .lines(40)
            .iter()
            .map(|line| line.to_string().trim_end().to_owned())
            .collect::<Vec<_>>()
            .join("\n");
        insta::assert_snapshot!(rendered, @"Child · Plan
Which option?
› First
  First option
  Second
  Second option
›
[ Submit ]  [ Cancel ]");
        view.handle_key_event(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        view.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        view.handle_paste("private".to_owned());
        let secret = view
            .lines(40)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!secret.contains("private"));
        view.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let AppEvent::HarnessAction(HarnessAction::RespondToAgent {
            request_id,
            response,
        }) = rx.try_recv().unwrap()
        else {
            panic!("expected response")
        };
        assert_eq!(request_id, acp::RequestId::Str("child/request".to_owned()));
        let Ok(acp::ClientResponse::ExtMethodResponse(response)) = *response else {
            panic!("expected extension response")
        };
        let result: serde_json::Value = serde_json::from_str(response.0.get()).unwrap();
        assert_eq!(
            result,
            serde_json::json!({"answers":{"choice":{"answers":["Second"]},"secret":{"answers":["private"]}}})
        );
    }

    #[test]
    fn escape_cancels_the_request() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut view = UserQuestionView::new(request(), AppEventSender::new(tx));
        assert!(matches!(view.on_escape(), CancellationEvent::Handled));
        let AppEvent::HarnessAction(HarnessAction::RespondToAgent { response, .. }) =
            rx.try_recv().unwrap()
        else {
            panic!("expected response")
        };
        let Ok(acp::ClientResponse::ExtMethodResponse(response)) = *response else {
            panic!("expected extension response")
        };
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(response.0.get()).unwrap(),
            serde_json::json!({"cancelled":true})
        );
    }

    #[test]
    fn withdrawal_removes_only_the_matching_request_without_replying() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut view = UserQuestionView::new(request(), AppEventSender::new(tx));
        assert!(!view.withdraw_approval_request(&acp::RequestId::Str("other".to_owned())));
        assert!(view.withdraw_approval_request(&acp::RequestId::Str("child/request".to_owned())));
        assert!(view.is_complete());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn rejects_empty_or_duplicate_question_ids() {
        let mut request = request();
        request.questions[1].id = request.questions[0].id.clone();
        assert!(request.validation_error().is_some());
        request.questions.clear();
        assert!(request.validation_error().is_some());
    }

    #[test]
    fn scrolling_options_preserves_a_custom_answer() {
        use crossterm::event::MouseEvent;
        use crossterm::event::MouseEventKind;
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut view = UserQuestionView::new(request(), AppEventSender::new(tx));
        view.handle_paste("custom answer".to_owned());
        view.handle_mouse_event(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 1,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(view.text, "custom answer");
    }

    #[test]
    fn multiple_choice_returns_every_selected_label() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut request = request();
        request.questions.truncate(1);
        request.questions[0].multiple = true;
        let mut view = UserQuestionView::new(request, AppEventSender::new(tx));
        for code in [
            KeyCode::Char(' '),
            KeyCode::Down,
            KeyCode::Char(' '),
            KeyCode::Enter,
        ] {
            view.handle_key_event(KeyEvent::new(code, KeyModifiers::NONE));
        }
        let AppEvent::HarnessAction(HarnessAction::RespondToAgent { response, .. }) =
            rx.try_recv().unwrap()
        else {
            panic!("expected response")
        };
        let Ok(acp::ClientResponse::ExtMethodResponse(response)) = *response else {
            panic!("expected extension response")
        };
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(response.0.get()).unwrap(),
            serde_json::json!({"answers":{"choice":{"answers":["First","Second"]}}})
        );
    }

    #[test]
    fn touch_selects_option_and_submits_only_on_release() {
        use crossterm::event::MouseButton;
        use crossterm::event::MouseEvent;
        use crossterm::event::MouseEventKind;
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut view = UserQuestionView::new(request(), AppEventSender::new(tx));
        let area = Rect::new(2, 3, 40, 8);
        view.render(area, &mut Buffer::empty(area));
        let mouse = |kind, row| MouseEvent {
            kind,
            column: 4,
            row,
            modifiers: KeyModifiers::NONE,
        };
        view.handle_mouse_event(mouse(MouseEventKind::Down(MouseButton::Left), 7));
        view.handle_mouse_event(mouse(MouseEventKind::Drag(MouseButton::Left), 8));
        view.handle_mouse_event(mouse(MouseEventKind::Up(MouseButton::Left), 7));
        assert_eq!(view.selected, 0);
        view.handle_mouse_event(mouse(MouseEventKind::Down(MouseButton::Left), 7));
        view.handle_mouse_event(mouse(MouseEventKind::Up(MouseButton::Left), 7));
        assert_eq!(view.selected, 1);
        view.handle_mouse_event(mouse(MouseEventKind::Down(MouseButton::Left), 10));
        view.handle_mouse_event(mouse(MouseEventKind::Up(MouseButton::Left), 10));
        assert_eq!(view.current, 1);
    }
}
