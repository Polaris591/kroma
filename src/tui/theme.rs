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
    pub inset: Option<String>,
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
            inset: None,
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
    pub inset: Color,
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
                // Restore the original Plain grayscale foregrounds and
                // surfaces. Brand identity and execution states keep their
                // distinct accent colours without tinting ordinary content.
                background: Color::Rgb(13, 13, 13), // original terminal gray
                text: Color::Rgb(244, 244, 245),    // crisp primary ink on unchanged gray
                muted: Color::Rgb(189, 191, 197),   // readable secondary ink
                accent: Color::Rgb(94, 218, 214),   // vivid but controlled cyan
                border: Color::Rgb(86, 86, 86),     // original separators
                user: Color::Rgb(190, 155, 255),    // distinct luminous violet
                tool: Color::Rgb(189, 191, 197),    // subdued command metadata
                code: Color::Rgb(244, 244, 245),    // syntax tokens styled separately
                error: Color::Rgb(255, 139, 151),   // legible coral error ink
                surface: Color::Rgb(27, 27, 27),    // original message gray
                inset: Color::Rgb(17, 17, 17),      // recessed tool area
                overlay: Color::Rgb(30, 30, 30),    // code and overlays
                faint: Color::Rgb(161, 161, 166),   // brighter quiet metadata
                accent_text: Color::Rgb(94, 218, 214),
                success: Color::Rgb(142, 223, 173),
                warning: Color::Rgb(255, 193, 120),
            },
            "light" => Self {
                background: Color::Rgb(246, 245, 242),
                text: Color::Rgb(37, 38, 43),
                muted: Color::Rgb(84, 89, 101),
                accent: Color::Rgb(59, 83, 150),
                border: Color::Rgb(203, 203, 208),
                user: Color::Rgb(137, 72, 94),
                tool: Color::Rgb(84, 89, 101),
                code: Color::Rgb(37, 38, 43),
                error: Color::Rgb(161, 62, 72),
                surface: Color::Rgb(238, 237, 233),
                inset: Color::Rgb(244, 243, 240),
                overlay: Color::Rgb(229, 228, 223),
                faint: Color::Rgb(97, 102, 114),
                accent_text: Color::Rgb(59, 83, 150),
                success: Color::Rgb(44, 107, 73),
                warning: Color::Rgb(121, 88, 31),
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
                inset: Color::Reset,
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
            (&settings.inset, &mut theme.inset),
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
        let mut settings = settings;
        if env::var_os("NO_COLOR").is_some() || env::var_os("KROMA_NO_COLOR").is_some() {
            settings.preset = "mono".into();
        }
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
    fn dark_preset_restores_original_plain_grays_without_losing_role_colors() {
        let theme = Theme::from_settings(&ThemeSettings::default()).unwrap();
        assert_eq!(theme.background, Color::Rgb(13, 13, 13));
        assert_eq!(theme.surface, Color::Rgb(27, 27, 27));
        assert_eq!(theme.inset, Color::Rgb(17, 17, 17));
        assert_eq!(theme.overlay, Color::Rgb(30, 30, 30));
        assert_eq!(theme.text, Color::Rgb(244, 244, 245));
        assert_eq!(theme.muted, Color::Rgb(189, 191, 197));
        assert_eq!(theme.faint, Color::Rgb(161, 161, 166));
        assert_eq!(theme.code, theme.text);
        assert_eq!(theme.accent, Color::Rgb(94, 218, 214));
        assert_eq!(theme.user, Color::Rgb(190, 155, 255));
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
                    let (a, b) = (luminance(ink), luminance(surface));
                    let ratio = (a.max(b) + 0.05) / (a.min(b) + 0.05);
                    // Faint is decorative only; functional text must meet
                    // 4.5:1 on every dark surface.
                    let minimum = if name == "faint" && preset == "dark" {
                        2.5
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
    #[test]
    fn selected_picker_actions_and_diff_tints_keep_text_readable() {
        for preset in ["dark", "light"] {
            let theme = Theme::from_settings(&ThemeSettings {
                preset: preset.into(),
                ..ThemeSettings::default()
            })
            .unwrap();
            let selected = Theme::blend(theme.accent, theme.overlay, 0.04);
            for ink in [theme.text, theme.muted, theme.accent_text] {
                let (a, b) = (luminance(ink), luminance(selected));
                assert!(
                    (a.max(b) + 0.05) / (a.min(b) + 0.05) >= 4.5,
                    "{preset} selected text {ink:?} on {selected:?}"
                );
            }
            for line in ["+new_value", "-old_value"] {
                let style = super::super::highlight::diff_style(line, theme);
                let (a, b) = (
                    luminance(style.fg.unwrap()),
                    luminance(style.bg.unwrap_or(theme.surface)),
                );
                assert!(
                    (a.max(b) + 0.05) / (a.min(b) + 0.05) >= 4.5,
                    "{preset} diff {line}"
                );
            }
        }
    }
}
