//! Keep the draft bar anchored while a searchable picker occupies the space above it.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::BottomPaneView;
use super::ChatComposer;
use crate::render::renderable::Renderable;

pub(super) struct PopupLayout<'a> {
    pub(super) view: &'a dyn BottomPaneView,
    pub(super) composer: &'a ChatComposer,
}

impl Renderable for PopupLayout<'_> {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        let composer_height = self.composer.desired_height(area.width).min(area.height);
        let popup_height = area.height.saturating_sub(composer_height);
        self.view
            .render(Rect::new(area.x, area.y, area.width, popup_height), buf);
        self.composer.render(
            Rect::new(area.x, area.y + popup_height, area.width, composer_height),
            buf,
        );
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.view
            .desired_height(width)
            .saturating_add(self.composer.desired_height(width))
    }

    fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        let height = area
            .height
            .saturating_sub(self.composer.desired_height(area.width));
        self.view
            .cursor_pos(Rect::new(area.x, area.y, area.width, height))
    }
}
