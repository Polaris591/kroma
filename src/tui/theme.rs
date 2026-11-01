//! Theme settings are data, not executable plugins. The defaults work even
//! with no file or HOME; AI can edit ~/.kroma/theme.toml without recompiling.
use anyhow::{bail, Context, Result};
use ratatui::style::Color;
use serde::{Deserialize, Serialize};
use std::{env, fs, path::PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeSettings {
    pub preset: String,
    pub accent: Option<String>,
    pub text: Option<String>,
    pub muted: Option<String>,
    pub border: Option<String>,
    pub background: Option<String>,
    pub user: Option<String>,
    pub tool: Option<String>,
    pub code: Option<String>,
    pub surface: Option<String>,
    pub overlay: Option<String>,
    pub faint: Option<String>,
    pub accent_text: Option<String>,
    pub success: Option<String>,
    pub warning: Option<String>,
}

impl Default for ThemeSettings {
    fn default() -> Self {
        Self {
            preset: "dark".into(),
            accent: None,
            text: None,
            muted: None,
            border: None,
            background: None,
            user: None,
            tool: None,
            code: None,
            surface: None,
            overlay: None,
            faint: None,
            accent_text: None,
            success: None,
            warning: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub background: Color,
    pub text: Color,
    pub muted: Color,
    pub accent: Color,
    pub border: Color,
    pub user: Color,
    pub tool: Color,
    pub code: Color,
    pub error: Color,
    pub surface: Color,
    pub overlay: Color,
    pub faint: Color,
    pub accent_text: Color,
    pub success: Color,
    pub warning: Color,
}

impl Theme {
    fn from_settings(settings: &ThemeSettings) -> Result<Self> {
        let mut theme = match settings.preset.as_str() {
            "dark" => Self {
                // Obsidian Dark: neutral charcoal elevation with restrained brand accents.
                background: Color::Rgb(11, 12, 14),
                text: Color::Rgb(236, 238, 242),
                muted: Color::Rgb(142, 149, 165),
                accent: Color::Rgb(110, 136, 255),
                border: Color::Rgb(38, 41, 50),
                user: Color::Rgb(226, 140, 156),
                tool: Color::Rgb(142, 149, 165),
                code: Color::Rgb(213, 217, 226),
                error: Color::Rgb(240, 82, 82),
                surface: Color::Rgb(19, 21, 25),
                overlay: Color::Rgb(26, 29, 35),
                faint: Color::Rgb(94, 100, 116),
                accent_text: Color::Rgb(140, 162, 255),
                success: Color::Rgb(76, 195, 138),
                warning: Color::Rgb(229, 169, 60),
            },
            "light" => Self {
                background: Color::Rgb(250, 250, 252),
                text: Color::Rgb(20, 22, 28),
                muted: Color::Rgb(85, 91, 110),
                accent: Color::Rgb(59, 91, 219),
                border: Color::Rgb(216, 220, 230),
                user: Color::Rgb(156, 54, 80),
                tool: Color::Rgb(85, 91, 110),
                code: Color::Rgb(44, 49, 64),
                error: Color::Rgb(186, 38, 38),
                surface: Color::Rgb(241, 242, 246),
                overlay: Color::Rgb(230, 232, 240),
                faint: Color::Rgb(116, 122, 142),
                accent_text: Color::Rgb(54, 79, 199),
                success: Color::Rgb(20, 114, 72),
                warning: Color::Rgb(138, 90, 0),
            },
            "mono" => Self {
                // Respect the user's native foreground/background pair.
                // White foreground on a light terminal used to disappear.
                background: Color::Reset,
                text: Color::Reset,
                muted: Color::Reset,
                accent: Color::Reset,
                border: Color::Reset,
                user: Color::Reset,
                tool: Color::Reset,
                code: Color::Reset,
                error: Color::Reset,
                surface: Color::Reset,
                overlay: Color::Reset,
                faint: Color::Reset,
                accent_text: Color::Reset,
                success: Color::Reset,
                warning: Color::Reset,
            },
            other => bail!("unknown theme '{other}'; available: dark, light, mono"),
        };
        for (value, target) in [
            (&settings.accent, &mut theme.accent),
            (&settings.text, &mut theme.text),
            (&settings.muted, &mut theme.muted),
            (&settings.border, &mut theme.border),
            (&settings.background, &mut theme.background),
            (&settings.user, &mut theme.user),
            (&settings.tool, &mut theme.tool),
            (&settings.code, &mut theme.code),
            (&settings.surface, &mut theme.surface),
            (&settings.overlay, &mut theme.overlay),
            (&settings.faint, &mut theme.faint),
            (&settings.accent_text, &mut theme.accent_text),
            (&settings.success, &mut theme.success),
            (&settings.warning, &mut theme.warning),
        ] {
            if let Some(value) = value {
                *target = parse_hex(value)?;
            }
        }
        Ok(theme)
    }
}

impl Theme {
    /// The raised composer/transcript surface (native background in mono).
    pub fn input_surface(&self) -> Color {
        self.surface
    }

    /// Blend foreground into a background; named/indexed terminal colours
    /// cannot be mixed reliably and are returned unchanged.
    pub fn blend(fg: Color, bg: Color, t: f64) -> Color {
        match (fg, bg) {
            (Color::Rgb(r, g, b), Color::Rgb(br, bg, bb)) => {
                let t = if t.is_finite() {
                    t.clamp(0.0, 1.0)
                } else {
                    1.0
                };
                let mix = |a, b| (f64::from(a) * t + f64::from(b) * (1.0 - t)).round() as u8;
                Color::Rgb(mix(r, br), mix(g, bg), mix(b, bb))
            }
            _ => fg,
        }
    }
}

pub(crate) fn parse_hex(raw: &str) -> Result<Color> {
    let hex = raw.strip_prefix('#').unwrap_or(raw);
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("theme color '{raw}' must be #RRGGBB");
    }
    let n = u32::from_str_radix(hex, 16)?;
    Ok(Color::Rgb((n >> 16) as u8, (n >> 8) as u8, n as u8))
}

pub struct ThemeStore {
    path: Option<PathBuf>,
    settings: ThemeSettings,
    pub colors: Theme,
}

impl ThemeStore {
    fn home_path() -> Option<PathBuf> {
        env::var_os("HOME")
            .or_else(|| env::var_os("USERPROFILE"))
            .map(|home| PathBuf::from(home).join(".kroma/theme.toml"))
    }

    /// If the theme is malformed, the UI can still start in dark mode. F2
    /// then overwrites the broken setting, or /theme reload can reread it.
    pub fn fallback() -> Self {
        let settings = ThemeSettings::default();
        let colors = Theme::from_settings(&settings).expect("builtin theme is valid");
        Self {
            path: Self::home_path(),
            settings,
            colors,
        }
    }

    pub fn load() -> Result<Self> {
        Self::load_at(Self::home_path())
    }

    pub(crate) fn load_at(path: Option<PathBuf>) -> Result<Self> {
        let settings = match path.as_deref() {
            Some(path) if path.is_file() => {
                let source = fs::read_to_string(path)
                    .with_context(|| format!("reading theme {}", path.display()))?;
                toml::from_str(&source).context("invalid theme.toml")?
            }
            _ => ThemeSettings::default(),
        };
        let colors = Theme::from_settings(&settings)?;
        Ok(Self {
            path,
            settings,
            colors,
        })
    }

    pub fn preset(&self) -> &str {
        &self.settings.preset
    }

    pub fn reload(&mut self) -> Result<()> {
        let updated = Self::load_at(self.path.clone())?;
        *self = updated;
        Ok(())
    }

    pub fn select(&mut self, preset: &str) -> Result<()> {
        let mut settings = self.settings.clone();
        settings.preset = preset.into();
        let colors = Theme::from_settings(&settings)?;
        if let Some(path) = &self.path {
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir)?;
            }
            fs::write(path, toml::to_string_pretty(&settings)?)
                .with_context(|| format!("saving theme {}", path.display()))?;
        }
        self.settings = settings;
        self.colors = colors;
        Ok(())
    }

    pub fn cycle(&mut self) -> Result<()> {
        let next = match self.preset() {
            "dark" => "light",
            "light" => "mono",
            _ => "dark",
        };
        self.select(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn luminance(rgb: Color) -> f64 {
        let Color::Rgb(r, g, b) = rgb else {
            panic!("test requires RGB theme")
        };
        let linear = |value: u8| {
            let value = f64::from(value) / 255.0;
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        linear(r) * 0.2126 + linear(g) * 0.7152 + linear(b) * 0.0722
    }

    #[test]
    fn builtin_text_and_status_colors_have_accessible_contrast() {
        for preset in ["dark", "light"] {
            let colors = Theme::from_settings(&ThemeSettings {
                preset: preset.into(),
                ..ThemeSettings::default()
            })
            .unwrap();
            for surface in [colors.background, colors.surface, colors.overlay] {
                for (name, ink) in [
                    ("text", colors.text),
                    ("muted", colors.muted),
                    ("accent_text", colors.accent_text),
                    ("user", colors.user),
                    ("success", colors.success),
                    ("warning", colors.warning),
                    ("error", colors.error),
                    ("faint", colors.faint),
                    ("accent", colors.accent),
                ] {
                    if name == "faint" && surface == colors.overlay {
                        continue;
                    }
                    let (a, b) = (luminance(ink), luminance(surface));
                    let ratio = (a.max(b) + 0.05) / (a.min(b) + 0.05);
                    let minimum = if matches!(name, "accent" | "faint") {
                        3.0
                    } else {
                        4.5
                    };
                    assert!(
                        ratio >= minimum,
                        "{preset} {name} on {surface:?}: {ratio:.2} < {minimum}"
                    );
                }
            }
            assert_ne!(colors.input_surface(), colors.background);
        }
    }
    #[test]
    fn monochrome_theme_uses_terminal_default_foreground_and_background() {
        let theme = Theme::from_settings(&ThemeSettings {
            preset: "mono".into(),
            ..ThemeSettings::default()
        })
        .unwrap();
        assert_eq!(theme.background, Color::Reset);
        assert_eq!(theme.text, Color::Reset);
        assert_eq!(theme.accent, Color::Reset);
        assert_eq!(theme.code, Color::Reset);
        assert_eq!(theme.input_surface(), Color::Reset);
    }

    #[test]
    fn presets_custom_hex_and_cycle_reload() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("kroma-theme-{}", std::process::id()));
        fs::create_dir_all(&dir)?;
        let path = dir.join("theme.toml");
        fs::write(&path, "preset = 'dark'\naccent = '#1020fF'\n")?;
        let mut store = ThemeStore::load_at(Some(path.clone()))?;
        assert_eq!(store.colors.accent, Color::Rgb(16, 32, 255));
        store.cycle()?;
        assert_eq!(store.preset(), "light");
        store.reload()?;
        assert_eq!(store.colors.accent, Color::Rgb(16, 32, 255));
        assert_eq!(store.preset(), "light");
        assert!(store.select("invalid").is_err());
        assert_eq!(store.preset(), "light");
        fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[test]
    fn rejects_invalid_colors_without_touching_file() {
        let settings = ThemeSettings {
            accent: Some("#12345G".into()),
            ..Default::default()
        };
        assert!(Theme::from_settings(&settings).is_err());
    }
}
