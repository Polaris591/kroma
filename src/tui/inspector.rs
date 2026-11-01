//! Read-only tool inspection shares the existing panel and authoritative entries.
use super::super::picker::{Item, Kind as PickerKind};
use super::*;

/// Narrow terminals keep the operation readable; /inspect remains available.
pub(super) const MIN_ACTION_WIDTH: usize = 60;

impl UiState {
    /// Browse recorded calls without replacing the unsent draft.
    pub(in crate::tui) fn browse_execution(&mut self) {
        let items = self
            .entries
            .iter()
            .rev()
            .filter(|entry| matches!(entry.kind, Kind::Tool))
            .filter_map(|entry| {
                let id = entry.call_id.as_ref()?;
                Some(Item::new(
                    self.tool_summary(entry),
                    format!("/inspect {id}"),
                    &entry.title,
                ))
            })
            .collect::<Vec<_>>();
        if items.is_empty() {
            self.note(
                self.preferences
                    .language
                    .text("No recorded tool calls", "暂无工具执行记录"),
            );
            return;
        }
        let mut picker = Picker::new(PickerKind::Commands, items, "");
        picker.preserve_order = true;
        picker.title = Some(
            self.preferences
                .language
                .text("Execution records", "执行记录")
                .into(),
        );
        self.picker = Some(picker);
    }

    /// Inspect a snapshot of the actual command and output, without rerunning it.
    pub(in crate::tui) fn inspect_tool(&mut self, id: &str) {
        let Some(entry) =
            self.entries.iter().rev().find(|entry| {
                matches!(entry.kind, Kind::Tool) && entry.call_id.as_deref() == Some(id)
            })
        else {
            self.note(
                self.preferences
                    .language
                    .text("Tool record is unavailable", "该执行记录不可用"),
            );
            return;
        };
        let language = self.preferences.language;
        let title = format!(
            "{} · {}",
            language.text("Snapshot", "执行快照"),
            self.tool_summary(entry)
        );
        let content = format!(
            "{title}\n\n{}\n{}\n\n{}\n{}",
            language.text("Command", "执行命令"),
            entry.body,
            language.text("Output", "执行输出"),
            if entry.details.is_empty() {
                language.text("No output recorded", "暂无已记录输出")
            } else {
                &entry.details
            }
        );
        self.open_plugin_panel(content);
        if let Some(panel) = &mut self.plugin_panel {
            panel.placement = PanelPlacement::Docked;
            panel.search = Some(transcript_panel::Search::default());
        }
    }

    pub(super) fn inspection_label(&self) -> &'static str {
        self.preferences.language.text("[view]", "[检视]")
    }

    /// Match the painted inspection action, not the underlying command text.
    pub(in crate::tui) fn inspection_at(&self, column: u16, row: u16) -> Option<&str> {
        if self.plugin_panel.is_some()
            || self.picker.is_some()
            || !self.selection_area.contains((column, row).into())
        {
            return None;
        }
        let width = UnicodeWidthStr::width(self.inspection_label()) as u16;
        if column < self.selection_area.right().saturating_sub(width + 1)
            || column >= self.selection_area.right().saturating_sub(1)
        {
            return None;
        }
        let wrapped = self.history_top_row + usize::from(row - self.selection_area.y);
        self.history_cache
            .as_ref()?
            .decor
            .iter()
            .find_map(|(line, item)| match item {
                Decor::Inspect(id) if *line == wrapped => Some(id.as_str()),
                _ => None,
            })
    }
}
