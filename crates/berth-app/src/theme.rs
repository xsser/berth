//! Built-in color themes and per-cell color resolution.
//!
//! Two presets ship. [`Theme::light`] is the default: a white background
//! with the One Light 16-color palette, two of whose colors are darkened so
//! every entry clears 3:1 against white (see the function's docs).
//! [`Theme::dark`] keeps the values berth shipped before: Ghostty's
//! defaults, taken from `ghostty +show-config --default` on the development
//! machine (background `#282c34`, foreground `#ffffff`, Tomorrow Night
//! 16-color palette). Both build 16..=255 with [`xterm_palette`]. Cursor
//! color empty in Ghostty means "foreground", cursor text means
//! "background".
//!
//! `[theme]` in `config.toml` picks the preset and overrides any of its
//! colors ([`crate::config`]); everything drawn outside the grid derives
//! its colors from the resulting [`Theme`], keyed off [`Theme::is_light`]
//! (sidebar: [`crate::sidebar::Palette`], pane chrome: `app::pane_chrome`).

use berth_core::{CellFlags, Color, Style};

pub type Rgb = [u8; 3];

/// Ghostty `faint-opacity` default: DIM text is drawn at half strength.
pub const DIM_FACTOR: f32 = 0.5;

/// Accent of the dark preset (「等授权」 pulse, focused pane border).
pub const DARK_ACCENT: Rgb = [0xde, 0x93, 0x5f];
/// Accent of the light preset: the dark one reads at 2.5:1 on white, this
/// one at 4.7:1.
pub const LIGHT_ACCENT: Rgb = [0xb3, 0x5c, 0x00];

#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Rgb,
    pub cursor_text: Rgb,
    /// Chrome accent (`[theme].accent`): the 「等授权」 pulse, the unread
    /// count, the focused pane's border. Part of the theme so that every
    /// color the window draws travels in one object.
    pub accent: Rgb,
    /// 0..=15 named colors, 16..=231 color cube, 232..=255 gray ramp.
    pub palette: [Rgb; 256],
}

/// Tomorrow Night, Ghostty's default 16 colors.
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

/// One Light's 16 colors, with green (`#98c379` → `#3f8a3e`), yellow
/// (`#e5c07b` → `#9a6a00`), white (`#a0a1a7` → `#8e9096`) and bright
/// magenta (`#c678dd` → `#b45ccd`) darkened: upstream's are meant for
/// syntax highlighting on a slightly gray editor background and fall under
/// 3:1 on pure white.
const ONE_LIGHT_16: [Rgb; 16] = [
    [0x38, 0x3a, 0x42],
    [0xe4, 0x56, 0x49],
    [0x3f, 0x8a, 0x3e],
    [0x9a, 0x6a, 0x00],
    [0x3a, 0x67, 0xd8],
    [0xa6, 0x26, 0xa4],
    [0x01, 0x84, 0xbc],
    [0x8e, 0x90, 0x96],
    [0x4f, 0x52, 0x5e],
    [0xe0, 0x6c, 0x75],
    [0x50, 0xa1, 0x4f],
    [0xc1, 0x84, 0x01],
    [0x40, 0x78, 0xf2],
    [0xb4, 0x5c, 0xcd],
    [0x09, 0x97, 0xb3],
    [0x38, 0x3a, 0x42],
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
        Self::light()
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
    /// White background, One Light colors. berth's default.
    pub fn light() -> Self {
        Self::from_parts(
            [0x1f, 0x23, 0x28],
            [0xff, 0xff, 0xff],
            LIGHT_ACCENT,
            &ONE_LIGHT_16,
        )
    }

    /// Ghostty's defaults (background `#282c34`, Tomorrow Night colors).
    pub fn dark() -> Self {
        Self::from_parts(
            [0xff, 0xff, 0xff],
            [0x28, 0x2c, 0x34],
            DARK_ACCENT,
            &GHOSTTY_16,
        )
    }

    fn from_parts(fg: Rgb, bg: Rgb, accent: Rgb, named: &[Rgb; 16]) -> Self {
        Self {
            foreground: fg,
            background: bg,
            cursor: fg,
            cursor_text: bg,
            accent,
            palette: xterm_palette(named),
        }
    }

    /// Whether the background is a light one, which decides how the chrome
    /// around the grid is derived (mixing toward black darkens a dark
    /// theme's sidebar but only muddies a white one).
    pub fn is_light(&self) -> bool {
        relative_luminance(self.background) > 0.5
    }

    /// The accent a background of this brightness should get when
    /// `[theme].accent` is not set.
    pub fn default_accent(&self) -> Rgb {
        if self.is_light() {
            LIGHT_ACCENT
        } else {
            DARK_ACCENT
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

/// WCAG 2.1 relative luminance: sRGB components gamma-expanded, then
/// weighted. 0 for black, 1 for white.
pub fn relative_luminance(c: Rgb) -> f32 {
    let channel = |v: u8| {
        let s = v as f32 / 255.0;
        if s <= 0.04045 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(c[0]) + 0.7152 * channel(c[1]) + 0.0722 * channel(c[2])
}

/// WCAG 2.1 contrast ratio, 1.0 (same color) to 21.0 (black on white).
pub fn contrast_ratio(a: Rgb, b: Rgb) -> f32 {
    let (x, y) = (relative_luminance(a), relative_luminance(b));
    let (hi, lo) = if x > y { (x, y) } else { (y, x) };
    (hi + 0.05) / (lo + 0.05)
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
        let t = Theme::dark();
        assert_eq!(t.palette[1], [0xcc, 0x66, 0x66]);
        assert_eq!(t.palette[16], [0, 0, 0]);
        assert_eq!(t.palette[17], [0, 0, 0x5f]); // Ghostty default palette 17 = #00005f
        assert_eq!(t.palette[21], [0, 0, 255]);
        assert_eq!(t.palette[196], [255, 0, 0]);
        assert_eq!(t.palette[231], [255, 255, 255]);
        assert_eq!(t.palette[232], [8, 8, 8]);
        assert_eq!(t.palette[255], [238, 238, 238]);
        // The cube and the ramp do not depend on the preset.
        assert_eq!(Theme::light().palette[16..], t.palette[16..]);
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

    #[test]
    fn light_is_the_default_preset() {
        let t = Theme::default();
        assert_eq!(t, Theme::light());
        assert_eq!(t.background, [0xff, 0xff, 0xff]);
        assert_eq!(t.cursor, t.foreground);
        assert_eq!(t.cursor_text, t.background);
        assert!(t.is_light());
        assert!(!Theme::dark().is_light());
        assert_eq!(t.accent, LIGHT_ACCENT);
        assert_eq!(t.default_accent(), LIGHT_ACCENT);
        assert_eq!(Theme::dark().accent, DARK_ACCENT);
        assert_eq!(Theme::dark().default_accent(), DARK_ACCENT);
    }

    #[test]
    fn luminance_and_contrast_follow_wcag() {
        assert!(relative_luminance([0, 0, 0]).abs() < 1e-6);
        assert!((relative_luminance([255, 255, 255]) - 1.0).abs() < 1e-6);
        assert!((contrast_ratio([0, 0, 0], [255, 255, 255]) - 21.0).abs() < 1e-3);
        assert!((contrast_ratio([1, 2, 3], [1, 2, 3]) - 1.0).abs() < 1e-6);
        // Gamma expansion matters: a linear average would put mid gray at
        // 0.5, WCAG puts it near 0.216.
        assert!((relative_luminance([128, 128, 128]) - 0.2158).abs() < 1e-3);
        // `is_light` splits the two presets on that curve.
        assert!(relative_luminance([0x28, 0x2c, 0x34]) < 0.5);
    }

    /// Every color the light preset can put on its own background stays
    /// above the 3:1 non-text contrast floor (WCAG 1.4.11). Bright colors
    /// sit between 3:1 and 4.5:1 by design: they are accents, not body
    /// text, and darkening them further loses the "bright" reading.
    ///
    /// `cargo test -p berth-app light_preset -- --nocapture` prints the
    /// whole table, which is how a change to the palette is reviewed.
    #[test]
    fn light_preset_clears_three_to_one_on_its_background() {
        const NAMES: [&str; 16] = [
            "black",
            "red",
            "green",
            "yellow",
            "blue",
            "magenta",
            "cyan",
            "white",
            "br black",
            "br red",
            "br green",
            "br yellow",
            "br blue",
            "br magenta",
            "br cyan",
            "br white",
        ];
        let t = Theme::light();
        let bg = t.background;
        println!("light preset on {bg:02x?} (WCAG contrast)");
        for (i, c) in t.palette[..16].iter().enumerate() {
            let r = contrast_ratio(*c, bg);
            println!(
                "  {i:>2} {:<11} #{:02x}{:02x}{:02x}  {r:5.2}:1",
                NAMES[i], c[0], c[1], c[2]
            );
            assert!(r >= 3.0, "ansi {i} {c:02x?} is {r:.2}:1 on {bg:02x?}");
        }
        for (name, c) in [
            ("foreground", t.foreground),
            ("accent", LIGHT_ACCENT),
            ("cursor", t.cursor),
        ] {
            let r = contrast_ratio(c, bg);
            println!(
                "     {name:<11} #{:02x}{:02x}{:02x}  {r:5.2}:1",
                c[0], c[1], c[2]
            );
        }
        assert!(contrast_ratio(t.foreground, bg) >= 7.0);
        assert!(contrast_ratio(LIGHT_ACCENT, bg) >= 4.5);
        assert!(contrast_ratio(DARK_ACCENT, Theme::dark().background) >= 4.5);
    }
}
