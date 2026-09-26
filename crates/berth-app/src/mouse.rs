//! Mouse → PTY (integrate.md §2): xterm mouse reporting (modes 1000/1002/
//! 1003 with the SGR 1006, UTF-8 1005 or legacy X10 encoding), alternate
//! scroll (1007: wheel → arrow keys on the alternate screen) and wheel delta
//! accumulation. Pure functions of (event, cell, modifiers, modes), as in
//! Alacritty.

use berth_core::TermModes;
use winit::event::MouseScrollDelta;

use crate::input::Mods;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Middle,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseEvent {
    Press(Button),
    Release(Button),
    /// The pointer entered another cell; `held` is the pressed button.
    Motion {
        held: Option<Button>,
    },
    WheelUp,
    WheelDown,
}

fn button_code(b: Button) -> u8 {
    match b {
        Button::Left => 0,
        Button::Middle => 1,
        Button::Right => 2,
    }
}

/// The report for `ev` at cell (`col`, `row`), 0-based; `None` when the
/// current modes do not report it or the cell is not encodable (X10 stops at
/// column/row 223, UTF-8 at 2015).
pub fn report(ev: MouseEvent, col: u16, row: u16, mods: Mods, modes: TermModes) -> Option<Vec<u8>> {
    if !modes.mouse_reporting() {
        return None;
    }
    let base = match ev {
        MouseEvent::Press(b) | MouseEvent::Release(b) => button_code(b),
        MouseEvent::Motion { held: Some(b) } => {
            if !modes.intersects(TermModes::MOUSE_DRAG | TermModes::MOUSE_MOTION) {
                return None;
            }
            button_code(b) + 32
        }
        MouseEvent::Motion { held: None } => {
            if !modes.contains(TermModes::MOUSE_MOTION) {
                return None;
            }
            3 + 32
        }
        MouseEvent::WheelUp => 64,
        MouseEvent::WheelDown => 65,
    };
    let m = 4 * u8::from(mods.shift) + 8 * u8::from(mods.alt) + 16 * u8::from(mods.ctrl);
    let release = matches!(ev, MouseEvent::Release(_));
    if modes.contains(TermModes::SGR_MOUSE) {
        let fin = if release { 'm' } else { 'M' };
        let s = format!(
            "\x1b[<{};{};{}{fin}",
            base + m,
            u32::from(col) + 1,
            u32::from(row) + 1
        );
        return Some(s.into_bytes());
    }
    // Legacy encodings cannot say which button was released.
    let code = if release { 3 } else { base } + m;
    let mut out = b"\x1b[M".to_vec();
    out.push(32 + code);
    let utf8 = modes.contains(TermModes::UTF8_MOUSE);
    for pos in [col, row] {
        let v = u32::from(pos) + 33;
        if utf8 {
            if v > 0x7ff {
                return None;
            }
            let mut buf = [0u8; 4];
            out.extend_from_slice(char::from_u32(v)?.encode_utf8(&mut buf).as_bytes());
        } else {
            out.push(u8::try_from(v).ok()?);
        }
    }
    Some(out)
}

/// Wheel on the alternate screen without mouse reporting (mode 1007): arrow
/// keys, `lines` > 0 meaning up.
pub fn alternate_scroll(lines: i32, modes: TermModes) -> Option<Vec<u8>> {
    if lines == 0
        || modes.mouse_reporting()
        || !modes.contains(TermModes::ALT_SCREEN | TermModes::ALTERNATE_SCROLL)
    {
        return None;
    }
    let key: &[u8] = match (modes.contains(TermModes::APP_CURSOR), lines > 0) {
        (true, true) => b"\x1bOA",
        (true, false) => b"\x1bOB",
        (false, true) => b"\x1b[A",
        (false, false) => b"\x1b[B",
    };
    Some(key.repeat(lines.unsigned_abs() as usize))
}

/// Lines per wheel notch (Alacritty's default `scrolling.multiplier`).
pub const WHEEL_LINES: f64 = 3.0;

/// Accumulates fractional wheel/trackpad deltas into whole lines.
#[derive(Clone, Copy, Debug, Default)]
pub struct WheelAccum {
    lines: f64,
}

impl WheelAccum {
    pub fn reset(&mut self) {
        self.lines = 0.0;
    }

    /// Whole lines to scroll now (positive: up, into the history).
    pub fn lines(&mut self, delta: MouseScrollDelta, cell_height: f64) -> i32 {
        let d = match delta {
            MouseScrollDelta::LineDelta(_, y) => f64::from(y) * WHEEL_LINES,
            MouseScrollDelta::PixelDelta(p) if cell_height > 0.0 => p.y / cell_height,
            MouseScrollDelta::PixelDelta(_) => 0.0,
        };
        if d.signum() != self.lines.signum() && self.lines != 0.0 {
            // Direction change: drop the remainder of the other direction.
            self.lines = 0.0;
        }
        self.lines += d;
        let whole = self.lines.trunc();
        self.lines -= whole;
        whole.clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
    }
}

/// The cell under a point relative to the grid origin, clamped to the grid.
pub fn cell_at(x: f64, y: f64, cell_w: f64, cell_h: f64, cols: u16, rows: u16) -> (u16, u16) {
    let clamp = |v: f64, size: f64, n: u16| -> u16 {
        if size <= 0.0 || n == 0 {
            return 0;
        }
        ((v / size).floor().max(0.0) as u64).min(u64::from(n - 1)) as u16
    };
    (clamp(x, cell_w, cols), clamp(y, cell_h, rows))
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::dpi::PhysicalPosition;

    const CLICK: TermModes = TermModes::MOUSE_CLICK;

    fn s(b: Option<Vec<u8>>) -> String {
        String::from_utf8_lossy(&b.expect("reported")).into_owned()
    }

    #[test]
    fn sgr_press_release_and_modifiers() {
        let m = CLICK | TermModes::SGR_MOUSE;
        let none = Mods::default();
        assert_eq!(
            s(report(MouseEvent::Press(Button::Left), 0, 0, none, m)),
            "\x1b[<0;1;1M"
        );
        assert_eq!(
            s(report(MouseEvent::Release(Button::Left), 9, 4, none, m)),
            "\x1b[<0;10;5m"
        );
        let ctrl_alt = Mods {
            ctrl: true,
            alt: true,
            ..Mods::default()
        };
        assert_eq!(
            s(report(MouseEvent::Press(Button::Right), 2, 3, ctrl_alt, m)),
            "\x1b[<26;3;4M"
        );
        assert_eq!(
            s(report(MouseEvent::WheelUp, 0, 0, none, m)),
            "\x1b[<64;1;1M"
        );
        assert_eq!(
            s(report(MouseEvent::WheelDown, 0, 0, none, m)),
            "\x1b[<65;1;1M"
        );
        // SGR has no coordinate limit.
        assert_eq!(
            s(report(MouseEvent::Press(Button::Middle), 999, 0, none, m)),
            "\x1b[<1;1000;1M"
        );
    }

    #[test]
    fn motion_follows_the_tracking_mode() {
        let none = Mods::default();
        let held = MouseEvent::Motion {
            held: Some(Button::Left),
        };
        let free = MouseEvent::Motion { held: None };
        let sgr = TermModes::SGR_MOUSE;
        assert_eq!(
            report(held, 1, 1, none, CLICK | sgr),
            None,
            "1000: no motion"
        );
        assert_eq!(
            s(report(held, 1, 1, none, TermModes::MOUSE_DRAG | sgr)),
            "\x1b[<32;2;2M"
        );
        assert_eq!(report(free, 1, 1, none, TermModes::MOUSE_DRAG | sgr), None);
        assert_eq!(
            s(report(free, 1, 1, none, TermModes::MOUSE_MOTION | sgr)),
            "\x1b[<35;2;2M"
        );
        assert_eq!(
            report(held, 1, 1, none, TermModes::empty()),
            None,
            "not reporting"
        );
    }

    #[test]
    fn legacy_x10_and_utf8_encodings() {
        let none = Mods::default();
        assert_eq!(
            report(MouseEvent::Press(Button::Left), 0, 0, none, CLICK),
            Some(b"\x1b[M\x20\x21\x21".to_vec())
        );
        // Release is button 3 in the legacy encodings.
        assert_eq!(
            report(MouseEvent::Release(Button::Right), 1, 2, none, CLICK),
            Some(b"\x1b[M\x23\x22\x23".to_vec())
        );
        assert_eq!(
            report(MouseEvent::Press(Button::Left), 222, 0, none, CLICK).map(|v| v[4]),
            Some(255)
        );
        assert_eq!(
            report(MouseEvent::Press(Button::Left), 223, 0, none, CLICK),
            None
        );
        let utf8 = CLICK | TermModes::UTF8_MOUSE;
        let r = report(MouseEvent::Press(Button::Left), 300, 0, none, utf8).expect("utf8");
        assert_eq!(&r[..4], b"\x1b[M\x20");
        assert_eq!(std::str::from_utf8(&r[4..]).expect("utf8"), "\u{14d}!");
        assert_eq!(
            report(MouseEvent::Press(Button::Left), 2015, 0, none, utf8),
            None
        );
    }

    #[test]
    fn alternate_scroll_only_on_the_alternate_screen_without_reporting() {
        let alt = TermModes::ALT_SCREEN | TermModes::ALTERNATE_SCROLL;
        assert_eq!(alternate_scroll(2, alt), Some(b"\x1b[A\x1b[A".to_vec()));
        assert_eq!(
            alternate_scroll(-1, alt | TermModes::APP_CURSOR),
            Some(b"\x1bOB".to_vec())
        );
        assert_eq!(alternate_scroll(1, TermModes::ALT_SCREEN), None);
        assert_eq!(alternate_scroll(1, TermModes::ALTERNATE_SCROLL), None);
        assert_eq!(alternate_scroll(1, alt | CLICK), None);
        assert_eq!(alternate_scroll(0, alt), None);
    }

    #[test]
    fn wheel_deltas_accumulate_into_whole_lines() {
        let mut acc = WheelAccum::default();
        assert_eq!(acc.lines(MouseScrollDelta::LineDelta(0.0, 1.0), 20.0), 3);
        assert_eq!(acc.lines(MouseScrollDelta::LineDelta(0.0, -1.0), 20.0), -3);
        let px = |y| MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, y));
        assert_eq!(acc.lines(px(15.0), 20.0), 0);
        assert_eq!(acc.lines(px(15.0), 20.0), 1, "30 px = 1.5 lines");
        assert_eq!(
            acc.lines(px(-5.0), 20.0),
            0,
            "direction change drops the remainder"
        );
        assert_eq!(acc.lines(px(-40.0), 20.0), -2);
    }

    #[test]
    fn cells_are_clamped_to_the_grid() {
        assert_eq!(cell_at(0.0, 0.0, 10.0, 20.0, 80, 24), (0, 0));
        assert_eq!(cell_at(19.9, 39.9, 10.0, 20.0, 80, 24), (1, 1));
        assert_eq!(cell_at(-5.0, 1e9, 10.0, 20.0, 80, 24), (0, 23));
        assert_eq!(cell_at(5.0, 5.0, 0.0, 0.0, 80, 24), (0, 0));
    }
}
