//! Brief, unobtrusive messages in the top right of the screen.
//!
//! A toast is drawn over the right end of the topmost rows Nori itself wrote above the prompt,
//! so what it covers can always be written back from Nori's record of those rows. It is drawn
//! again after every frame while it lives, and when it ends the next frame repaints the rows,
//! which erases it. Nothing is added to the history, and nothing is drawn where Nori has not
//! written (the shell's rows above it), so a toast never leaves a mark behind.

use std::io;
use std::io::Write;
use std::time::Duration;
use std::time::Instant;

use crossterm::cursor::MoveTo;
use crossterm::cursor::RestorePosition;
use crossterm::cursor::SavePosition;
use crossterm::queue;
use crossterm::style::Attribute;
use crossterm::style::Print;
use crossterm::style::SetAttribute;
use crossterm::style::SetForegroundColor;
use unicode_width::UnicodeWidthStr;

/// How long a toast stays.
pub(crate) const TOAST_LIFETIME: Duration = Duration::from_secs(4);

/// Toasts shown at once; a new one pushes the oldest out.
const MAX_TOASTS: usize = 3;

/// The most of a message that is shown, in columns.
const MAX_TEXT_COLUMNS: usize = 44;

/// Small marks to put in front, picked by the text so one message always gets the same one.
const SPARKLES: [&str; 5] = ["✦", "✧", "❀", "♪", "☾"];

#[derive(Debug)]
struct Toast {
    text: String,
    shown_at: Instant,
}

/// The toasts on screen.
#[derive(Debug, Default)]
pub(crate) struct Toasts {
    items: Vec<Toast>,
}

impl Toasts {
    pub(crate) fn push(&mut self, text: String, now: Instant) {
        // The same message again just starts its time over.
        self.items.retain(|toast| toast.text != text);
        self.items.push(Toast {
            text,
            shown_at: now,
        });
        if self.items.len() > MAX_TOASTS {
            self.items.remove(0);
        }
    }

    /// Drop the toasts whose time is up, and say whether any went.
    pub(crate) fn expire(&mut self, now: Instant) -> bool {
        let before = self.items.len();
        self.items
            .retain(|toast| now.duration_since(toast.shown_at) < TOAST_LIFETIME);
        self.items.len() != before
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// The words a toast shows: a sparkle, then the message cut to fit.
fn label(text: &str) -> String {
    let sparkle = SPARKLES[text.bytes().map(usize::from).sum::<usize>() % SPARKLES.len()];
    let mut shown = String::new();
    let mut columns = 0;
    for c in text.chars().filter(|c| !c.is_control()) {
        let width = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if columns + width > MAX_TEXT_COLUMNS {
            shown.push('…');
            break;
        }
        shown.push(c);
        columns += width;
    }
    format!(" {sparkle} {shown} ")
}

/// Queue drawing every toast, newest last, from `first_row` down over `rows` rows, flush against
/// the right edge of a `width`-column screen. The cursor is left where it was.
pub(crate) fn draw(
    writer: &mut impl Write,
    toasts: &Toasts,
    first_row: u16,
    rows: u16,
    width: u16,
) -> io::Result<()> {
    if toasts.items.is_empty() || rows == 0 {
        return Ok(());
    }
    queue!(writer, SavePosition)?;
    for (index, toast) in toasts
        .items
        .iter()
        .rev()
        .take(usize::from(rows))
        .enumerate()
    {
        let text = label(&toast.text);
        let columns = u16::try_from(UnicodeWidthStr::width(text.as_str())).unwrap_or(width);
        let column = width.saturating_sub(columns.saturating_add(1));
        queue!(
            writer,
            MoveTo(column, first_row + u16::try_from(index).unwrap_or(0)),
            SetAttribute(Attribute::Reverse),
            SetForegroundColor(crossterm::style::Color::Cyan),
            Print(text),
            SetAttribute(Attribute::Reset),
        )?;
    }
    queue!(writer, RestorePosition)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn a_toast_lives_for_its_lifetime_and_then_goes() {
        let start = Instant::now();
        let mut toasts = Toasts::default();
        toasts.push("hello".into(), start);
        assert!(!toasts.expire(start + Duration::from_secs(1)));
        assert!(!toasts.is_empty());
        assert!(toasts.expire(start + TOAST_LIFETIME + Duration::from_millis(1)));
        assert!(toasts.is_empty());
    }

    #[test]
    fn the_same_message_restarts_instead_of_stacking_and_only_a_few_show() {
        let start = Instant::now();
        let mut toasts = Toasts::default();
        toasts.push("same".into(), start);
        toasts.push("same".into(), start + Duration::from_secs(3));
        assert_eq!(toasts.items.len(), 1);
        assert!(
            !toasts.expire(start + Duration::from_secs(5)),
            "restarted at 3s"
        );

        for n in 0..6 {
            toasts.push(format!("t{n}"), start);
        }
        assert_eq!(toasts.items.len(), MAX_TOASTS);
        assert_eq!(toasts.items.last().map(|t| t.text.as_str()), Some("t5"));
    }

    #[test]
    fn a_long_message_is_cut_with_an_ellipsis_and_gets_a_stable_sparkle() {
        let long = "x".repeat(100);
        let text = label(&long);
        assert!(text.ends_with("… "), "{text:?}");
        assert!(UnicodeWidthStr::width(text.as_str()) <= MAX_TEXT_COLUMNS + 6);
        assert_eq!(label("title"), label("title"));
    }

    #[test]
    fn drawing_goes_to_the_right_edge_and_puts_the_cursor_back() {
        let mut toasts = Toasts::default();
        toasts.push("hi".into(), Instant::now());
        let mut bytes = Vec::new();
        draw(&mut bytes, &toasts, 4, 2, 80).expect("draw");
        let text = String::from_utf8(bytes).expect("utf8");
        assert!(
            text.starts_with("\u{1b}7"),
            "saves the cursor first: {text:?}"
        );
        assert!(text.ends_with("\u{1b}8"), "puts it back last: {text:?}");
        assert!(text.contains("hi"));
        // " ✦ hi " is six columns; flush right leaves one column of margin: column 73 (1-based 74).
        assert!(text.contains("\u{1b}[5;74H"), "{text:?}");
    }

    #[test]
    fn nothing_is_drawn_without_toasts_or_rows() {
        let mut bytes = Vec::new();
        draw(&mut bytes, &Toasts::default(), 0, 3, 80).expect("draw");
        assert!(bytes.is_empty());
        let mut toasts = Toasts::default();
        toasts.push("x".into(), Instant::now());
        draw(&mut bytes, &toasts, 0, 0, 80).expect("draw");
        assert!(bytes.is_empty());
    }
}
