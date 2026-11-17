//! Lay out main history and its live output within the space above the composer.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::layout_cache::CachedCell;
use super::session_transcript::SessionTranscript;
use crate::render::renderable::Renderable;

pub(super) struct OwnedTranscript<'a> {
    pub(super) transcript: &'a SessionTranscript,
    pub(super) active: Option<&'a CachedCell>,
    pub(super) live: Option<&'a CachedCell>,
    pub(super) live_separator: bool,
}

impl Renderable for OwnedTranscript<'_> {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        let active_rows = self
            .active
            .map_or(0, |cell| cell.desired_height(area.width).saturating_add(1))
            .min(area.height);
        let live_separator = u16::from(self.live_separator);
        let live_rows = self
            .live
            .map_or(0, |cell| {
                cell.desired_height(area.width)
                    .saturating_add(live_separator)
            })
            .min(area.height.saturating_sub(active_rows));
        let history_rows = area
            .height
            .saturating_sub(active_rows)
            .saturating_sub(live_rows);
        let history = Rect::new(
            area.x,
            area.y.saturating_add(1),
            area.width,
            history_rows.saturating_sub(1),
        );
        self.transcript.render(history, buf);
        let active_y = area.y.saturating_add(history_rows);
        if let Some(active) = self.active {
            active.render(
                Rect::new(
                    area.x,
                    active_y.saturating_add(1),
                    area.width,
                    active_rows.saturating_sub(1),
                ),
                buf,
            );
        }
        if let Some(live) = self.live {
            live.render(
                Rect::new(
                    area.x,
                    active_y
                        .saturating_add(active_rows)
                        .saturating_add(live_separator),
                    area.width,
                    live_rows.saturating_sub(live_separator),
                ),
                buf,
            );
        }
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.transcript
            .desired_height(width)
            .saturating_add(1)
            .saturating_add(
                self.active
                    .map_or(0, |cell| cell.desired_height(width).saturating_add(1)),
            )
            .saturating_add(self.live.map_or(0, |cell| {
                cell.desired_height(width)
                    .saturating_add(u16::from(self.live_separator))
            }))
    }

    fn fills_available_height(&self) -> bool {
        true
    }
}
