//! Message identity decoration, separate from S2 execution-state colours.
//! Rails use background-coloured spaces so native terminal copies stay clean.
use super::palette::MessageColors;
use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum MessageRole {
    User,
    Assistant,
    Tools,
}

impl UiState {
    pub(super) fn message_colors(&self) -> MessageColors {
        self.plain_palette().message
    }

    pub(super) fn decorate_message(
        &self,
        start: usize,
        end: usize,
        role: MessageRole,
        decor: &mut Vec<(usize, Decor)>,
    ) {
        let p = self.message_colors();
        for row in start..end {
            // User, assistant and tool groups share one neutral surface.
            // Identity lives in the rail, not in competing shades of gray.
            decor.push((row, Decor::Surface(Surface::Message)));
            let color = match role {
                MessageRole::User => p.user_rail,
                MessageRole::Assistant | MessageRole::Tools => p.assistant_rail,
            };
            decor.push((
                row,
                Decor::Rail {
                    column: 1,
                    color,
                    first_only: false,
                },
            ));
        }
    }

    /// Start a single visible phase, not the entire assistant turn. The next
    /// model response or batch of tool calls gets a separate card and rail.
    pub(super) fn begin_plain_block(
        &self,
        lines: &mut Vec<Line<'static>>,
        decor: &mut Vec<(usize, Decor)>,
        start: &mut Option<(usize, MessageRole)>,
        role: MessageRole,
    ) {
        if start.is_some() {
            return;
        }
        let block_start = lines.len();
        lines.push(Line::default());
        decor.push((
            lines.len(),
            Decor::Role {
                label: match role {
                    MessageRole::Tools => self.preferences.language.text("TOOLS", "工具"),
                    MessageRole::User => self.preferences.language.text("YOU", "你"),
                    MessageRole::Assistant => "KROMA",
                },
                color: match role {
                    MessageRole::User => self.message_colors().user_rail,
                    MessageRole::Assistant | MessageRole::Tools => {
                        self.message_colors().assistant_rail
                    }
                },
            },
        ));
        lines.push(Line::default());
        *start = Some((block_start, role));
    }

    /// Close a phase with one continuous surface and rail, then leave a single
    /// terminal-background row between this card and whatever comes next.
    pub(super) fn close_plain_block(
        &self,
        lines: &mut Vec<Line<'static>>,
        decor: &mut Vec<(usize, Decor)>,
        start: &mut Option<(usize, MessageRole)>,
    ) {
        let Some((block_start, role)) = start.take() else {
            return;
        };
        if lines.last().is_some_and(|line| !line.spans.is_empty()) {
            lines.push(Line::default());
        }
        self.decorate_message(block_start, lines.len(), role, decor);
        lines.push(Line::default());
    }

    /// Both speakers fill the available transcript width, independently of
    /// the prose wrapping limit. Plain style leaves a terminal-background
    /// gutter before the scrollbar so the right safety margin stays visible.
    pub(super) fn message_surface_area(&self, history: Rect, y: u16) -> Rect {
        let gutter = if self.plain_style {
            PLAIN_RIGHT_GUTTER
        } else {
            0
        };
        Rect::new(
            history.x.saturating_add(1),
            y,
            history.width.saturating_sub(1 + gutter),
            1,
        )
    }
}
