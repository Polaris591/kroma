//! Transcript hit testing and turn navigation share the wrapped source metadata.
use super::*;

impl UiState {
    /// Move by user turns without touching the editor or model history.
    pub(in crate::tui) fn jump_turn(&mut self, previous: bool) {
        if self.picker.is_some() || self.plugin_panel.is_some() || self.attach_mode {
            return;
        }
        let turns = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| matches!(entry.kind, Kind::User))
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        let current = self.navigation_turn.or_else(|| {
            self.history_cache
                .as_ref()?
                .decor
                .iter()
                .rev()
                .find_map(|(row, item)| match item {
                    Decor::TurnStart(index) if *row <= self.history_top_row => Some(*index),
                    _ => None,
                })
        });
        let target = if previous {
            match current {
                Some(index) if self.navigation_turn.is_some() => turns
                    .iter()
                    .rev()
                    .copied()
                    .find(|turn| *turn < index)
                    .or_else(|| turns.first().copied()),
                Some(index) => Some(index),
                None => turns.last().copied(),
            }
        } else {
            current
                .and_then(|index| turns.iter().copied().find(|turn| *turn > index))
                .or_else(|| turns.last().copied())
        };
        let Some(target) = target else {
            return;
        };
        self.navigation_turn = Some(target);
        self.scroll_anchor = Some(self.history_top_row);
        self.pending_scroll_turn = Some(target);
        let needed = self.entries.len() - target;
        if needed > self.history_visible {
            self.history_visible = needed;
            self.history_cache = None;
        }
    }

    /// Return raw fenced code only when the mouse hits the painted copy label.
    pub(in crate::tui) fn code_at(&self, column: u16, row: u16) -> Option<&str> {
        if !self.selection_area.contains((column, row).into()) {
            return None;
        }
        let wrapped = self.history_top_row + usize::from(row - self.selection_area.y);
        self.history_cache
            .as_ref()?
            .decor
            .iter()
            .find_map(|(line, item)| {
                let Decor::CodeLabel(block) = item else {
                    return None;
                };
                let label = self.code_label(block, self.selection_area.width);
                let size = UnicodeWidthStr::width(label.as_str())
                    .min(self.selection_area.width.saturating_sub(2) as usize)
                    as u16;
                let margin = if self.plain_style {
                    PLAIN_RIGHT_GUTTER
                } else {
                    1
                };
                let end = self.selection_area.right().saturating_sub(margin);
                (*line == wrapped && column >= end.saturating_sub(size) && column < end)
                    .then_some(block.raw.as_str())
            })
    }
    pub(super) fn code_label(&self, block: &markdown::CodeBlock, width: u16) -> String {
        let copy = self.preferences.language.text("⧉ Copy", "⧉ 复制");
        let label = if block.language.is_empty() {
            copy.into()
        } else {
            format!("{} · {copy}", block.language)
        };
        ellipsize_cells(&label, width.saturating_sub(4) as usize)
    }
}
