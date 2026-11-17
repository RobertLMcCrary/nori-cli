//! Pointer targets captured from the same layout that paints the picker.

use ratatui::layout::Rect;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PickerHitTarget<K> {
    Item(K),
    Category(Option<String>),
    Search,
    Close,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PickerHit<K> {
    pub area: Rect,
    pub target: PickerHitTarget<K>,
}
