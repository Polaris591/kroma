//! The supplied Kroma SVG mark, sampled into terminal half-block cells.
//! Windows Terminal, SSH and ordinary POSIX terminals all show the same
//! three wedges, without Kitty/Sixel image protocols or network access.
use super::theme::Theme;
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::Paragraph,
    Frame,
};

#[cfg(test)]
const MARK_WIDTH: usize = 28;
#[cfg(test)]
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

fn pixel_at(
    x: usize,
    subrow: usize,
    width: usize,
    height: usize,
    light: bool,
    mono: bool,
) -> Option<Color> {
    let px = (x as f64 + 0.5) * 100.0 / width as f64;
    let py = (subrow as f64 + 0.5) * 86.6 / (height * 2) as f64;
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

fn mark_rows_sized(
    theme: Theme,
    light: bool,
    mono: bool,
    width: usize,
    height: usize,
) -> Vec<Line<'static>> {
    (0..height)
        .map(|row| {
            let cells = (0..width).map(|column| {
                let top = pixel_at(column, row * 2, width, height, light, mono);
                let bottom = pixel_at(column, row * 2 + 1, width, height, light, mono);
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

/// Draw the compact original mark inside a layout owned by the welcome view.
pub(super) fn draw(frame: &mut Frame, area: Rect, theme: Theme, preset: &str) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let show_mark = area.height >= 5 && area.width >= 24;
    if show_mark {
        frame.render_widget(
            Paragraph::new(Text::from(mark_rows_sized(
                theme,
                preset == "light",
                preset == "mono",
                12,
                5,
            ))),
            Rect::new(area.x, area.y, 12, 5),
        );
    }
    let offset = if show_mark { 16 } else { 0 };
    frame.render_widget(
        Paragraph::new(Span::styled(
            "Kroma",
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        )),
        Rect::new(
            area.x + offset,
            area.y + if show_mark { 2 } else { 0 },
            area.width.saturating_sub(offset),
            1,
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn supplied_mark_has_three_distinct_brand_colors() {
        let samples = (0..MARK_HEIGHT * 2)
            .flat_map(|y| {
                (0..MARK_WIDTH)
                    .filter_map(move |x| pixel_at(x, y, MARK_WIDTH, MARK_HEIGHT, false, false))
            })
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

    #[test]
    fn compact_mark_keeps_three_wedges_at_the_requested_size() {
        let theme = super::super::theme::ThemeStore::load_at(None)
            .unwrap()
            .colors;
        let rows = mark_rows_sized(theme, false, false, 20, 8);
        assert_eq!(rows.len(), 8);
        assert!(rows.iter().all(|line| line.spans.len() == 20));
        let colors = rows
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.style.fg)
            .collect::<Vec<_>>();
        for color in [
            Some(Color::Rgb(91, 124, 255)),
            Some(Color::Rgb(244, 244, 246)),
            Some(Color::Rgb(255, 111, 145)),
        ] {
            assert!(colors.contains(&color), "compact mark lost a wedge");
        }
    }
}
