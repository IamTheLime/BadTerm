use gpui::{Hsla, Rgba, rgb};
use tw_scripting::HexColor;
use tw_terminal::Rgb;

/// UI palette selected from the settings menu.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UiTheme {
    #[default]
    AyuDark,
}

impl UiTheme {
    pub fn label(self) -> &'static str {
        match self {
            Self::AyuDark => "Ayu Dark",
        }
    }
}

/// Used when installed; gpui falls back to the system monospace otherwise.
pub const FONT_FAMILY: &str = "IosevkaTiago Nerd Font";
pub const FONT_SIZE: f32 = 14.0;
pub const LINE_HEIGHT_FACTOR: f32 = 1.3;
pub const SIDEBAR_WIDTH: f32 = 380.0;
pub const TITLEBAR_HEIGHT: f32 = 40.0;

pub fn hex(value: u32) -> Hsla {
    rgb(value).into()
}

pub fn bg() -> Hsla {
    hex(0x0b0e14)
}
pub fn titlebar() -> Hsla {
    hex(0x0f131a)
}
pub fn panel() -> Hsla {
    hex(0x151a21)
}
pub fn raised() -> Hsla {
    hex(0x1f2430)
}
pub fn border() -> Hsla {
    hex(0x252b38)
}
pub fn text() -> Hsla {
    hex(0xbfbdb6)
}
pub fn muted() -> Hsla {
    hex(0x626a73)
}
pub fn accent() -> Hsla {
    hex(0x59c2ff)
}
pub fn error() -> Hsla {
    hex(0xf07178)
}
pub fn warn() -> Hsla {
    hex(0xffb454)
}
pub fn button() -> Hsla {
    hex(0x1b3a4b)
}
pub fn button_hover() -> Hsla {
    hex(0x244d63)
}
pub fn code_bg() -> Hsla {
    hex(0x0b0e14)
}

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
