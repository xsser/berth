//! Colors used to answer OSC 4 / 10 / 11 / 12 queries that the application
//! has not overridden itself.
//!
//! `alacritty_terminal` leaves unset palette entries to the embedding UI
//! (`Event::ColorRequest`). That UI is the GUI, in another process: it sends
//! the colors it paints the grid with (`TermColors`, see
//! `Terminal::set_default_colors`), and [`gui_color`] answers from them.
//! Until it has, and for any entry it left out, the answer is Alacritty's
//! built-in default scheme ([`default_color`], the `alacritty` crate's
//! `config::color` defaults): the 16 ANSI colors below, the xterm 6×6×6 cube
//! and gray ramp, `#d8d8d8` on `#181818`, and dim colors at 2/3 brightness.

use alacritty_terminal::vte::ansi::{NamedColor, Rgb};
use berth_core::TermColors;

const fn rgb(r: u8, g: u8, b: u8) -> Rgb {
    Rgb { r, g, b }
}

const NORMAL: [Rgb; 8] = [
    rgb(0x18, 0x18, 0x18),
    rgb(0xac, 0x42, 0x42),
    rgb(0x90, 0xa9, 0x59),
    rgb(0xf4, 0xbf, 0x75),
    rgb(0x6a, 0x9f, 0xb5),
    rgb(0xaa, 0x75, 0x9f),
    rgb(0x75, 0xb5, 0xaa),
    rgb(0xd8, 0xd8, 0xd8),
];

const BRIGHT: [Rgb; 8] = [
    rgb(0x6b, 0x6b, 0x6b),
    rgb(0xc5, 0x55, 0x55),
    rgb(0xaa, 0xc4, 0x74),
    rgb(0xfe, 0xca, 0x88),
    rgb(0x82, 0xb8, 0xc8),
    rgb(0xc2, 0x8c, 0xb8),
    rgb(0x93, 0xd3, 0xc3),
    rgb(0xf8, 0xf8, 0xf8),
];

const FOREGROUND: Rgb = rgb(0xd8, 0xd8, 0xd8);
const BACKGROUND: Rgb = rgb(0x18, 0x18, 0x18);
const DIM_FACTOR: f32 = 0.66;

const FG: usize = NamedColor::Foreground as usize;
const BG: usize = NamedColor::Background as usize;
const CURSOR: usize = NamedColor::Cursor as usize;
const DIM_BLACK: usize = NamedColor::DimBlack as usize;
const DIM_WHITE: usize = NamedColor::DimWhite as usize;
const BRIGHT_FG: usize = NamedColor::BrightForeground as usize;
const DIM_FG: usize = NamedColor::DimForeground as usize;

/// The GUI's color for an index of `alacritty_terminal::term::color::Colors`,
/// if it sent one. Programs can ask for the palette (OSC 4) and for the
/// foreground, background and cursor (OSC 10 / 11 / 12); no other index
/// reaches a query.
pub(crate) fn gui_color(colors: &TermColors, index: usize) -> Option<Rgb> {
    let [r, g, b] = match index {
        0..=255 => *colors.palette.get(index)?,
        FG => colors.foreground,
        BG => colors.background,
        CURSOR => colors.cursor,
        _ => return None,
    };
    Some(rgb(r, g, b))
}

/// Default color for an index of `alacritty_terminal::term::color::Colors`
/// (0..=255 palette, then the named dynamic colors).
pub(crate) fn default_color(index: usize) -> Option<Rgb> {
    let color = match index {
        0..=7 => NORMAL[index],
        8..=15 => BRIGHT[index - 8],
        16..=231 => {
            let cube = index - 16;
            rgb(
                cube_level(cube / 36),
                cube_level(cube / 6 % 6),
                cube_level(cube % 6),
            )
        }
        232..=255 => {
            let value = u8::try_from((index - 232) * 10 + 8).ok()?;
            rgb(value, value, value)
        }
        FG | BRIGHT_FG => FOREGROUND,
        BG => BACKGROUND,
        // Alacritty draws the cursor with the cell's foreground by default.
        CURSOR => FOREGROUND,
        DIM_BLACK..=DIM_WHITE => NORMAL[index - DIM_BLACK] * DIM_FACTOR,
        DIM_FG => FOREGROUND * DIM_FACTOR,
        _ => return None,
    };
    Some(color)
}

fn cube_level(step: usize) -> u8 {
    match step {
        0 => 0,
        // 1..=5 → 95, 135, 175, 215, 255
        _ => u8::try_from(step * 40 + 55).unwrap_or(u8::MAX),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covers_every_alacritty_color_slot() {
        for index in 0..alacritty_terminal::term::color::COUNT {
            assert!(default_color(index).is_some(), "index {index}");
        }
        assert_eq!(default_color(alacritty_terminal::term::color::COUNT), None);
    }

    #[test]
    fn xterm_cube_and_ramp() {
        assert_eq!(default_color(16), Some(rgb(0, 0, 0)));
        assert_eq!(default_color(196), Some(rgb(255, 0, 0)));
        assert_eq!(default_color(231), Some(rgb(255, 255, 255)));
        assert_eq!(default_color(232), Some(rgb(8, 8, 8)));
        assert_eq!(default_color(255), Some(rgb(238, 238, 238)));
        assert_eq!(default_color(BG), Some(BACKGROUND));
    }
}
