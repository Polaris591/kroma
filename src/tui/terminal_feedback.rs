//! Terminal-only presentation: capability fallback, title and opt-in WT progress.
use ratatui::{buffer::Buffer, style::Color};
use std::{
    collections::HashMap,
    io::{self, Write},
};

fn truecolor(colorterm: Option<&str>, windows_terminal: bool) -> bool {
    windows_terminal
        || colorterm.is_some_and(|value| {
            let value = value.to_ascii_lowercase();
            value.contains("truecolor") || value.contains("24bit")
        })
}
fn indexed(color: Color) -> Color {
    let Color::Rgb(r, g, b) = color else {
        return color;
    };
    let levels = [0u8, 95, 135, 175, 215, 255];
    let nearest = |value: u8| {
        levels
            .iter()
            .enumerate()
            .min_by_key(|(_, level)| i32::from(value).abs_diff(i32::from(**level)))
            .map_or(0, |(index, _)| index)
    };
    let (ri, gi, bi) = (nearest(r), nearest(g), nearest(b));
    // Grey index minimises squared distance to all three channels.
    let grey_index = (((f64::from(r) + f64::from(g) + f64::from(b)) / 3.0 - 8.0) / 10.0)
        .round()
        .clamp(0.0, 23.0) as u8;
    let grey = 8 + grey_index * 10;
    let squared = |a: u8, c: u8, d: u8| {
        (i32::from(a) - i32::from(r)).pow(2)
            + (i32::from(c) - i32::from(g)).pow(2)
            + (i32::from(d) - i32::from(b)).pow(2)
    };
    if squared(grey, grey, grey) < squared(levels[ri], levels[gi], levels[bi]) {
        Color::Indexed(232 + grey_index)
    } else {
        Color::Indexed((16 + 36 * ri + 6 * gi + bi) as u8)
    }
}

pub(super) struct Feedback {
    pub windows_terminal: bool,
    reduce: bool,
    progress: bool,
    title: String,
    palette: HashMap<Color, Color>,
}
impl Feedback {
    pub(super) fn from_environment() -> Self {
        let windows_terminal = std::env::var_os("WT_SESSION").is_some();
        Self {
            windows_terminal,
            reduce: !truecolor(std::env::var("COLORTERM").ok().as_deref(), windows_terminal),
            progress: false,
            title: String::new(),
            palette: HashMap::new(),
        }
    }
    /// Quantise after blending/rendering so diff foregrounds remain readable.
    pub(super) fn reduce_palette(&mut self, buffer: &mut Buffer) {
        if !self.reduce {
            return;
        }
        for cell in &mut buffer.content {
            cell.fg = *self
                .palette
                .entry(cell.fg)
                .or_insert_with(|| indexed(cell.fg));
            cell.bg = *self
                .palette
                .entry(cell.bg)
                .or_insert_with(|| indexed(cell.bg));
        }
        if self.palette.len() > 1024 {
            self.palette.clear();
        }
    }
    pub(super) fn update(
        &mut self,
        writer: &mut impl Write,
        title: &str,
        running: bool,
    ) -> io::Result<()> {
        if self.title != title {
            crossterm::execute!(writer, crossterm::terminal::SetTitle(title))?;
            self.title = title.into();
        }
        if self.windows_terminal && self.progress != running {
            writer.write_all(if running {
                b"\x1b]9;4;3\x07"
            } else {
                b"\x1b]9;4;0\x07"
            })?;
            writer.flush()?;
            self.progress = running;
        }
        Ok(())
    }
    pub(super) fn clear(&mut self, writer: &mut impl Write) {
        if self.windows_terminal {
            clear_progress(writer);
            self.progress = false;
        }
    }
}
pub(super) fn clear_progress(writer: &mut impl Write) {
    let _ = writer.write_all(b"\x1b]9;4;0\x07");
    let _ = writer.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn truecolor_detection_and_nearest_cube_grey_fallback() {
        assert!(truecolor(Some("TRUECOLOR"), false));
        assert!(truecolor(Some("24bit"), false));
        assert!(truecolor(None, true));
        assert!(!truecolor(None, false));
        assert_eq!(indexed(Color::Rgb(95, 135, 175)), Color::Indexed(67));
        assert_eq!(indexed(Color::Rgb(128, 128, 128)), Color::Indexed(244));
        assert_eq!(indexed(Color::Reset), Color::Reset);
    }
    #[test]
    fn vivid_semantic_and_role_colours_keep_chroma_in_the_256_colour_fallback() {
        for color in [
            Color::Rgb(122, 162, 247),
            Color::Rgb(134, 217, 154),
            Color::Rgb(229, 181, 103),
            Color::Rgb(240, 120, 134),
            Color::Rgb(101, 199, 173),
        ] {
            let Color::Indexed(index) = indexed(color) else {
                panic!("indexed fallback expected")
            };
            assert!((16..232).contains(&index));
            let cube = index - 16;
            let (r, g, b) = (cube / 36, (cube % 36) / 6, cube % 6);
            assert!(
                r != g || g != b,
                "semantic colour collapsed to gray: {color:?}"
            );
        }
    }
    #[test]
    fn progress_only_targets_windows_terminal_and_clears_on_exit() {
        let mut feedback = Feedback {
            windows_terminal: false,
            reduce: false,
            progress: false,
            title: "title".into(),
            palette: HashMap::new(),
        };
        let mut output = Vec::new();
        feedback.update(&mut output, "title", true).unwrap();
        feedback.clear(&mut output);
        assert!(output.is_empty());
        feedback.windows_terminal = true;
        feedback.update(&mut output, "title", true).unwrap();
        feedback.update(&mut output, "title", true).unwrap();
        feedback.update(&mut output, "title", false).unwrap();
        assert_eq!(output, b"\x1b]9;4;3\x07\x1b]9;4;0\x07");
        output.clear();
        feedback.clear(&mut output);
        assert_eq!(output, b"\x1b]9;4;0\x07");
    }
}
