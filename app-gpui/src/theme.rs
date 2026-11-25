//! Agent Superviser color tokens applied on top of GPUI Kit's theme.
//! Matches the light/dark palette and deepening-green setup accents from `app/ui/style.css`.

use gpui_kit::component::theme::{Theme, ThemeMode};
use gpui_kit::{App, Hsla, Window, rgb};

/// Agent Superviser accent & semantic palette for light and dark mode.
#[derive(Clone, Copy)]
pub struct Palette {
    pub bg: Hsla,
    pub panel: Hsla,
    pub text: Hsla,
    pub muted: Hsla,
    pub line: Hsla,
    pub chip: Hsla,
    pub accent: Hsla,
    pub accent_text: Hsla,
    pub good: Hsla,
    pub warn: Hsla,
    pub bad: Hsla,
    pub green_1: Hsla,
    pub on_green_1: Hsla,
    pub green_2: Hsla,
    pub on_green_2: Hsla,
    pub green_3: Hsla,
    pub on_green_3: Hsla,
}

pub fn palette(is_dark: bool) -> Palette {
    if is_dark {
        Palette {
            bg: rgb(0x17191c).into(),
            panel: rgb(0x1f2226).into(),
            text: rgb(0xe8eaed).into(),
            muted: rgb(0x9aa1a9).into(),
            line: rgb(0x30343a).into(),
            chip: rgb(0x2a2e33).into(),
            accent: rgb(0x46b376).into(),
            accent_text: rgb(0x06150c).into(),
            good: rgb(0x5cc98a).into(),
            warn: rgb(0xe0a83a).into(),
            bad: rgb(0xf07167).into(),
            green_1: rgb(0x24402e).into(),
            on_green_1: rgb(0xc9f0d6).into(),
            green_2: rgb(0x2f7a4b).into(),
            on_green_2: rgb(0xeffff4).into(),
            green_3: rgb(0x46b376).into(),
            on_green_3: rgb(0x06150c).into(),
        }
    } else {
        Palette {
            bg: rgb(0xf6f7f8).into(),
            panel: rgb(0xffffff).into(),
            text: rgb(0x1c2024).into(),
            muted: rgb(0x646b73).into(),
            line: rgb(0xe2e5e9).into(),
            chip: rgb(0xeef1f3).into(),
            accent: rgb(0x1e7f43).into(),
            accent_text: rgb(0xffffff).into(),
            good: rgb(0x1d7a46).into(),
            warn: rgb(0x9a6400).into(),
            bad: rgb(0xb3261e).into(),
            green_1: rgb(0xd4f0dd).into(),
            on_green_1: rgb(0x14532d).into(),
            green_2: rgb(0x8fd5a6).into(),
            on_green_2: rgb(0x0f3d22).into(),
            green_3: rgb(0x1e7f43).into(),
            on_green_3: rgb(0xffffff).into(),
        }
    }
}

/// Syncs with the system appearance (light/dark) and applies our green brand palette.
pub fn apply_theme(window: Option<&mut Window>, cx: &mut App) {
    Theme::sync_system_appearance(window, cx);
    let is_dark = Theme::global(cx).mode == ThemeMode::Dark;
    let p = palette(is_dark);

    let theme = Theme::global_mut(cx);
    theme.background = p.bg;
    theme.foreground = p.text;
    theme.muted = p.chip;
    theme.muted_foreground = p.muted;
    theme.border = p.line;
    theme.primary = p.accent;
    theme.primary_foreground = p.accent_text;
    theme.primary_hover = p.accent.opacity(0.88);
    theme.primary_active = p.accent.opacity(0.78);
    theme.button_primary = p.accent;
    theme.button_primary_foreground = p.accent_text;
    theme.button_primary_hover = p.accent.opacity(0.88);
    theme.button_primary_active = p.accent.opacity(0.78);
    theme.tokens.primary = p.accent.into();
    theme.tokens.primary_hover = p.accent.opacity(0.88).into();
    theme.tokens.primary_active = p.accent.opacity(0.78).into();
    theme.tokens.button_primary = p.accent.into();
    theme.tokens.button_primary_hover = p.accent.opacity(0.88).into();
    theme.tokens.button_primary_active = p.accent.opacity(0.78).into();
    theme.sidebar = p.panel;
    theme.sidebar_border = p.line;
    theme.popover = p.panel;
    theme.popover_foreground = p.text;
    theme.colors.list = p.panel;
    theme.secondary = p.panel;
    theme.secondary_foreground = p.text;
    theme.secondary_hover = p.chip;
    theme.secondary_active = p.line;
    theme.success = p.good;
    theme.warning = p.warn;
    theme.danger = p.bad;
    theme.ring = p.accent;
}

pub fn gib(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

pub fn whole_gib(bytes: u64) -> String {
    format!("{} GB", (bytes as f64 / (1024.0 * 1024.0 * 1024.0)).round() as u64)
}
