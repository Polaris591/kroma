//! The only source of Plain's visual tokens. Role identity, message surfaces
//! and S2 state colours are derived here, never independently in renderers.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct MessageColors {
    /// Shared message background, including Kroma replies.
    pub surface: Color,
    pub user_rail: Color,
    pub assistant_rail: Color,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SemanticColors {
    pub running: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
    pub link: Color,
    pub add: Color,
    pub remove: Color,
}

#[derive(Clone, Copy)]
pub(super) struct PlainPalette {
    pub theme: Theme,
    pub message: MessageColors,
    pub semantic: SemanticColors,
}

impl UiState {
    /// Plain deliberately stays grayscale except for identity rails and S2
    /// state colours. `mono` delegates every colour to the terminal.
    pub(super) fn plain_palette(&self) -> PlainPalette {
        if self.themes.preset() == "mono" {
            let native = Color::Reset;
            return self.with_plugin_colors(PlainPalette {
                theme: Theme {
                    background: native,
                    surface: native,
                    inset: native,
                    overlay: native,
                    text: native,
                    muted: native,
                    faint: native,
                    border: native,
                    accent: native,
                    accent_text: native,
                    user: native,
                    tool: native,
                    code: native,
                    success: native,
                    warning: native,
                    error: native,
                },
                message: MessageColors {
                    surface: native,
                    user_rail: native,
                    assistant_rail: native,
                },
                semantic: SemanticColors {
                    running: native,
                    success: native,
                    warning: native,
                    error: native,
                    link: native,
                    add: native,
                    remove: native,
                },
            });
        }

        if self.themes.preset() == "dark" {
            // The default Plain renderer uses the same semantic palette as
            // the theme settings, including user theme.toml overrides.
            let base = self.themes.colors;
            return self.with_plugin_colors(PlainPalette {
                theme: base,
                message: MessageColors {
                    surface: base.surface,
                    user_rail: base.user,
                    assistant_rail: base.accent,
                },
                semantic: SemanticColors {
                    running: base.accent,
                    success: base.success,
                    warning: base.warning,
                    error: base.error,
                    // Links are separate from the cyan role rail.
                    link: Color::Rgb(141, 189, 255),
                    add: base.success,
                    remove: base.error,
                },
            });
        }

        let is_light = self.themes.preset() == "light";
        let gray = |dark_value: u8, light_value: u8| {
            let value = if is_light { light_value } else { dark_value };
            Color::Rgb(value, value, value)
        };
        let semantic = if is_light {
            SemanticColors {
                running: Color::Rgb(36, 91, 180),
                success: Color::Rgb(44, 107, 60),
                warning: Color::Rgb(128, 85, 14),
                error: Color::Rgb(170, 53, 71),
                link: Color::Rgb(36, 91, 180),
                add: Color::Rgb(44, 107, 60),
                remove: Color::Rgb(170, 53, 71),
            }
        } else {
            SemanticColors {
                running: Color::Rgb(122, 162, 247),
                success: Color::Rgb(134, 217, 154),
                warning: Color::Rgb(229, 181, 103),
                error: Color::Rgb(240, 120, 134),
                link: Color::Rgb(122, 162, 247),
                add: Color::Rgb(134, 217, 154),
                remove: Color::Rgb(240, 120, 134),
            }
        };
        self.with_plugin_colors(PlainPalette {
            theme: Theme {
                background: gray(13, 239),
                surface: gray(23, 226),
                inset: gray(17, 239),
                overlay: gray(30, 219),
                text: gray(229, 41),
                muted: gray(176, 83),
                faint: gray(145, 96),
                border: gray(86, 152),
                accent: gray(244, 17),
                accent_text: gray(244, 17),
                user: gray(176, 83),
                tool: gray(176, 83),
                code: gray(229, 41),
                success: gray(176, 83),
                warning: gray(229, 41),
                error: gray(244, 17),
            },
            message: MessageColors {
                surface: gray(27, 227),
                user_rail: if is_light {
                    Color::Rgb(109, 74, 199)
                } else {
                    Color::Rgb(167, 139, 250)
                },
                assistant_rail: if is_light {
                    Color::Rgb(15, 107, 107)
                } else {
                    Color::Rgb(94, 211, 211)
                },
            },
            semantic,
        })
    }

    /// `[ui.colors]` is a host-level appearance contract: a manifest slot
    /// overrides the same token in every preset, including `mono`, and the
    /// identity rails follow the `user` / `accent` slots they are built from.
    fn with_plugin_colors(&self, mut palette: PlainPalette) -> PlainPalette {
        for (slot, color) in &self.plugin_colors {
            match slot.as_str() {
                "accent" => {
                    palette.theme.accent = *color;
                    palette.theme.accent_text = *color;
                    palette.message.assistant_rail = *color;
                    palette.semantic.running = *color;
                }
                "background" => palette.theme.background = *color,
                "border" => palette.theme.border = *color,
                "text" => palette.theme.text = *color,
                "muted" => palette.theme.muted = *color,
                "user" => {
                    palette.theme.user = *color;
                    palette.message.user_rail = *color;
                }
                "tool" => palette.theme.tool = *color,
                "code" => palette.theme.code = *color,
                _ => {}
            }
        }
        palette
    }
}
