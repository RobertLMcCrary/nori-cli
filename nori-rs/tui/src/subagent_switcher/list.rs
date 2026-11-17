//! The switcher's list: which rows show, in which order, and which one is selected.
//!
//! The search runs when the query or the identity of a row changes, never per frame. A frame only
//! reads [`ListState::rows`]. The selection is a [`Target`], not an index, so rows that appear,
//! end or reorder around it leave it where it is.

use std::collections::HashMap;
use std::time::Instant;

use codex_common::fuzzy_match::fuzzy_match;

use super::roster::Entry;
use super::roster::Roster;
use super::roster::Target;

/// A match in the secondary text ranks behind one in the title.
const AGENT_PENALTY: i32 = 30;
const MODEL_PENALTY: i32 = 40;
const PARENT_PENALTY: i32 = 50;
const STATUS_PENALTY: i32 = 60;

/// The character positions the query matched in each shown field.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Highlights {
    pub(crate) title: Vec<usize>,
    pub(crate) agent: Vec<usize>,
    pub(crate) model: Vec<usize>,
}

/// One row that passed the filter.
#[derive(Debug, Clone)]
pub(crate) struct Row {
    pub(crate) entry: Entry,
    pub(crate) highlights: Highlights,
}

#[derive(Debug)]
pub(crate) struct ListState {
    /// Every entry in natural order: main, running (most recently active first), then ended.
    entries: Vec<Entry>,
    query: String,
    rows: Vec<Row>,
    selected: Target,
    /// When each running row last did something, as of the first time it was seen. Fixed from
    /// then on, so activity never shuffles rows under the cursor.
    first_seen_active: HashMap<Target, Instant>,
    /// Rows the last frame showed, for paging.
    pub(crate) page: usize,
    pub(crate) scroll_top: usize,
}

impl ListState {
    pub(crate) fn new(roster: &Roster) -> Self {
        let mut state = Self {
            entries: Vec::new(),
            query: String::new(),
            rows: Vec::new(),
            selected: Target::Main,
            first_seen_active: HashMap::new(),
            page: 8,
            scroll_top: 0,
        };
        state.apply(roster);
        state
    }

    pub(crate) fn query(&self) -> &str {
        &self.query
    }

    pub(crate) fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub(crate) fn selected(&self) -> Target {
        self.selected
    }

    pub(crate) fn selected_entry(&self) -> Option<&Entry> {
        self.rows
            .iter()
            .find(|row| row.entry.target == self.selected)
            .map(|row| &row.entry)
    }

    pub(crate) fn selected_index(&self) -> Option<usize> {
        self.rows
            .iter()
            .position(|row| row.entry.target == self.selected)
    }

    pub(crate) fn select_index_at(&mut self, index: usize) {
        self.select_index(index);
    }

    /// Whether the conversation has any subagent at all, for the empty-state hint.
    pub(crate) fn has_subagents(&self) -> bool {
        self.entries.len() > 1
    }

    /// Take a fresh roster. Searches again only when something the search looks at changed.
    pub(crate) fn apply(&mut self, roster: &Roster) {
        let selected = roster.resolve(self.selected);
        let mut fresh: Vec<Entry> = Vec::with_capacity(roster.subagents.len() + 1);
        fresh.push(roster.main.clone());
        fresh.extend(roster.subagents.iter().cloned());
        for entry in &fresh {
            self.first_seen_active
                .entry(entry.target)
                .or_insert(entry.active_at);
        }
        fresh[1..].sort_by(|a, b| self.natural_order(a, b));

        let unchanged = selected == self.selected
            && fresh.len() == self.entries.len()
            && fresh
                .iter()
                .zip(&self.entries)
                .all(|(new, old)| new.same_identity(old));
        if unchanged {
            // Only times and counts moved: refresh them in place, search nothing.
            for row in &mut self.rows {
                if let Some(entry) = fresh.iter().find(|entry| entry.target == row.entry.target) {
                    row.entry = entry.clone();
                }
            }
            self.entries = fresh;
            return;
        }
        self.entries = fresh;
        self.selected = selected;
        self.refilter();
    }

    fn natural_order(&self, a: &Entry, b: &Entry) -> std::cmp::Ordering {
        let class = |entry: &Entry| i32::from(!entry.status.is_running());
        class(a).cmp(&class(b)).then_with(|| {
            if a.status.is_running() {
                let seen = |entry: &Entry| {
                    self.first_seen_active
                        .get(&entry.target)
                        .copied()
                        .unwrap_or(entry.active_at)
                };
                seen(b).cmp(&seen(a))
            } else {
                a.ended_ago.cmp(&b.ended_ago)
            }
        })
    }

    fn refilter(&mut self) {
        let previous = self.selected_index();
        let terms: Vec<&str> = self.query.split_whitespace().collect();
        if terms.is_empty() {
            self.rows = self
                .entries
                .iter()
                .map(|entry| Row {
                    entry: entry.clone(),
                    highlights: Highlights::default(),
                })
                .collect();
        } else {
            let mut ranked: Vec<(i32, Row)> = self
                .entries
                .iter()
                .filter_map(|entry| rank(entry, &terms))
                .collect();
            // Stable: equal scores keep their natural order.
            ranked.sort_by_key(|(score, _)| *score);
            self.rows = ranked.into_iter().map(|(_, row)| row).collect();
        }
        self.keep_selection(previous);
    }

    /// Keep the selected row selected. If the search dropped it, select what is now where it was.
    fn keep_selection(&mut self, previous: Option<usize>) {
        if self
            .rows
            .iter()
            .any(|row| row.entry.target == self.selected)
        {
            return;
        }
        let at = previous.unwrap_or(0).min(self.rows.len().saturating_sub(1));
        self.selected = self
            .rows
            .get(at)
            .map_or(Target::Main, |row| row.entry.target);
    }

    pub(crate) fn push_char(&mut self, ch: char) {
        self.query.push(ch);
        self.after_query_change();
    }

    pub(crate) fn pop_char(&mut self) {
        if self.query.pop().is_some() {
            self.after_query_change();
        }
    }

    /// Delete the last word of the query, and the spaces before it.
    pub(crate) fn delete_word(&mut self) {
        let kept = self
            .query
            .trim_end()
            .trim_end_matches(|ch: char| !ch.is_whitespace())
            .len();
        if kept < self.query.len() {
            self.query.truncate(kept);
            self.after_query_change();
        }
    }

    pub(crate) fn clear_query(&mut self) {
        if !self.query.is_empty() {
            self.query.clear();
            self.after_query_change();
        }
    }

    fn after_query_change(&mut self) {
        self.refilter();
        // Typing starts at the best match. Emptying the query leaves the selection where it is.
        if !self.query.is_empty() {
            if let Some(best) = self.rows.first() {
                self.selected = best.entry.target;
            }
            self.scroll_top = 0;
        }
    }

    /// Move the selection by `delta` rows, wrapping around the ends for single steps.
    pub(crate) fn step(&mut self, delta: isize) {
        let len = self.rows.len();
        if len == 0 {
            return;
        }
        let current = self.selected_index().unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(len as isize) as usize;
        self.select_index(next);
    }

    /// Move by `delta` rows and stop at the ends.
    pub(crate) fn jump(&mut self, delta: isize) {
        let len = self.rows.len();
        if len == 0 {
            return;
        }
        let current = self.selected_index().unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, len as isize - 1) as usize;
        self.select_index(next);
    }

    pub(crate) fn first(&mut self) {
        self.select_index(0);
    }

    pub(crate) fn last(&mut self) {
        self.select_index(self.rows.len().saturating_sub(1));
    }

    fn select_index(&mut self, index: usize) {
        if let Some(row) = self.rows.get(index) {
            self.selected = row.entry.target;
        }
    }

    /// Scroll so the selected row is inside `height` rows, moving as little as possible.
    pub(crate) fn scroll_to_selection(&mut self, height: usize) {
        self.page = height.max(1);
        let Some(selected) = self.selected_index() else {
            self.scroll_top = 0;
            return;
        };
        if selected < self.scroll_top {
            self.scroll_top = selected;
        } else if height > 0 && selected >= self.scroll_top + height {
            self.scroll_top = selected + 1 - height;
        }
        self.scroll_top = self
            .scroll_top
            .min(self.rows.len().saturating_sub(height.max(1)));
    }
}

/// Match every term in at least one field. The score is the sum of each term's best field.
fn rank(entry: &Entry, terms: &[&str]) -> Option<(i32, Row)> {
    let mut highlights = Highlights::default();
    let mut total = 0;
    for term in terms {
        let mut best: Option<i32> = None;
        let mut consider = |score: i32| {
            best = Some(best.map_or(score, |best| best.min(score)));
        };
        if let Some((indices, score)) = fuzzy_match(&entry.title, term) {
            merge(&mut highlights.title, indices);
            consider(score);
        }
        if let Some(agent) = &entry.agent
            && let Some((indices, score)) = fuzzy_match(agent, term)
        {
            merge(&mut highlights.agent, indices);
            consider(score + AGENT_PENALTY);
        }
        if let Some(model) = &entry.model
            && let Some((indices, score)) = fuzzy_match(model, term)
        {
            merge(&mut highlights.model, indices);
            consider(score + MODEL_PENALTY);
        }
        if let Some(parent) = &entry.parent
            && let Some((_, score)) = fuzzy_match(parent, term)
        {
            consider(score + PARENT_PENALTY);
        }
        if let Some((_, score)) = fuzzy_match(entry.status.word(), term) {
            consider(score + STATUS_PENALTY);
        }
        total += best?;
    }
    Some((
        total,
        Row {
            entry: entry.clone(),
            highlights,
        },
    ))
}

fn merge(into: &mut Vec<usize>, indices: Vec<usize>) {
    into.extend(indices);
    into.sort_unstable();
    into.dedup();
}
