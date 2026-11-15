pub fn requery_default_colors() {
    imp::refresh_colors(true);
}

/// Take the colors a previous run saved at `path`, so the first frame already has them. The
/// terminal is not asked here: a new tab can take two to three seconds to answer, and the first
/// paint must not wait for that.
pub fn load_cached_colors(path: &std::path::Path) {
    imp::load_cached_colors(path);
}

/// How long after launch the terminal is first asked for its colors.
pub(crate) const REFRESH_DELAY: std::time::Duration = std::time::Duration::from_secs(4);

/// Ask the terminal for its colors, and save the answer for the next run's first frame. True when
/// the background differs from what the frames used so far, so they should be drawn again. A
/// terminal that did not answer last time is not asked again at launch: the question holds up
/// input for as long as it waits.
pub(crate) fn refresh_colors() -> bool {
    imp::refresh_colors(false)
}

#[derive(Clone, Copy)]
pub struct DefaultColors {
    fg: (u8, u8, u8),
    bg: (u8, u8, u8),
}

#[cfg(any(test, unix))]
/// A text color that reads on `bg`: light on a dark background, dark on a light one.
fn contrasting_fg((r, g, b): (u8, u8, u8)) -> (u8, u8, u8) {
    let luminance = (299 * u32::from(r) + 587 * u32::from(g) + 114 * u32::from(b)) / 1000;
    if luminance < 128 {
        (204, 204, 204)
    } else {
        (51, 51, 51)
    }
}

pub fn default_colors() -> Option<DefaultColors> {
    imp::default_colors()
}

pub fn default_fg() -> Option<(u8, u8, u8)> {
    default_colors().map(|colors| colors.fg)
}

pub fn default_bg() -> Option<(u8, u8, u8)> {
    default_colors().map(|colors| colors.bg)
}

#[cfg(all(unix, not(test)))]
mod imp {
    use std::path::Path;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::OnceLock;

    use crossterm::style::Color as CrosstermColor;
    use crossterm::style::query_background_color;

    use super::DefaultColors;

    #[derive(Default)]
    struct Known {
        colors: Option<DefaultColors>,
        /// Where the background is saved for the next run's first frame.
        saved_at: Option<PathBuf>,
    }

    fn known() -> &'static Mutex<Known> {
        static KNOWN: OnceLock<Mutex<Known>> = OnceLock::new();
        KNOWN.get_or_init(|| Mutex::new(Known::default()))
    }

    /// What is known now: the saved colors or a later answer. Never asks the terminal, so a
    /// slow one cannot hold up a frame.
    pub(super) fn default_colors() -> Option<DefaultColors> {
        known().lock().ok()?.colors
    }

    pub(super) fn load_cached_colors(path: &Path) {
        if let Ok(mut known) = known().lock() {
            known.colors = read_saved(path);
            known.saved_at = Some(path.to_path_buf());
        }
    }

    /// `asked_for` is a request made on purpose (the window regained focus); at launch only a
    /// terminal that has answered before is asked.
    pub(super) fn refresh_colors(asked_for: bool) -> bool {
        if !asked_for && known().lock().is_ok_and(|known| known.colors.is_none()) {
            return false;
        }
        let answer = query_default_colors();
        tracing::debug!(answered = ?answer.as_ref().map(Option::is_some), "terminal colors asked");
        let Ok(Some(colors)) = answer else {
            return false;
        };
        let Ok(mut known) = known().lock() else {
            return false;
        };
        let differs = known.colors.is_none_or(|known| known.bg != colors.bg);
        known.colors = Some(colors);
        if let Some(path) = &known.saved_at {
            let (r, g, b) = colors.bg;
            let _ = std::fs::write(path, format!("{r} {g} {b}"));
        }
        differs
    }

    fn read_saved(path: &Path) -> Option<DefaultColors> {
        let text = std::fs::read_to_string(path).ok()?;
        let mut parts = text.split_whitespace().map(|part| part.parse::<u8>().ok());
        let bg = (parts.next()??, parts.next()??, parts.next()??);
        Some(DefaultColors {
            fg: super::contrasting_fg(bg),
            bg,
        })
    }

    /// Only the background is asked for. A terminal that does not answer the foreground query
    /// (Rex is one) would hold the caller for crossterm's whole two-second timeout, and the
    /// foreground only ever tints the shimmer, so a legible one is derived from the background.
    fn query_default_colors() -> std::io::Result<Option<DefaultColors>> {
        let bg = query_background_color()?.and_then(color_to_tuple);
        Ok(bg.map(|bg| DefaultColors {
            fg: super::contrasting_fg(bg),
            bg,
        }))
    }

    fn color_to_tuple(color: CrosstermColor) -> Option<(u8, u8, u8)> {
        match color {
            CrosstermColor::Rgb { r, g, b } => Some((r, g, b)),
            _ => None,
        }
    }
}

#[cfg(not(all(unix, not(test))))]
mod imp {
    use super::DefaultColors;

    pub(super) fn default_colors() -> Option<DefaultColors> {
        None
    }

    pub(super) fn load_cached_colors(_path: &std::path::Path) {}

    pub(super) fn refresh_colors(_asked_for: bool) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::contrasting_fg;

    #[test]
    fn the_derived_foreground_reads_on_dark_and_light_backgrounds() {
        assert_eq!(contrasting_fg((20, 20, 20)), (204, 204, 204));
        assert_eq!(contrasting_fg((250, 250, 250)), (51, 51, 51));
    }
}
