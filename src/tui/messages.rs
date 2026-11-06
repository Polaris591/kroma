//! Message identity decoration, separate from S2 execution-state colours.
//! Rails use background-coloured spaces so native terminal copies stay clean.
use super::palette::MessageColors;
use super::*;

#[derive(Clone, Copy)]
pub(super) enum MessageRole {
    User,
    Assistant,
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
            // Classic B: user messages are subtle gray cards, while the
            // assistant reads directly on the terminal background. Nested
            // tool calls provide their own gray panel inside that rail.
            if matches!(role, MessageRole::User) {
                decor.push((row, Decor::Surface(Surface::Message)));
            }
            let color = match role {
                MessageRole::User => p.user_rail,
                MessageRole::Assistant => p.assistant_rail,
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

    /// Start one assistant turn: one blank padding row, one role label, one
    /// blank row. Later text, reasoning and tool records join this rail.
    pub(super) fn begin_plain_assistant(
        &self,
        lines: &mut Vec<Line<'static>>,
        decor: &mut Vec<(usize, Decor)>,
        start: &mut Option<usize>,
    ) {
        if start.is_some() {
            return;
        }
        let block_start = lines.len();
        lines.push(Line::default());
        decor.push((
            lines.len(),
            Decor::Role {
                label: "KROMA",
                color: self.message_colors().assistant_rail,
            },
        ));
        lines.push(Line::default());
        *start = Some(block_start);
    }

    /// End the turn at its last content row, paint one continuous rail over
    /// every nested row, and retain no leading separator outside the block.
    pub(super) fn close_plain_assistant(
        &self,
        lines: &mut Vec<Line<'static>>,
        decor: &mut Vec<(usize, Decor)>,
        start: &mut Option<usize>,
    ) {
        let Some(block_start) = start.take() else {
            return;
        };
        if lines.last().is_some_and(|line| !line.spans.is_empty()) {
            lines.push(Line::default());
        }
        self.decorate_message(block_start, lines.len(), MessageRole::Assistant, decor);
        // A single terminal-background row separates consecutive turns.
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
