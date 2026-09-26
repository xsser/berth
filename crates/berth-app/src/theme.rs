//! Built-in color theme.
//!
//! Values are Ghostty's defaults, taken from `ghostty +show-config --default`
//! on the development machine (background `#282c34`, foreground `#ffffff`,
//! Tomorrow Night 16-color palette, xterm 256-color cube). Cursor color empty
//! in Ghostty means "foreground", cursor text means "background".

use berth_core::{CellFlags, Color, Style};

pub type Rgb = [u8; 3];

/// Ghostty `faint-opacity` default: DIM text is drawn at half strength.
pub const DIM_FACTOR: f32 = 0.5;

#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Rgb,
    pub cursor_text: Rgb,
    /// 0..=15 named colors, 16..=231 color cube, 232..=255 gray ramp.
    pub palette: [Rgb; 256],
}

const GHOSTTY_16: [Rgb; 16] = [
    [0x1d, 0x1f, 0x21],
    [0xcc, 0x66, 0x66],
    [0xb5, 0xbd, 0x68],
    [0xf0, 0xc6, 0x74],
    [0x81, 0xa2, 0xbe],
    [0xb2, 0x94, 0xbb],
    [0x8a, 0xbe, 0xb7],
    [0xc5, 0xc8, 0xc6],
    [0x66, 0x66, 0x66],
    [0xd5, 0x4e, 0x53],
    [0xb9, 0xca, 0x4a],
    [0xe7, 0xc5, 0x47],
    [0x7a, 0xa6, 0xda],
    [0xc3, 0x97, 0xd8],
    [0x70, 0xc0, 0xb1],
    [0xea, 0xea, 0xea],
];

/// xterm 256-color palette with the given first 16 entries.
pub fn xterm_palette(named: &[Rgb; 16]) -> [Rgb; 256] {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let mut p = [[0u8; 3]; 256];
    p[..16].copy_from_slice(named);
    for i in 0..216usize {
        p[16 + i] = [LEVELS[i / 36], LEVELS[(i / 6) % 6], LEVELS[i % 6]];
    }
    for i in 0..24usize {
        let v = 8 + 10 * i as u8;
        p[232 + i] = [v, v, v];
    }
    p
}

impl Default for Theme {
    fn default() -> Self {
        Self::ghostty_default()
    }
}

/// Colors of one cell after applying theme defaults and SGR attributes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellColors {
    pub fg: Rgb,
    pub bg: Rgb,
    /// True when the background equals the theme background and nothing
    /// needs to be painted (the clear color already covers it).
    pub bg_is_default: bool,
    pub underline: Rgb,
    /// Glyph alpha: 0 for HIDDEN, otherwise 1.
    pub fg_alpha: f32,
}

impl Theme {
    pub fn ghostty_default() -> Self {
        let fg = [0xff, 0xff, 0xff];
        let bg = [0x28, 0x2c, 0x34];
        Self {
            foreground: fg,
            background: bg,
            cursor: fg,
            cursor_text: bg,
            palette: xterm_palette(&GHOSTTY_16),
        }
    }

    pub fn color(&self, c: Color, default: Rgb) -> Rgb {
        match c {
            Color::Default => default,
            Color::Indexed(i) => self.palette[i as usize],
            Color::Rgb(r, g, b) => [r, g, b],
        }
    }

    /// Resolve a cell style. `selected` applies Ghostty's default selection
    /// look (foreground and background swapped).
    pub fn resolve(&self, style: &Style, selected: bool) -> CellColors {
        let mut fg = self.color(style.fg, self.foreground);
        let mut bg = self.color(style.bg, self.background);
        let mut bg_is_default = style.bg == Color::Default;
        let mut inverse = style.flags.contains(CellFlags::INVERSE);
        if selected {
            inverse = !inverse;
        }
        if inverse {
            std::mem::swap(&mut fg, &mut bg);
            bg_is_default = bg == self.background;
        }
        if style.flags.contains(CellFlags::DIM) {
            fg = mix(bg, fg, DIM_FACTOR);
        }
        let underline = match style.underline {
            Color::Default => fg,
            c => self.color(c, fg),
        };
        let fg_alpha = if style.flags.contains(CellFlags::HIDDEN) {
            0.0
        } else {
            1.0
        };
        CellColors {
            fg,
            bg,
            bg_is_default,
            underline,
            fg_alpha,
        }
    }
}

/// Linear mix in sRGB space: `t = 0` gives `a`, `t = 1` gives `b`.
pub fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let f = |x: u8, y: u8| {
        (x as f32 + (y as f32 - x as f32) * t)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    [f(a[0], b[0]), f(a[1], b[1]), f(a[2], b[2])]
}

pub fn rgba(c: Rgb, alpha: f32) -> [f32; 4] {
    [
        c[0] as f32 / 255.0,
        c[1] as f32 / 255.0,
        c[2] as f32 / 255.0,
        alpha,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_matches_xterm_cube_and_ramp() {
        let t = Theme::default();
        assert_eq!(t.palette[1], [0xcc, 0x66, 0x66]);
        assert_eq!(t.palette[16], [0, 0, 0]);
        assert_eq!(t.palette[17], [0, 0, 0x5f]); // Ghostty default palette 17 = #00005f
        assert_eq!(t.palette[21], [0, 0, 255]);
        assert_eq!(t.palette[196], [255, 0, 0]);
        assert_eq!(t.palette[231], [255, 255, 255]);
        assert_eq!(t.palette[232], [8, 8, 8]);
        assert_eq!(t.palette[255], [238, 238, 238]);
    }

    #[test]
    fn inverse_swaps_and_marks_background_non_default() {
        let t = Theme::default();
        let s = Style {
            flags: CellFlags::INVERSE,
            ..Default::default()
        };
        let c = t.resolve(&s, false);
        assert_eq!(c.fg, t.background);
        assert_eq!(c.bg, t.foreground);
        assert!(!c.bg_is_default);
        // Selecting an inverse cell flips it back.
        let c = t.resolve(&s, true);
        assert_eq!(c.fg, t.foreground);
        assert!(c.bg_is_default);
    }

    #[test]
    fn dim_hidden_and_underline_color() {
        let t = Theme::default();
        let s = Style {
            fg: Color::Rgb(200, 100, 0),
            underline: Color::Indexed(2),
            flags: CellFlags::DIM | CellFlags::HIDDEN,
            ..Default::default()
        };
        let c = t.resolve(&s, false);
        assert_eq!(c.fg, mix(t.background, [200, 100, 0], DIM_FACTOR));
        assert_eq!(c.fg_alpha, 0.0);
        assert_eq!(c.underline, t.palette[2]);
        let plain = t.resolve(&Style::default(), false);
        assert!(plain.bg_is_default);
        assert_eq!(plain.underline, t.foreground);
    }
}
