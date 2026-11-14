//! The colours of a diff: line tints by terminal theme and colour depth, and the stronger
//! emphasis the changed words of an edited line get on top of them.

use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DiffColorLevel {
    TrueColor,
    Ansi256,
    Ansi16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DiffTheme {
    Dark,
    Light,
}

// Hardcoded palette constants. The emphasis tints are the line tints made stronger, as GitHub
// marks the changed words of a line.
const DARK_TC_ADD_BG: (u8, u8, u8) = (33, 58, 43);
const DARK_TC_DEL_BG: (u8, u8, u8) = (74, 34, 29);
const LIGHT_TC_ADD_BG: (u8, u8, u8) = (218, 251, 225);
const LIGHT_TC_DEL_BG: (u8, u8, u8) = (255, 235, 233);
const DARK_TC_ADD_EMPHASIS_BG: (u8, u8, u8) = (40, 92, 56);
const DARK_TC_DEL_EMPHASIS_BG: (u8, u8, u8) = (118, 46, 40);
const LIGHT_TC_ADD_EMPHASIS_BG: (u8, u8, u8) = (172, 238, 187);
const LIGHT_TC_DEL_EMPHASIS_BG: (u8, u8, u8) = (255, 206, 203);

const DARK_256_ADD_BG: u8 = 22;
const DARK_256_DEL_BG: u8 = 52;
const LIGHT_256_ADD_BG: u8 = 194;
const LIGHT_256_DEL_BG: u8 = 224;
const DARK_256_ADD_EMPHASIS_BG: u8 = 28;
const DARK_256_DEL_EMPHASIS_BG: u8 = 88;
const LIGHT_256_ADD_EMPHASIS_BG: u8 = 157;
const LIGHT_256_DEL_EMPHASIS_BG: u8 = 217;

fn diff_color_level() -> DiffColorLevel {
    let Some(level) = supports_color::on_cached(supports_color::Stream::Stdout) else {
        return DiffColorLevel::Ansi16;
    };
    if level.has_16m {
        DiffColorLevel::TrueColor
    } else if level.has_256 {
        DiffColorLevel::Ansi256
    } else {
        DiffColorLevel::Ansi16
    }
}

fn diff_theme() -> DiffTheme {
    match crate::terminal_palette::default_bg() {
        Some(bg) if crate::color::is_light(bg) => DiffTheme::Light,
        _ => DiffTheme::Dark,
    }
}

/// How strongly a tint marks its line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tint {
    Line,
    Emphasis,
}

fn resolve_bg(theme: DiffTheme, level: DiffColorLevel, is_add: bool, tint: Tint) -> Option<Color> {
    match level {
        DiffColorLevel::TrueColor => {
            let (r, g, b) = match (theme, is_add, tint) {
                (DiffTheme::Dark, true, Tint::Line) => DARK_TC_ADD_BG,
                (DiffTheme::Dark, false, Tint::Line) => DARK_TC_DEL_BG,
                (DiffTheme::Light, true, Tint::Line) => LIGHT_TC_ADD_BG,
                (DiffTheme::Light, false, Tint::Line) => LIGHT_TC_DEL_BG,
                (DiffTheme::Dark, true, Tint::Emphasis) => DARK_TC_ADD_EMPHASIS_BG,
                (DiffTheme::Dark, false, Tint::Emphasis) => DARK_TC_DEL_EMPHASIS_BG,
                (DiffTheme::Light, true, Tint::Emphasis) => LIGHT_TC_ADD_EMPHASIS_BG,
                (DiffTheme::Light, false, Tint::Emphasis) => LIGHT_TC_DEL_EMPHASIS_BG,
            };
            #[allow(clippy::disallowed_methods)]
            Some(Color::Rgb(r, g, b))
        }
        DiffColorLevel::Ansi256 => {
            let idx = match (theme, is_add, tint) {
                (DiffTheme::Dark, true, Tint::Line) => DARK_256_ADD_BG,
                (DiffTheme::Dark, false, Tint::Line) => DARK_256_DEL_BG,
                (DiffTheme::Light, true, Tint::Line) => LIGHT_256_ADD_BG,
                (DiffTheme::Light, false, Tint::Line) => LIGHT_256_DEL_BG,
                (DiffTheme::Dark, true, Tint::Emphasis) => DARK_256_ADD_EMPHASIS_BG,
                (DiffTheme::Dark, false, Tint::Emphasis) => DARK_256_DEL_EMPHASIS_BG,
                (DiffTheme::Light, true, Tint::Emphasis) => LIGHT_256_ADD_EMPHASIS_BG,
                (DiffTheme::Light, false, Tint::Emphasis) => LIGHT_256_DEL_EMPHASIS_BG,
            };
            #[allow(clippy::disallowed_methods)]
            Some(Color::Indexed(idx))
        }
        DiffColorLevel::Ansi16 => None,
    }
}

pub(super) struct DiffRenderStyleContext {
    pub(super) add_bg: Option<Color>,
    pub(super) del_bg: Option<Color>,
    /// What the changed words of an added line that has a removed counterpart are drawn with,
    /// over its syntax colours: a stronger tint, or where there are no backgrounds, bold and
    /// underline.
    pub(super) add_emphasis: Style,
    pub(super) del_emphasis: Style,
}

impl DiffRenderStyleContext {
    pub(super) fn new(theme: DiffTheme, level: DiffColorLevel) -> Self {
        let emphasis = |is_add| match resolve_bg(theme, level, is_add, Tint::Emphasis) {
            Some(bg) => Style::default().bg(bg),
            None => Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        };
        Self {
            add_bg: resolve_bg(theme, level, true, Tint::Line),
            del_bg: resolve_bg(theme, level, false, Tint::Line),
            add_emphasis: emphasis(true),
            del_emphasis: emphasis(false),
        }
    }

    /// The context for the terminal the TUI is drawing on.
    pub(super) fn detected() -> Self {
        Self::new(diff_theme(), diff_color_level())
    }
}

pub(super) fn style_gutter() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

pub(super) fn style_context() -> Style {
    Style::default()
}

pub(super) fn style_add(ctx: &DiffRenderStyleContext) -> Style {
    let mut s = Style::default().fg(Color::Green);
    if let Some(bg) = ctx.add_bg {
        s = s.bg(bg);
    }
    s
}

pub(super) fn style_del(ctx: &DiffRenderStyleContext) -> Style {
    let mut s = Style::default().fg(Color::Red);
    if let Some(bg) = ctx.del_bg {
        s = s.bg(bg);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn diff_style_add_has_green_fg() {
        let ctx = DiffRenderStyleContext::new(DiffTheme::Dark, DiffColorLevel::Ansi16);
        let style = style_add(&ctx);
        assert_eq!(style.fg, Some(Color::Green));
    }

    #[test]
    fn diff_style_del_has_red_fg() {
        let ctx = DiffRenderStyleContext::new(DiffTheme::Dark, DiffColorLevel::Ansi16);
        let style = style_del(&ctx);
        assert_eq!(style.fg, Some(Color::Red));
    }

    #[test]
    fn emphasis_is_a_stronger_tint_or_bold_underline_without_backgrounds() {
        #[allow(clippy::disallowed_methods)]
        let truecolor = (
            Style::default().bg(Color::Rgb(40, 92, 56)),
            Style::default().bg(Color::Rgb(118, 46, 40)),
        );
        let ctx = DiffRenderStyleContext::new(DiffTheme::Dark, DiffColorLevel::TrueColor);
        assert_eq!((ctx.add_emphasis, ctx.del_emphasis), truecolor);
        assert_ne!(ctx.add_emphasis.bg, ctx.add_bg);

        let bold_underline = Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
        let ctx = DiffRenderStyleContext::new(DiffTheme::Light, DiffColorLevel::Ansi16);
        assert_eq!(
            (ctx.add_bg, ctx.add_emphasis, ctx.del_emphasis),
            (None, bold_underline, bold_underline)
        );
    }

    #[test]
    fn diff_color_level_detection() {
        // Just verify it returns without panicking; the actual value depends on
        // the test runner's terminal capabilities.
        let _level = diff_color_level();
    }

    #[test]
    fn diff_theme_defaults_to_dark() {
        // In test environments, default_bg() returns None, so diff_theme()
        // should fall back to Dark.
        assert_eq!(diff_theme(), DiffTheme::Dark);
    }
}
