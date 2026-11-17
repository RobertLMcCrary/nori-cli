use super::*;
// crossterm types are intentionally not imported here to avoid unused warnings
use rand::prelude::*;

fn rand_grapheme(rng: &mut rand::rngs::StdRng) -> String {
    let r: u8 = rng.random_range(0..100);
    match r {
        0..=4 => "\n".to_string(),
        5..=12 => " ".to_string(),
        13..=35 => (rng.random_range(b'a'..=b'z') as char).to_string(),
        36..=45 => (rng.random_range(b'A'..=b'Z') as char).to_string(),
        46..=52 => (rng.random_range(b'0'..=b'9') as char).to_string(),
        53..=65 => {
            // Some emoji (wide graphemes)
            let choices = ["👍", "😊", "🐍", "🚀", "🧪", "🌟"];
            choices[rng.random_range(0..choices.len())].to_string()
        }
        66..=75 => {
            // CJK wide characters
            let choices = ["漢", "字", "測", "試", "你", "好", "界", "编", "码"];
            choices[rng.random_range(0..choices.len())].to_string()
        }
        76..=85 => {
            // Combining mark sequences
            let base = ["e", "a", "o", "n", "u"][rng.random_range(0..5)];
            let marks = ["\u{0301}", "\u{0308}", "\u{0302}", "\u{0303}"];
            format!("{base}{}", marks[rng.random_range(0..marks.len())])
        }
        86..=92 => {
            // Some non-latin single codepoints (Greek, Cyrillic, Hebrew)
            let choices = ["Ω", "β", "Ж", "ю", "ש", "م", "ह"];
            choices[rng.random_range(0..choices.len())].to_string()
        }
        _ => {
            // ZWJ sequences (single graphemes but multi-codepoint)
            let choices = [
                "👩\u{200D}💻", // woman technologist
                "👨\u{200D}💻", // man technologist
                "🏳️\u{200D}🌈", // rainbow flag
            ];
            choices[rng.random_range(0..choices.len())].to_string()
        }
    }
}

fn ta_with(text: &str) -> TextArea {
    let mut t = TextArea::new();
    t.insert_str(text);
    t
}

// ===== Helper to create a vim-enabled textarea in Normal mode =====
fn vim_normal(text: &str) -> TextArea {
    let mut t = ta_with(text);
    t.set_vim_mode_enabled(true);
    t.enter_vim_normal_mode();
    t
}

fn key(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}

fn shift_key(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::SHIFT)
}

fn esc_key() -> KeyEvent {
    KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
}

mod part1;
mod part2;
mod part3;
mod part4;
mod part5;
mod part6;

#[test]
fn kakoune_selection_first_editing() {
    use pretty_assertions::assert_eq;

    let mut textarea = TextArea::new();
    textarea.set_kakoune_mode_enabled(true);
    for ch in "abc".chars() {
        textarea.input(key(ch));
    }
    textarea.input(esc_key());
    textarea.input(key('h'));
    textarea.input(key('d'));
    assert_eq!(textarea.text(), "ab");
}

#[test]
fn selection_editor_insert_text_has_no_selection_highlight() {
    use ratatui::style::Color;
    use ratatui::style::Modifier;

    let mut textarea = TextArea::new();
    textarea.set_kakoune_mode_enabled(true);
    for ch in "ordinary text".chars() {
        textarea.input(key(ch));
    }
    let area = Rect::new(0, 0, 20, 1);
    let mut buffer = Buffer::empty(area);
    WidgetRef::render_ref(&&textarea, area, &mut buffer);
    for x in 0..13 {
        let cell = &buffer[(x, 0)];
        assert_eq!(cell.bg, Color::Reset);
        assert!(!cell.modifier.contains(Modifier::REVERSED));
    }
}

#[test]
fn kakoune_selection_rendering() {
    use ratatui::style::Modifier;

    let mut textarea = TextArea::new();
    textarea.set_kakoune_mode_enabled(true);
    for ch in "abc".chars() {
        textarea.input(key(ch));
    }
    textarea.input(esc_key());
    textarea.input(shift_key('H'));
    let area = Rect::new(0, 0, 8, 1);
    let mut buf = Buffer::empty(area);
    WidgetRef::render_ref(&&textarea, area, &mut buf);
    let cells = (0..area.width)
        .map(|x| {
            let cell = &buf[(x, 0)];
            let selected = if cell.modifier.contains(Modifier::REVERSED) {
                '*'
            } else {
                ' '
            };
            format!("{}{selected}", cell.symbol())
        })
        .collect::<String>();
    assert!(cells.contains('*'), "selection should be highlighted");
    insta::assert_snapshot!("kakoune_selection", cells);
}

#[test]
fn kakoune_treats_placeholders_as_atomic() {
    use pretty_assertions::assert_eq;

    let mut textarea = TextArea::new();
    textarea.set_kakoune_mode_enabled(true);
    textarea.insert_element("[Pasted Content]");
    textarea.input(esc_key());
    textarea.input(key('h'));
    assert_eq!(textarea.cursor(), 0);
    textarea.input(key('i'));
    textarea.input(key('x'));
    assert_eq!(textarea.text(), "x[Pasted Content]");
    assert_eq!(textarea.elements[0].range, 1..17);
    let mut deletion = TextArea::new();
    deletion.set_kakoune_mode_enabled(true);
    deletion.insert_element("[Pasted Content]");
    deletion.input(esc_key());
    deletion.input(key('h'));
    deletion.input(key('d'));
    assert_eq!(deletion.text(), "");
    assert!(deletion.elements.is_empty());
}

#[test]
fn selection_editor_starts_in_insert_and_supports_helix_selection() {
    use pretty_assertions::assert_eq;
    let mut textarea = TextArea::new();
    textarea.set_kakoune_mode_enabled(true);
    assert_eq!(textarea.vim_mode_state(), VimModeState::Insert);
    for ch in "abc".chars() {
        textarea.input(key(ch));
    }
    textarea.input(esc_key());
    assert_eq!(textarea.vim_mode_state(), VimModeState::Normal);
    textarea.input(key('h'));
    textarea.input(key('v'));
    textarea.input(key('h'));
    textarea.input(key('d'));
    assert_eq!(textarea.text(), "a");
}

#[test]
fn selection_editor_respects_external_cursor_moves_and_unicode() {
    use pretty_assertions::assert_eq;
    let mut textarea = TextArea::new();
    textarea.set_kakoune_mode_enabled(true);
    for ch in "aé🙂z".chars() {
        textarea.input(key(ch));
    }
    textarea.set_cursor("aé".len());
    textarea.input(key('X'));
    assert_eq!(textarea.text(), "aéX🙂z");
    assert_eq!(textarea.cursor(), "aéX".len());
}

#[test]
fn selection_editor_supports_configured_kill_and_yank() {
    use pretty_assertions::assert_eq;
    let mut textarea = TextArea::new();
    textarea.set_kakoune_mode_enabled(true);
    for ch in "hello world".chars() {
        textarea.input(key(ch));
    }
    textarea.set_cursor(5);
    textarea.input(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::ALT));
    assert_eq!(textarea.text(), "hello");
    textarea.input(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL));
    assert_eq!(textarea.text(), "hello world");
    textarea.input(key('!'));
    assert_eq!(textarea.text(), "hello world!");
}

#[test]
fn selection_editor_undo_restores_deleted_selection() {
    use pretty_assertions::assert_eq;
    let mut textarea = TextArea::new();
    textarea.set_kakoune_mode_enabled(true);
    for ch in "abc".chars() {
        textarea.input(key(ch));
    }
    textarea.input(esc_key());
    textarea.input(key('h'));
    textarea.input(key('d'));
    assert_eq!(textarea.text(), "ab");
    textarea.input(key('u'));
    assert_eq!(textarea.text(), "abc");
}

#[test]
fn selection_editor_renders_wrapped_unicode_text() {
    use pretty_assertions::assert_eq;
    let mut textarea = TextArea::new();
    textarea.set_kakoune_mode_enabled(true);
    for ch in "héllo world".chars() {
        textarea.input(key(ch));
    }
    let area = Rect::new(0, 0, 6, 3);
    let mut buf = Buffer::empty(area);
    WidgetRef::render_ref(&&textarea, area, &mut buf);
    let rows: Vec<String> = (0..3)
        .map(|y| (0..6).map(|x| buf[(x, y)].symbol()).collect())
        .collect();
    assert_eq!(rows, vec!["héllo ", "world ", "      "]);
}

#[test]
fn selection_editor_undo_restores_configured_kill() {
    use pretty_assertions::assert_eq;
    let mut textarea = TextArea::new();
    textarea.set_kakoune_mode_enabled(true);
    for ch in "hello world".chars() {
        textarea.input(key(ch));
    }
    textarea.set_cursor(5);
    textarea.input(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::ALT));
    textarea.input(esc_key());
    textarea.input(key('u'));
    assert_eq!(textarea.text(), "hello world");
}

#[test]
fn selection_editor_line_selection_change_and_extend() {
    use pretty_assertions::assert_eq;
    let mut changed = TextArea::new();
    changed.set_kakoune_mode_enabled(true);
    for ch in "first\nsecond".chars() {
        changed.input(if ch == '\n' {
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
        } else {
            key(ch)
        });
    }
    changed.input(esc_key());
    changed.input(key('x'));
    changed.input(key('c'));
    assert_eq!(changed.vim_mode_state(), VimModeState::Insert);
    for ch in "new".chars() {
        changed.input(key(ch));
    }
    assert_eq!(changed.text(), "first\nnew");

    let mut extended = TextArea::new();
    extended.set_kakoune_mode_enabled(true);
    for ch in "first\nsecond\nthird".chars() {
        extended.input(if ch == '\n' {
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
        } else {
            key(ch)
        });
    }
    extended.input(esc_key());
    extended.input(key('x'));
    extended.input(shift_key('X'));
    extended.input(key('d'));
    // X expands the selection to line boundaries; it does not select the
    // preceding line when x already selected the complete current line.
    // Removing the final line also removes its preceding separator.
    assert_eq!(extended.text(), "first\nsecond");
}

#[test]
fn selection_editor_deletes_complete_zwj_grapheme() {
    use pretty_assertions::assert_eq;
    let mut textarea = TextArea::new();
    textarea.set_kakoune_mode_enabled(true);
    for ch in "a👩‍💻z".chars() {
        textarea.input(key(ch));
    }
    textarea.input(esc_key());
    textarea.input(key('h'));
    textarea.input(key('h'));
    textarea.input(key('d'));
    assert_eq!(textarea.text(), "az");
    textarea.input(key('u'));
    assert_eq!(textarea.text(), "a👩‍💻z");
}

#[test]
fn selection_editor_exact_width_caret_and_widened_viewport() {
    use pretty_assertions::assert_eq;
    let mut textarea = TextArea::new();
    textarea.set_kakoune_mode_enabled(true);
    let draft = "draft survives child switch";
    assert_eq!(draft.len(), 27);
    for ch in draft.chars() {
        textarea.input(key(ch));
    }
    assert_eq!(textarea.desired_height(27), 2);
    let narrow = Rect::new(0, 0, 27, 2);
    let mut narrow_buffer = Buffer::empty(narrow);
    WidgetRef::render_ref(&&textarea, narrow, &mut narrow_buffer);
    let rows = (0..2)
        .map(|y| {
            (0..27)
                .map(|x| narrow_buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>();
    assert_eq!(rows[0], draft);
    assert_eq!(textarea.cursor_pos(narrow), Some((0, 1)));
    insta::assert_snapshot!(
        "selection_editor_exact_width_caret",
        format!("|{}|\n|{}|\ncaret=(0, 1)", rows[0], rows[1])
    );

    // A clipped terminal can still force one row. Growing it again must reset
    // the native viewport instead of leaving the draft scrolled offscreen.
    let clipped = Rect::new(0, 0, 27, 1);
    WidgetRef::render_ref(&&textarea, clipped, &mut Buffer::empty(clipped));
    let wide = Rect::new(0, 0, 40, 1);
    let mut wide_buffer = Buffer::empty(wide);
    WidgetRef::render_ref(&&textarea, wide, &mut wide_buffer);
    let row = (0..40)
        .map(|x| wide_buffer[(x, 0)].symbol())
        .collect::<String>();
    assert!(row.starts_with(draft), "{row:?}");
    assert_eq!(textarea.cursor_pos(wide), Some((27, 0)));
    insta::assert_snapshot!("selection_editor_wide_after_exact_width", row);
}
