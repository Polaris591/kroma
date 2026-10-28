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
}

impl Theme {
    fn from_settings(settings: &ThemeSettings) -> Result<Self> {
        let mut theme = match settings.preset.as_str() {
            "dark" => Self {
                background: Color::Rgb(15, 18, 24),
                text: Color::Rgb(228, 233, 240),
                muted: Color::Rgb(153, 165, 183),
                accent: Color::Rgb(128, 206, 218),
                border: Color::Rgb(67, 78, 91),
                user: Color::Rgb(187, 223, 165),
                tool: Color::Rgb(215, 185, 138),
                code: Color::Rgb(194, 209, 237),
                error: Color::Rgb(249, 139, 147),
            },
            "light" => Self {
                background: Color::Rgb(248, 249, 246),
                text: Color::Rgb(30, 40, 51),
                muted: Color::Rgb(82, 96, 113),
                accent: Color::Rgb(28, 100, 125),
                border: Color::Rgb(185, 195, 203),
                user: Color::Rgb(43, 103, 70),
                tool: Color::Rgb(129, 86, 46),
                code: Color::Rgb(62, 81, 128),
                error: Color::Rgb(170, 50, 61),
            },
            "mono" => Self {
                background: Color::Reset,
                text: Color::White,
                muted: Color::Gray,
                accent: Color::White,
                border: Color::DarkGray,
                user: Color::White,
                tool: Color::Gray,
                code: Color::White,
                error: Color::Red,
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
        ] {
            if let Some(value) = value {
                *target = parse_hex(value)?;
            }
        }
        Ok(theme)
    }
}

impl Theme {
    /// Give the input a subtle second layer without adding another theme key.
    /// The contrast between surfaces is intentionally much lower than text
    /// contrast; in monochrome terminals use the native background.
    pub fn input_surface(&self) -> Color {
        let Color::Rgb(r, g, b) = self.background else {
            return self.background;
        };
        let brighten = u16::from(r) + u16::from(g) + u16::from(b) < 384;
        let change = |v: u8| {
            if brighten {
                v.saturating_add(10)
            } else {
                v.saturating_sub(10)
            }
        };
        Color::Rgb(change(r), change(g), change(b))
    }
}

fn parse_hex(raw: &str) -> Result<Color> {
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
            for (name, ink) in [
                ("body", colors.text),
                ("muted", colors.muted),
                ("accent", colors.accent),
                ("user", colors.user),
                ("tool", colors.tool),
                ("code", colors.code),
                ("error", colors.error),
            ] {
                let (a, b) = (luminance(ink), luminance(colors.background));
                let ratio = (a.max(b) + 0.05) / (a.min(b) + 0.05);
                assert!(ratio >= 4.5, "{preset} {name} contrast {ratio:.2} < 4.5");
            }
            assert_ne!(colors.input_surface(), colors.background);
        }
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
