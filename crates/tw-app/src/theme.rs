use std::sync::atomic::{AtomicU8, Ordering};

use gpui::{Hsla, Rgba, rgb};
use tw_scripting::HexColor;
use tw_terminal::Rgb;

/// UI palette selected from the command palette.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UiTheme {
    #[default]
    AyuDark,
    AyuMirage,
    AyuLight,
    SolarizedDark,
    SolarizedLight,
}

impl UiTheme {
    pub const ALL: [Self; 5] = [Self::AyuDark, Self::AyuMirage, Self::AyuLight, Self::SolarizedDark, Self::SolarizedLight];

    pub fn label(self) -> &'static str {
        match self {
            Self::AyuDark => "Ayu Dark",
            Self::AyuMirage => "Ayu Mirage",
            Self::AyuLight => "Ayu Light",
            Self::SolarizedDark => "Solarized Dark",
            Self::SolarizedLight => "Solarized Light",
        }
    }
}

/// Used when installed; gpui falls back to the system monospace otherwise.
pub const FONT_FAMILY: &str = "IosevkaTiago Nerd Font";
pub const FONT_SIZE: f32 = 14.0;
pub const LINE_HEIGHT_FACTOR: f32 = 1.3;
pub const SIDEBAR_WIDTH: f32 = 380.0;
pub const TITLEBAR_HEIGHT: f32 = 40.0;

#[derive(Clone, Copy)]
struct Palette {
    bg: Hsla,
    titlebar: Hsla,
    panel: Hsla,
    raised: Hsla,
    border: Hsla,
    text: Hsla,
    muted: Hsla,
    accent: Hsla,
    error: Hsla,
    warn: Hsla,
    button: Hsla,
    button_hover: Hsla,
    code_bg: Hsla,
}

static ACTIVE_THEME: AtomicU8 = AtomicU8::new(UiTheme::AyuDark as u8);

pub fn active() -> UiTheme {
    match ACTIVE_THEME.load(Ordering::Relaxed) {
        1 => UiTheme::AyuMirage,
        2 => UiTheme::AyuLight,
        3 => UiTheme::SolarizedDark,
        4 => UiTheme::SolarizedLight,
        _ => UiTheme::AyuDark,
    }
}

pub fn set_active(theme: UiTheme) {
    ACTIVE_THEME.store(theme as u8, Ordering::Relaxed);
}

fn palette() -> Palette {
    match active() {
        UiTheme::AyuDark => Palette {
            bg: hex(0x0b0e14),
            titlebar: hex(0x0f131a),
            panel: hex(0x151a21),
            raised: hex(0x1f2430),
            border: hex(0x252b38),
            text: hex(0xbfbdb6),
            muted: hex(0x626a73),
            accent: hex(0x59c2ff),
            error: hex(0xf07178),
            warn: hex(0xffb454),
            button: hex(0x1b3a4b),
            button_hover: hex(0x244d63),
            code_bg: hex(0x0b0e14),
        },
        UiTheme::AyuMirage => Palette {
            bg: hex(0x1f2430),
            titlebar: hex(0x171b24),
            panel: hex(0x242936),
            raised: hex(0x2d3443),
            border: hex(0x3d4556),
            text: hex(0xcbccc6),
            muted: hex(0x707a8c),
            accent: hex(0x5ccfe6),
            error: hex(0xf28779),
            warn: hex(0xffad66),
            button: hex(0x2c4054),
            button_hover: hex(0x35546d),
            code_bg: hex(0x1f2430),
        },
        UiTheme::AyuLight => Palette {
            bg: hex(0xf8f9fa),
            titlebar: hex(0xf3f4f5),
            panel: hex(0xffffff),
            raised: hex(0xe6e8eb),
            border: hex(0xd6d9de),
            text: hex(0x5c6166),
            muted: hex(0x8a9199),
            accent: hex(0x399ee6),
            error: hex(0xd95757),
            warn: hex(0xd08b20),
            button: hex(0xdceffb),
            button_hover: hex(0xc5e6f8),
            code_bg: hex(0xf3f4f5),
        },
        UiTheme::SolarizedDark => Palette {
            bg: hex(0x002b36),
            titlebar: hex(0x073642),
            panel: hex(0x073642),
            raised: hex(0x0b3b45),
            border: hex(0x14515c),
            text: hex(0x839496),
            muted: hex(0x586e75),
            accent: hex(0x268bd2),
            error: hex(0xdc322f),
            warn: hex(0xb58900),
            button: hex(0x174b59),
            button_hover: hex(0x1e6070),
            code_bg: hex(0x002b36),
        },
        UiTheme::SolarizedLight => Palette {
            bg: hex(0xfdf6e3),
            titlebar: hex(0xeee8d5),
            panel: hex(0xfdf6e3),
            raised: hex(0xeee8d5),
            border: hex(0xddd6c1),
            text: hex(0x657b83),
            muted: hex(0x93a1a1),
            accent: hex(0x268bd2),
            error: hex(0xdc322f),
            warn: hex(0xb58900),
            button: hex(0xe3f0f5),
            button_hover: hex(0xd4e8f0),
            code_bg: hex(0xeee8d5),
        },
    }
}

pub fn hex(value: u32) -> Hsla {
    rgb(value).into()
}

pub fn bg() -> Hsla { palette().bg }
pub fn titlebar() -> Hsla { palette().titlebar }
pub fn panel() -> Hsla { palette().panel }
pub fn raised() -> Hsla { palette().raised }
pub fn border() -> Hsla { palette().border }
pub fn text() -> Hsla { palette().text }
pub fn muted() -> Hsla { palette().muted }
pub fn accent() -> Hsla { palette().accent }
pub fn error() -> Hsla { palette().error }
pub fn warn() -> Hsla { palette().warn }
pub fn button() -> Hsla { palette().button }
pub fn button_hover() -> Hsla { palette().button_hover }
pub fn code_bg() -> Hsla { palette().code_bg }


/// The terminal's own colours, resolved by libghostty.
pub fn term(color: Rgb) -> Hsla {
    Rgba {
        r: f32::from(color.r) / 255.0,
        g: f32::from(color.g) / 255.0,
        b: f32::from(color.b) / 255.0,
        a: 1.0,
    }
    .into()
}

pub fn plugin(color: HexColor) -> Hsla {
    hex(color.0)
}
