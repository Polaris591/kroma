use super::*;

impl UiState {
    pub fn open_plugin_panel(&mut self, content: String) {
        let (title, body) = content.split_once("\n\n").unwrap_or(("Plugin", &content));
        self.plugin_panel = Some(PluginPanel {
            placement: PanelPlacement::Centered,
            search: None,
            title: title.to_owned(),
            body: body.to_owned(),
            scroll: 0,
            wrapped_width: 0,
            wrapped: Text::default(),
        });
        self.picker = None;
    }

    pub fn has_plugin_panel(&self) -> bool {
        self.plugin_panel.is_some()
    }

    pub fn close_plugin_panel(&mut self) {
        self.plugin_panel = None;
    }

    pub fn scroll_plugin_panel(&mut self, offset: i32) {
        if let Some(panel) = &mut self.plugin_panel {
            panel.scroll = if offset < 0 {
                panel.scroll.saturating_sub(offset.unsigned_abs() as u16)
            } else {
                panel.scroll.saturating_add(offset as u16)
            };
        }
    }

    /// Declarative UI-only slots; plugins never own widgets or terminal input.
    pub fn set_plugin_presentation(
        &mut self,
        activity_frames: Vec<String>,
        tool_labels: BTreeMap<String, String>,
        layout: Option<PluginLayoutSpec>,
        colors: BTreeMap<String, String>,
    ) {
        self.plugin_activity_frames = activity_frames;
        self.plugin_tool_labels = tool_labels;
        self.plugin_layout = layout;
        self.plugin_colors = colors
            .into_iter()
            .filter_map(|(slot, value)| {
                crate::tui::theme::parse_hex(&value)
                    .ok()
                    .map(|color| (slot, color))
            })
            .collect();
        self.activity_frame = 0;
        self.history_cache = None;
    }
}
