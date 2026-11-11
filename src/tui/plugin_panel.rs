use super::*;
use crate::plugins::{
    CustomTuiChannel, CustomTuiClientEvent, CustomTuiFrameUpdate, CustomTuiHost, CustomTuiSpan,
    CustomTuiSpec, PluginWidget, WidgetPlacement,
};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use std::sync::{mpsc as std_mpsc, Arc, Mutex};
use tokio::sync::Notify;

#[derive(Clone, Default)]
pub struct CustomTuiSlot {
    pending: Arc<Mutex<Option<CustomTuiChannel>>>,
}

impl CustomTuiSlot {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn take(&self) -> Option<CustomTuiChannel> {
        self.pending.lock().ok()?.take()
    }
}

impl CustomTuiHost for CustomTuiSlot {
    fn open(&self, channel: CustomTuiChannel) -> bool {
        let Ok(mut slot) = self.pending.lock() else {
            return false;
        };
        if slot.is_some() {
            return false;
        }
        *slot = Some(channel);
        true
    }
}

pub(super) struct CustomTuiOverlay {
    spec: CustomTuiSpec,
    client_tx: std_mpsc::Sender<CustomTuiClientEvent>,
    frame_rx: std_mpsc::Receiver<CustomTuiFrameUpdate>,
    wake: Arc<Notify>,
    lines: Vec<Vec<CustomTuiSpan>>,
    cursor: Option<(u16, u16)>,
    last_inner_size: (u16, u16),
}

fn custom_theme_json(colors: Theme) -> serde_json::Value {
    let hex = |c: Color| match c {
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        _ => "reset".to_owned(),
    };
    serde_json::json!({
        "background": hex(colors.background),
        "surface": hex(colors.surface),
        "text": hex(colors.text),
        "muted": hex(colors.muted),
        "accent": hex(colors.accent),
        "border": hex(colors.border),
        "error": hex(colors.error),
        "warning": hex(colors.warning),
        "success": hex(colors.success),
    })
}

fn resolve_span_color(raw: Option<&str>, colors: Theme) -> Option<Color> {
    let raw = raw?.trim();
    match raw {
        "" => None,
        "text" => Some(colors.text),
        "muted" => Some(colors.muted),
        "accent" => Some(colors.accent),
        "error" => Some(colors.error),
        "warning" => Some(colors.warning),
        "success" => Some(colors.success),
        "background" => Some(colors.background),
        "surface" => Some(colors.surface),
        "border" => Some(colors.border),
        hex => crate::tui::theme::parse_hex(hex).ok(),
    }
}

fn key_name(code: KeyCode) -> Option<(&'static str, Option<String>)> {
    match code {
        KeyCode::Esc => Some(("Esc", None)),
        KeyCode::Enter => Some(("Enter", None)),
        KeyCode::Tab => Some(("Tab", None)),
        KeyCode::BackTab => Some(("BackTab", None)),
        KeyCode::Backspace => Some(("Backspace", None)),
        KeyCode::Delete => Some(("Delete", None)),
        KeyCode::Up => Some(("Up", None)),
        KeyCode::Down => Some(("Down", None)),
        KeyCode::Left => Some(("Left", None)),
        KeyCode::Right => Some(("Right", None)),
        KeyCode::Home => Some(("Home", None)),
        KeyCode::End => Some(("End", None)),
        KeyCode::PageUp => Some(("PageUp", None)),
        KeyCode::PageDown => Some(("PageDown", None)),
        KeyCode::Char(c) => Some(("Char", Some(c.to_string()))),
        _ => None,
    }
}

fn modifier_names(modifiers: KeyModifiers) -> Vec<String> {
    let mut out = Vec::new();
    if modifiers.contains(KeyModifiers::CONTROL) {
        out.push("ctrl".into());
    }
    if modifiers.contains(KeyModifiers::ALT) {
        out.push("alt".into());
    }
    if modifiers.contains(KeyModifiers::SHIFT) {
        out.push("shift".into());
    }
    out
}

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

    pub fn custom_tui_host(&self) -> Arc<dyn CustomTuiHost> {
        Arc::new(self.custom_tui_slot.clone())
    }

    pub fn has_custom_ui(&self) -> bool {
        self.custom_ui.is_some()
    }

    pub fn custom_ui_wake(&self) -> Option<Arc<Notify>> {
        self.custom_ui.as_ref().map(|ui| Arc::clone(&ui.wake))
    }

    fn custom_inner_size(&self, spec: &CustomTuiSpec) -> (u16, u16) {
        let (fw, fh) = self.last_terminal_size;
        let fw = fw.max(24);
        let fh = fh.max(10);
        if spec.fullscreen {
            return (fw.saturating_sub(2).max(12), fh.saturating_sub(2).max(5));
        }
        let target_w = spec
            .width
            .unwrap_or_else(|| fw.saturating_sub(8).min(96))
            .min(fw.saturating_sub(2))
            .max(16);
        let target_h = spec
            .height
            .unwrap_or_else(|| fh.saturating_sub(6).min(28))
            .min(fh.saturating_sub(2))
            .max(5);
        (target_w.saturating_sub(2), target_h.saturating_sub(2))
    }

    pub fn pull_custom_ui(&mut self) -> bool {
        if self.custom_ui.is_some() {
            return false;
        }
        let Some(channel) = self.custom_tui_slot.take() else {
            return false;
        };
        let (w, h) = self.custom_inner_size(&channel.spec);
        let _ = channel.client_tx.send(CustomTuiClientEvent::Render {
            width: w,
            height: h,
            theme: custom_theme_json(self.active_colors()),
        });
        self.custom_ui = Some(CustomTuiOverlay {
            spec: channel.spec,
            client_tx: channel.client_tx,
            frame_rx: channel.frame_rx,
            wake: channel.wake,
            lines: Vec::new(),
            cursor: None,
            last_inner_size: (w, h),
        });
        self.picker = None;
        true
    }

    pub fn poll_custom_ui(&mut self) -> bool {
        let mut changed = false;
        let mut close = false;
        if let Some(overlay) = &mut self.custom_ui {
            loop {
                match overlay.frame_rx.try_recv() {
                    Ok(update) => {
                        changed = true;
                        if update.done {
                            close = true;
                            break;
                        }
                        overlay.lines = update.lines;
                        overlay.cursor = update.cursor;
                    }
                    Err(std_mpsc::TryRecvError::Empty) => break,
                    Err(std_mpsc::TryRecvError::Disconnected) => {
                        close = true;
                        changed = true;
                        break;
                    }
                }
            }
        }
        if close {
            self.custom_ui = None;
        }
        changed
    }

    pub fn cancel_custom_ui(&mut self) {
        if let Some(overlay) = self.custom_ui.take() {
            let _ = overlay.client_tx.send(CustomTuiClientEvent::Cancel);
        }
    }

    pub fn custom_ui_event(&mut self, event: &Event) -> bool {
        let Some(overlay) = self.custom_ui.as_mut() else {
            return false;
        };
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    let _ = overlay.client_tx.send(CustomTuiClientEvent::Cancel);
                    self.custom_ui = None;
                    return true;
                }
                if let Some((name, ch)) = key_name(key.code) {
                    let (w, h) = overlay.last_inner_size;
                    if overlay
                        .client_tx
                        .send(CustomTuiClientEvent::Input {
                            key: name.to_owned(),
                            ch,
                            modifiers: modifier_names(key.modifiers),
                            width: w,
                            height: h,
                        })
                        .is_err()
                    {
                        self.custom_ui = None;
                    }
                }
                true
            }
            Event::Resize(_, _) => {
                let spec = overlay.spec.clone();
                let (w, h) = self.custom_inner_size(&spec);
                let colors = self.active_colors();
                if let Some(overlay) = self.custom_ui.as_mut() {
                    overlay.last_inner_size = (w, h);
                    if overlay
                        .client_tx
                        .send(CustomTuiClientEvent::Render {
                            width: w,
                            height: h,
                            theme: custom_theme_json(colors),
                        })
                        .is_err()
                    {
                        self.custom_ui = None;
                    }
                }
                true
            }
            _ => true,
        }
    }

    pub(super) fn draw_custom_ui(&mut self, frame: &mut Frame, full_area: Rect, colors: Theme) {
        let Some(overlay) = self.custom_ui.as_mut() else {
            return;
        };
        let box_area = if overlay.spec.fullscreen {
            full_area
        } else {
            let w = overlay
                .spec
                .width
                .unwrap_or_else(|| full_area.width.saturating_sub(8).min(96))
                .min(full_area.width)
                .max(16);
            let h = overlay
                .spec
                .height
                .unwrap_or_else(|| full_area.height.saturating_sub(6).min(28))
                .min(full_area.height)
                .max(5);
            Rect::new(
                full_area.x + full_area.width.saturating_sub(w) / 2,
                full_area.y + full_area.height.saturating_sub(h) / 2,
                w,
                h,
            )
        };
        frame.render_widget(Clear, box_area);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" {} · Ctrl+C 退出 ", overlay.spec.title))
            .border_style(Style::default().fg(colors.accent))
            .style(Style::default().bg(colors.overlay).fg(colors.text));
        let inner = block.inner(box_area);
        frame.render_widget(block, box_area);
        overlay.last_inner_size = (inner.width, inner.height);
        let lines: Vec<Line<'static>> = overlay
            .lines
            .iter()
            .take(usize::from(inner.height))
            .map(|row| {
                let spans: Vec<Span<'static>> = row
                    .iter()
                    .map(|span| {
                        let mut style = Style::default();
                        if let Some(fg) = resolve_span_color(span.fg.as_deref(), colors) {
                            style = style.fg(fg);
                        }
                        if let Some(bg) = resolve_span_color(span.bg.as_deref(), colors) {
                            style = style.bg(bg);
                        }
                        if span.bold {
                            style = style.add_modifier(Modifier::BOLD);
                        }
                        if span.dim {
                            style = style.add_modifier(Modifier::DIM);
                        }
                        if span.reverse {
                            style = style.add_modifier(Modifier::REVERSED);
                        }
                        Span::styled(span.text.clone(), style)
                    })
                    .collect();
                Line::from(spans)
            })
            .collect();
        frame.render_widget(Paragraph::new(Text::from(lines)), inner);
        if let Some((cx, cy)) = overlay.cursor {
            if cx < inner.width && cy < inner.height {
                let pos = (inner.x + cx, inner.y + cy);
                if let Some(cell) = frame.buffer_mut().cell_mut(pos) {
                    cell.set_style(Style::default().bg(colors.text).fg(colors.background));
                }
                self.ime_cursor_position = Some(pos);
            }
        }
    }

    pub(super) fn has_widgets(&self, placement: WidgetPlacement) -> bool {
        self.plugin_widgets
            .iter()
            .any(|widget| widget.placement == placement)
    }

    pub(super) fn widget_slot_height(&self, placement: WidgetPlacement, width: u16) -> u16 {
        let items: Vec<&PluginWidget> = self
            .plugin_widgets
            .iter()
            .filter(|w| w.placement == placement)
            .collect();
        if items.is_empty() {
            return 0;
        }
        let inner_width = usize::from(width.saturating_sub(4)).max(16);
        let mut rows: usize = 0;
        for widget in items {
            if !widget.title.is_empty() {
                rows += 1;
            }
            rows += crate::panel::render_text(&widget.blocks, inner_width).len();
            if !widget.actions.is_empty() {
                rows += 1;
            }
            rows += 1;
        }
        (rows as u16).min(12)
    }

    pub(super) fn draw_widget_slot(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        placement: WidgetPlacement,
        colors: Theme,
    ) {
        if area.height == 0 || area.width < 8 {
            return;
        }
        let widgets: Vec<PluginWidget> = self
            .plugin_widgets
            .iter()
            .filter(|w| w.placement == placement)
            .cloned()
            .collect();
        if widgets.is_empty() {
            return;
        }
        let mut y = area.y;
        let max_y = area.bottom();
        let inner_w = usize::from(area.width.saturating_sub(2)).max(12);
        for widget in widgets {
            if y >= max_y {
                break;
            }
            if !widget.title.is_empty() {
                frame.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled("▎ ", Style::default().fg(colors.accent)),
                        Span::styled(
                            widget.title.clone(),
                            Style::default()
                                .fg(colors.text)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ])),
                    Rect::new(area.x, y, area.width, 1),
                );
                y += 1;
            }
            for row in crate::panel::render_text(&widget.blocks, inner_w) {
                if y >= max_y {
                    break;
                }
                frame.render_widget(
                    Paragraph::new(format!("  {row}")).style(Style::default().fg(colors.muted)),
                    Rect::new(area.x, y, area.width, 1),
                );
                y += 1;
            }
            if !widget.actions.is_empty() && y < max_y {
                let mut x = area.x.saturating_add(2);
                for action in &widget.actions {
                    let pill = format!("[{}]", action.label);
                    let w = UnicodeWidthStr::width(pill.as_str()) as u16;
                    if x.saturating_add(w) > area.right() {
                        break;
                    }
                    let color = if action.danger {
                        colors.error
                    } else if action.primary {
                        colors.accent
                    } else {
                        colors.text
                    };
                    let rect = Rect::new(x, y, w, 1);
                    frame.render_widget(
                        Paragraph::new(pill).style(
                            Style::default()
                                .fg(color)
                                .bg(colors.surface)
                                .add_modifier(Modifier::BOLD),
                        ),
                        rect,
                    );
                    self.widget_action_hits.push((
                        rect,
                        widget.plugin_id.clone(),
                        widget.id.clone(),
                        action.id.clone(),
                    ));
                    x = x.saturating_add(w + 1);
                }
                y += 1;
            }
        }
    }

    pub fn widget_action_at(&self, column: u16, row: u16) -> Option<(String, String, String)> {
        self.widget_action_hits
            .iter()
            .find(|(rect, _, _, _)| rect.contains((column, row).into()))
            .map(|(_, plugin_id, widget_id, action_id)| {
                (plugin_id.clone(), widget_id.clone(), action_id.clone())
            })
    }
}
