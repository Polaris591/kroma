//! The supplied Kroma SVG mark, sampled into terminal half-block cells.
//! Windows Terminal, SSH and ordinary POSIX terminals all show the same
//! three wedges, without Kitty/Sixel image protocols or network access.
use super::theme::Theme;
use ratatui::{
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::Paragraph,
    Frame,
};

const MARK_WIDTH: usize = 28;
const MARK_HEIGHT: usize = 12;
const LEFT: [(f64, f64); 3] = [(46.18, 6.61), (0.0, 86.6), (30.79, 86.6)];
const MIDDLE: [(f64, f64); 3] = [(35.88, 86.6), (64.12, 86.6), (50.0, 13.23)];
const RIGHT: [(f64, f64); 3] = [(69.21, 86.6), (100.0, 86.6), (53.82, 6.61)];

fn in_triangle(x: f64, y: f64, triangle: [(f64, f64); 3]) -> bool {
    let cross = |a: (f64, f64), b: (f64, f64)| (b.0 - a.0) * (y - a.1) - (b.1 - a.1) * (x - a.0);
    let a = cross(triangle[0], triangle[1]);
    let b = cross(triangle[1], triangle[2]);
    let c = cross(triangle[2], triangle[0]);
    !(a < 0.0 || b < 0.0 || c < 0.0) || !(a > 0.0 || b > 0.0 || c > 0.0)
}

fn pixel(x: usize, subrow: usize, light: bool, mono: bool) -> Option<Color> {
    let px = (x as f64 + 0.5) * 100.0 / MARK_WIDTH as f64;
    let py = (subrow as f64 + 0.5) * 86.6 / (MARK_HEIGHT * 2) as f64;
    let (blue, white, rose) = if light {
        (
            Color::Rgb(63, 95, 234),
            Color::Rgb(15, 17, 21),
            Color::Rgb(240, 78, 120),
        )
    } else {
        (
            Color::Rgb(91, 124, 255),
            Color::Rgb(244, 244, 246),
            Color::Rgb(255, 111, 145),
        )
    };
    let color = if in_triangle(px, py, LEFT) {
        Some(blue)
    } else if in_triangle(px, py, MIDDLE) {
        Some(white)
    } else if in_triangle(px, py, RIGHT) {
        Some(rose)
    } else {
        None
    };
    if mono {
        color.map(|_| Color::Reset)
    } else {
        color
    }
}

pub(super) fn mark_rows(theme: Theme, light: bool, mono: bool) -> Vec<Line<'static>> {
    (0..MARK_HEIGHT)
        .map(|row| {
            let cells = (0..MARK_WIDTH).map(|column| {
                let top = pixel(column, row * 2, light, mono);
                let bottom = pixel(column, row * 2 + 1, light, mono);
                let (glyph, foreground, background) = match (top, bottom) {
                    (None, None) => (" ", theme.background, theme.background),
                    (Some(a), Some(b)) if a == b => ("█", a, theme.background),
                    (Some(a), Some(b)) => ("▀", a, b),
                    (Some(a), None) => ("▀", a, theme.background),
                    (None, Some(b)) => ("▄", b, theme.background),
                };
                Span::styled(glyph, Style::default().fg(foreground).bg(background))
            });
            Line::from(cells.collect::<Vec<_>>())
        })
        .collect()
}

pub(super) fn draw(frame: &mut Frame, area: Rect, theme: Theme, preset: &str) {
    if area.width < 24 || area.height < 5 {
        return;
    }
    let show_mark = area.height >= 17 && area.width >= MARK_WIDTH as u16 + 6;
    let height: u16 = if show_mark { 17 } else { 3 };
    let top = area.y + area.height.saturating_sub(height) / 2;
    if show_mark {
        frame.render_widget(
            Paragraph::new(Text::from(mark_rows(
                theme,
                preset == "light",
                preset == "mono",
            )))
            .alignment(Alignment::Center),
            Rect::new(area.x, top, area.width, MARK_HEIGHT as u16),
        );
    }
    let title = if show_mark {
        top + MARK_HEIGHT as u16 + 1
    } else {
        top
    };
    frame.render_widget(
        Paragraph::new(Span::styled(
            "K R O M A",
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        ))
        .alignment(Alignment::Center),
        Rect::new(area.x, title, area.width, 1),
    );
    if area.height >= 7 {
        frame.render_widget(
            Paragraph::new("Start a conversation  ·  /resume for history")
                .style(Style::default().fg(theme.muted))
                .alignment(Alignment::Center),
            Rect::new(area.x, title + 2, area.width, 1),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn supplied_mark_has_three_distinct_brand_colors() {
        let samples = (0..MARK_HEIGHT * 2)
            .flat_map(|y| (0..MARK_WIDTH).filter_map(move |x| pixel(x, y, false, false)))
            .collect::<Vec<_>>();
        for color in [
            Color::Rgb(91, 124, 255),
            Color::Rgb(244, 244, 246),
            Color::Rgb(255, 111, 145),
        ] {
            assert!(
                samples.contains(&color),
                "logo must retain all three original wedges"
            );
        }
        assert!(samples.len() > 120);
    }
}
