//! Keyboard input → xterm byte sequences (DESIGN §8.2).
//!
//! `encode` is a pure function of (key, modifiers, terminal modes) so every
//! case is unit-tested without a window. `key_from_winit` is the thin adapter
//! from winit's `KeyEvent` fields. ⌘ (logo) chords never reach the PTY: they
//! are GUI shortcuts and `encode` returns `None` for them.
//!
//! Conventions (xterm, as used by Ghostty/Alacritty):
//! - cursor keys: `CSI A`, or `SS3 A` in DECCKM (`APP_CURSOR`); with
//!   modifiers `CSI 1 ; m A` where `m = 1 + shift + 2·alt + 4·ctrl`.
//! - tilde keys: `CSI n ~` / `CSI n ; m ~` (Insert 2, Delete 3, PgUp 5,
//!   PgDn 6, F5..F12 = 15 17 18 19 20 21 23 24).
//! - F1..F4: `SS3 P..S`, with modifiers `CSI 1 ; m P..S`.
//! - Enter `\r`, Backspace `\x7f` (Ctrl: `\x08`), Tab `\t`, Shift+Tab `CSI Z`.
//! - Ctrl+letter → C0 control; Alt prefixes the encoded key with ESC.

use berth_core::TermModes;
use winit::keyboard::{Key as WinitKey, ModifiersState, NamedKey};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    /// ⌘ on macOS. Reserved for GUI shortcuts.
    pub logo: bool,
}

#[cfg(test)]
impl Mods {
    pub const NONE: Mods = Mods {
        shift: false,
        ctrl: false,
        alt: false,
        logo: false,
    };
    pub const SHIFT: Mods = Mods {
        shift: true,
        ..Mods::NONE
    };
    pub const CTRL: Mods = Mods {
        ctrl: true,
        ..Mods::NONE
    };
    pub const ALT: Mods = Mods {
        alt: true,
        ..Mods::NONE
    };
    pub const LOGO: Mods = Mods {
        logo: true,
        ..Mods::NONE
    };
}

impl Mods {
    /// `alt_is_meta`: whether the Alt/Option key acts as Meta (ESC prefix).
    /// On macOS this follows the window's `OptionAsAlt` setting; when Option
    /// composes characters instead, the composed text arrives in the key
    /// event and Alt must not be applied a second time.
    pub fn from_winit(m: ModifiersState, alt_is_meta: bool) -> Self {
        Mods {
            shift: m.shift_key(),
            ctrl: m.control_key(),
            alt: m.alt_key() && alt_is_meta,
            logo: m.super_key(),
        }
    }

    /// xterm modifier parameter (`1 + shift + 2·alt + 4·ctrl`).
    fn param(self) -> u8 {
        1 + self.shift as u8 + 2 * self.alt as u8 + 4 * self.ctrl as u8
    }

    fn any(self) -> bool {
        self.shift || self.alt || self.ctrl
    }
}

/// A key press, normalised away from any windowing library.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyInput {
    /// A single character key (already shifted, but not Ctrl-transformed).
    Char(char),
    /// Multi-character text (e.g. a dead-key composition).
    Text(String),
    Enter,
    Tab,
    Backspace,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    /// Function key 1..=12.
    F(u8),
}

const ESC: u8 = 0x1b;

/// Encode one key press. `None` means "nothing to send" (GUI shortcut or an
/// unsupported key).
pub fn encode(key: &KeyInput, mods: Mods, modes: TermModes) -> Option<Vec<u8>> {
    if mods.logo {
        return None;
    }
    let app_cursor = modes.contains(TermModes::APP_CURSOR);
    let bytes = match key {
        KeyInput::Up => cursor_key(b'A', mods, app_cursor),
        KeyInput::Down => cursor_key(b'B', mods, app_cursor),
        KeyInput::Right => cursor_key(b'C', mods, app_cursor),
        KeyInput::Left => cursor_key(b'D', mods, app_cursor),
        KeyInput::Home => cursor_key(b'H', mods, app_cursor),
        KeyInput::End => cursor_key(b'F', mods, app_cursor),
        KeyInput::Insert => tilde_key(2, mods),
        KeyInput::Delete => tilde_key(3, mods),
        KeyInput::PageUp => tilde_key(5, mods),
        KeyInput::PageDown => tilde_key(6, mods),
        KeyInput::F(n @ 1..=4) => {
            let fin = b"PQRS"[(*n - 1) as usize];
            if mods.any() {
                format!("\x1b[1;{}{}", mods.param(), fin as char).into_bytes()
            } else {
                vec![ESC, b'O', fin]
            }
        }
        KeyInput::F(n @ 5..=12) => {
            const CODES: [u8; 8] = [15, 17, 18, 19, 20, 21, 23, 24];
            tilde_key(CODES[(*n - 5) as usize], mods)
        }
        KeyInput::F(_) => return None,
        KeyInput::Enter => alt_prefixed(mods, vec![b'\r']),
        KeyInput::Tab if mods.shift => b"\x1b[Z".to_vec(),
        KeyInput::Tab => alt_prefixed(mods, vec![b'\t']),
        KeyInput::Backspace => alt_prefixed(mods, vec![if mods.ctrl { 0x08 } else { 0x7f }]),
        KeyInput::Escape => alt_prefixed(mods, vec![ESC]),
        KeyInput::Char(c) => {
            let base = if mods.ctrl {
                match ctrl_byte(*c) {
                    Some(b) => vec![b],
                    None => c.to_string().into_bytes(),
                }
            } else {
                c.to_string().into_bytes()
            };
            alt_prefixed(mods, base)
        }
        KeyInput::Text(s) if s.is_empty() => return None,
        KeyInput::Text(s) => s.as_bytes().to_vec(),
    };
    Some(bytes)
}

fn cursor_key(fin: u8, mods: Mods, app_cursor: bool) -> Vec<u8> {
    if mods.any() {
        format!("\x1b[1;{}{}", mods.param(), fin as char).into_bytes()
    } else if app_cursor {
        vec![ESC, b'O', fin]
    } else {
        vec![ESC, b'[', fin]
    }
}

fn tilde_key(code: u8, mods: Mods) -> Vec<u8> {
    if mods.any() {
        format!("\x1b[{code};{}~", mods.param()).into_bytes()
    } else {
        format!("\x1b[{code}~").into_bytes()
    }
}

fn alt_prefixed(mods: Mods, mut bytes: Vec<u8>) -> Vec<u8> {
    if mods.alt {
        bytes.insert(0, ESC);
    }
    bytes
}

/// C0 control byte for Ctrl+`c` (xterm/VT220 conventions).
pub fn ctrl_byte(c: char) -> Option<u8> {
    Some(match c {
        'a'..='z' => c as u8 - b'a' + 1,
        'A'..='Z' => c as u8 - b'A' + 1,
        '@' | ' ' | '2' => 0x00,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '6' => 0x1e,
        '_' | '-' | '7' => 0x1f,
        '?' | '8' => 0x7f,
        _ => return None,
    })
}

/// Map winit's logical key (+ produced text) to a `KeyInput`. Pure modifier
/// keys and keys without a terminal meaning return `None`.
pub fn key_from_winit(logical: &WinitKey, text: Option<&str>) -> Option<KeyInput> {
    match logical {
        WinitKey::Named(named) => Some(match named {
            NamedKey::Enter => KeyInput::Enter,
            NamedKey::Tab => KeyInput::Tab,
            NamedKey::Backspace => KeyInput::Backspace,
            NamedKey::Escape => KeyInput::Escape,
            NamedKey::Space => KeyInput::Char(' '),
            NamedKey::ArrowUp => KeyInput::Up,
            NamedKey::ArrowDown => KeyInput::Down,
            NamedKey::ArrowLeft => KeyInput::Left,
            NamedKey::ArrowRight => KeyInput::Right,
            NamedKey::Home => KeyInput::Home,
            NamedKey::End => KeyInput::End,
            NamedKey::PageUp => KeyInput::PageUp,
            NamedKey::PageDown => KeyInput::PageDown,
            NamedKey::Insert => KeyInput::Insert,
            NamedKey::Delete => KeyInput::Delete,
            NamedKey::F1 => KeyInput::F(1),
            NamedKey::F2 => KeyInput::F(2),
            NamedKey::F3 => KeyInput::F(3),
            NamedKey::F4 => KeyInput::F(4),
            NamedKey::F5 => KeyInput::F(5),
            NamedKey::F6 => KeyInput::F(6),
            NamedKey::F7 => KeyInput::F(7),
            NamedKey::F8 => KeyInput::F(8),
            NamedKey::F9 => KeyInput::F(9),
            NamedKey::F10 => KeyInput::F(10),
            NamedKey::F11 => KeyInput::F(11),
            NamedKey::F12 => KeyInput::F(12),
            _ => return None,
        }),
        WinitKey::Character(s) => {
            let mut chars = s.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => Some(KeyInput::Char(c)),
                (Some(_), Some(_)) => Some(KeyInput::Text(s.to_string())),
                _ => None,
            }
        }
        WinitKey::Dead(_) | WinitKey::Unidentified(_) => text
            .filter(|t| !t.is_empty() && !t.chars().any(char::is_control))
            .map(|t| KeyInput::Text(t.to_string())),
    }
}

/// The parts of a winit `KeyEvent` the routing decision needs. (`KeyEvent`
/// itself cannot be built in tests: its `platform_specific` field is
/// crate-private.)
#[derive(Clone, Copy, Debug)]
pub struct KeyPress<'a> {
    pub logical: &'a WinitKey,
    pub text: Option<&'a str>,
    pub pressed: bool,
    pub synthetic: bool,
    pub mods: ModifiersState,
}

/// IME state when the key event arrives.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ImeGate {
    /// `Ime::Enabled` seen and not yet `Disabled`.
    pub enabled: bool,
    /// A non-empty preedit is showing (composition in progress).
    pub composing: bool,
}

/// GUI shortcuts (⌘ chords, DESIGN §8.3). They never reach the PTY.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Shortcut {
    /// ⌘Q.
    Quit,
    /// ⌘N: new session in the current workspace.
    NewSession,
    /// ⌘⇧N: new workspace (folder picker).
    NewWorkspace,
    /// ⌘W: close the focused session.
    Close,
    /// ⌘1..⌘9.
    Jump(u8),
    /// ⌘K: command palette.
    Palette,
    /// ⌘C.
    Copy,
    /// ⌘V.
    Paste,
    /// Any other ⌘ chord.
    Unbound(String),
}

/// Local scrollback keys (Shift+PageUp/PageDown/Home/End outside the
/// alternate screen, as in Alacritty).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollKey {
    PageUp,
    PageDown,
    Top,
    Bottom,
}

/// What a key press does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyAction {
    /// Write `bytes` to the PTY; `desc` names the chord (status row, logs).
    Forward { desc: String, bytes: Vec<u8> },
    /// ⌘ chord for the app. Passes even while the IME is composing.
    Shortcut(Shortcut),
    /// Scroll the local view; nothing is sent.
    Scroll(ScrollKey),
    /// The IME is composing: the key belongs to it (Backspace edits the
    /// preedit, Enter commits it, arrows move within it), not to the PTY.
    SwallowedByIme { desc: String },
    /// Release, synthetic, modifier-only or unmapped key.
    Ignore,
}

/// Route one key press (DESIGN §8.2).
///
/// winit 0.30 on macOS documents that keys consumed by the input method do
/// not also arrive as `KeyboardInput`, but that is unverified with real IMEs
/// here, so the terminal enforces it itself: while a preedit is showing,
/// every key except ⌘ chords is swallowed. With no preedit — IME enabled or
/// not — keys are encoded normally. Option composes characters (not Meta,
/// Ghostty's default), so Alt is never applied on top of the composed text.
pub fn decide_key(press: &KeyPress, ime: ImeGate, modes: TermModes) -> KeyAction {
    if !press.pressed || press.synthetic {
        return KeyAction::Ignore;
    }
    if press.mods.super_key() {
        return KeyAction::Shortcut(shortcut_for(press.logical, press.mods));
    }
    let mods = Mods::from_winit(press.mods, false);
    let Some(key) = key_from_winit(press.logical, press.text) else {
        return KeyAction::Ignore;
    };
    let desc = describe(&key, mods);
    if ime.composing {
        return KeyAction::SwallowedByIme { desc };
    }
    if mods
        == (Mods {
            shift: true,
            ..Mods::default()
        })
        && !modes.contains(TermModes::ALT_SCREEN)
    {
        let scroll = match key {
            KeyInput::PageUp => Some(ScrollKey::PageUp),
            KeyInput::PageDown => Some(ScrollKey::PageDown),
            KeyInput::Home => Some(ScrollKey::Top),
            KeyInput::End => Some(ScrollKey::Bottom),
            _ => None,
        };
        if let Some(k) = scroll {
            return KeyAction::Scroll(k);
        }
    }
    match encode(&key, mods, modes) {
        Some(bytes) => KeyAction::Forward { desc, bytes },
        None => KeyAction::Ignore,
    }
}

fn shortcut_for(logical: &WinitKey, mods: ModifiersState) -> Shortcut {
    let WinitKey::Character(c) = logical else {
        return Shortcut::Unbound(format!("⌘{logical:?}"));
    };
    let key = c.to_lowercase();
    let plain = !mods.shift_key() && !mods.control_key() && !mods.alt_key();
    let shift_only = mods.shift_key() && !mods.control_key() && !mods.alt_key();
    match key.as_str() {
        "q" if plain => Shortcut::Quit,
        "n" if plain => Shortcut::NewSession,
        "n" if shift_only => Shortcut::NewWorkspace,
        "w" if plain => Shortcut::Close,
        "k" if plain => Shortcut::Palette,
        "c" if plain => Shortcut::Copy,
        "v" if plain => Shortcut::Paste,
        d if plain && d.len() == 1 && matches!(d.as_bytes()[0], b'1'..=b'9') => {
            Shortcut::Jump(d.as_bytes()[0] - b'0')
        }
        _ => {
            let mut chord = String::from("⌘");
            if mods.shift_key() {
                chord.push('⇧');
            }
            chord.push_str(&c.to_uppercase());
            Shortcut::Unbound(chord)
        }
    }
}

/// Short description of a key chord for the status line ("Ctrl+C", "Shift+Up").
pub fn describe(key: &KeyInput, mods: Mods) -> String {
    let mut s = String::new();
    if mods.ctrl {
        s.push_str("Ctrl+");
    }
    if mods.alt {
        s.push_str("Alt+");
    }
    let is_text = matches!(key, KeyInput::Char(c) if *c != ' ') || matches!(key, KeyInput::Text(_));
    if mods.shift && !is_text {
        s.push_str("Shift+");
    }
    match key {
        KeyInput::Char(' ') => s.push_str("Space"),
        KeyInput::Char(c) if mods.ctrl => s.push(c.to_ascii_uppercase()),
        KeyInput::Char(c) => s.push(*c),
        KeyInput::Text(t) => s.push_str(t),
        KeyInput::F(n) => s.push_str(&format!("F{n}")),
        other => s.push_str(&format!("{other:?}")),
    }
    s
}

/// Human-readable rendering of bytes for the local echo / debug line:
/// printable text as-is, C0 controls in caret notation (`^[`, `^M`).
pub fn caret_notation(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\u{0}'..='\u{1f}' => {
                out.push('^');
                out.push((c as u8 + 0x40) as char);
            }
            '\u{7f}' => out.push_str("^?"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::keyboard::SmolStr;

    fn enc(key: KeyInput, mods: Mods) -> Vec<u8> {
        encode(&key, mods, TermModes::empty()).expect("key encodes")
    }

    fn enc_app(key: KeyInput, mods: Mods) -> Vec<u8> {
        encode(&key, mods, TermModes::APP_CURSOR).expect("key encodes")
    }

    #[test]
    fn arrows_normal_app_cursor_and_modified() {
        assert_eq!(enc(KeyInput::Up, Mods::NONE), b"\x1b[A");
        assert_eq!(enc(KeyInput::Down, Mods::NONE), b"\x1b[B");
        assert_eq!(enc(KeyInput::Right, Mods::NONE), b"\x1b[C");
        assert_eq!(enc(KeyInput::Left, Mods::NONE), b"\x1b[D");
        assert_eq!(enc_app(KeyInput::Up, Mods::NONE), b"\x1bOA");
        assert_eq!(enc_app(KeyInput::Left, Mods::NONE), b"\x1bOD");
        // Modifiers force the CSI 1;m form even in application cursor mode.
        assert_eq!(enc(KeyInput::Right, Mods::CTRL), b"\x1b[1;5C");
        assert_eq!(enc_app(KeyInput::Right, Mods::CTRL), b"\x1b[1;5C");
        assert_eq!(enc(KeyInput::Up, Mods::SHIFT), b"\x1b[1;2A");
        assert_eq!(enc(KeyInput::Left, Mods::ALT), b"\x1b[1;3D");
        assert_eq!(
            enc(
                KeyInput::Down,
                Mods {
                    shift: true,
                    ctrl: true,
                    ..Mods::NONE
                }
            ),
            b"\x1b[1;6B"
        );
        assert_eq!(
            enc(
                KeyInput::Up,
                Mods {
                    shift: true,
                    alt: true,
                    ctrl: true,
                    logo: false
                }
            ),
            b"\x1b[1;8A"
        );
    }

    #[test]
    fn home_end_follow_cursor_key_rules() {
        assert_eq!(enc(KeyInput::Home, Mods::NONE), b"\x1b[H");
        assert_eq!(enc(KeyInput::End, Mods::NONE), b"\x1b[F");
        assert_eq!(enc_app(KeyInput::Home, Mods::NONE), b"\x1bOH");
        assert_eq!(enc_app(KeyInput::End, Mods::NONE), b"\x1bOF");
        assert_eq!(enc(KeyInput::End, Mods::SHIFT), b"\x1b[1;2F");
    }

    #[test]
    fn tilde_keys_plain_and_modified() {
        assert_eq!(enc(KeyInput::Insert, Mods::NONE), b"\x1b[2~");
        assert_eq!(enc(KeyInput::Delete, Mods::NONE), b"\x1b[3~");
        assert_eq!(enc(KeyInput::PageUp, Mods::NONE), b"\x1b[5~");
        assert_eq!(enc(KeyInput::PageDown, Mods::NONE), b"\x1b[6~");
        assert_eq!(enc(KeyInput::Delete, Mods::CTRL), b"\x1b[3;5~");
        assert_eq!(enc(KeyInput::PageUp, Mods::SHIFT), b"\x1b[5;2~");
        // Application cursor mode does not affect tilde keys.
        assert_eq!(enc_app(KeyInput::PageDown, Mods::NONE), b"\x1b[6~");
    }

    #[test]
    fn function_keys_f1_to_f12() {
        let expected: [&[u8]; 12] = [
            b"\x1bOP",
            b"\x1bOQ",
            b"\x1bOR",
            b"\x1bOS",
            b"\x1b[15~",
            b"\x1b[17~",
            b"\x1b[18~",
            b"\x1b[19~",
            b"\x1b[20~",
            b"\x1b[21~",
            b"\x1b[23~",
            b"\x1b[24~",
        ];
        for (i, want) in expected.iter().enumerate() {
            assert_eq!(
                enc(KeyInput::F(i as u8 + 1), Mods::NONE),
                *want,
                "F{}",
                i + 1
            );
        }
        assert_eq!(enc(KeyInput::F(1), Mods::SHIFT), b"\x1b[1;2P");
        assert_eq!(enc(KeyInput::F(4), Mods::CTRL), b"\x1b[1;5S");
        assert_eq!(enc(KeyInput::F(5), Mods::CTRL), b"\x1b[15;5~");
        assert_eq!(enc(KeyInput::F(12), Mods::ALT), b"\x1b[24;3~");
        assert_eq!(
            encode(&KeyInput::F(13), Mods::NONE, TermModes::empty()),
            None
        );
    }

    #[test]
    fn enter_backspace_tab_escape() {
        assert_eq!(enc(KeyInput::Enter, Mods::NONE), b"\r");
        assert_eq!(enc(KeyInput::Enter, Mods::ALT), b"\x1b\r");
        assert_eq!(enc(KeyInput::Backspace, Mods::NONE), b"\x7f");
        assert_eq!(enc(KeyInput::Backspace, Mods::CTRL), b"\x08");
        assert_eq!(enc(KeyInput::Backspace, Mods::ALT), b"\x1b\x7f");
        assert_eq!(enc(KeyInput::Tab, Mods::NONE), b"\t");
        assert_eq!(enc(KeyInput::Tab, Mods::SHIFT), b"\x1b[Z");
        assert_eq!(enc(KeyInput::Escape, Mods::NONE), b"\x1b");
    }

    #[test]
    fn ctrl_letters_and_symbols() {
        assert_eq!(enc(KeyInput::Char('a'), Mods::CTRL), [0x01]);
        assert_eq!(enc(KeyInput::Char('c'), Mods::CTRL), [0x03]);
        assert_eq!(enc(KeyInput::Char('z'), Mods::CTRL), [0x1a]);
        // Ctrl+Shift+C is still ETX.
        assert_eq!(
            enc(
                KeyInput::Char('C'),
                Mods {
                    ctrl: true,
                    shift: true,
                    ..Mods::NONE
                }
            ),
            [0x03]
        );
        assert_eq!(enc(KeyInput::Char(' '), Mods::CTRL), [0x00]);
        assert_eq!(enc(KeyInput::Char('@'), Mods::CTRL), [0x00]);
        assert_eq!(enc(KeyInput::Char('['), Mods::CTRL), [0x1b]);
        assert_eq!(enc(KeyInput::Char('\\'), Mods::CTRL), [0x1c]);
        assert_eq!(enc(KeyInput::Char(']'), Mods::CTRL), [0x1d]);
        assert_eq!(enc(KeyInput::Char('^'), Mods::CTRL), [0x1e]);
        assert_eq!(enc(KeyInput::Char('_'), Mods::CTRL), [0x1f]);
        assert_eq!(enc(KeyInput::Char('?'), Mods::CTRL), [0x7f]);
        // No control mapping: the character itself is sent.
        assert_eq!(enc(KeyInput::Char('.'), Mods::CTRL), b".");
    }

    #[test]
    fn alt_prefixes_escape() {
        assert_eq!(enc(KeyInput::Char('b'), Mods::ALT), b"\x1bb");
        assert_eq!(
            enc(
                KeyInput::Char('B'),
                Mods {
                    alt: true,
                    shift: true,
                    ..Mods::NONE
                }
            ),
            b"\x1bB"
        );
        assert_eq!(
            enc(
                KeyInput::Char('c'),
                Mods {
                    alt: true,
                    ctrl: true,
                    ..Mods::NONE
                }
            ),
            b"\x1b\x03"
        );
        assert_eq!(enc(KeyInput::Char('中'), Mods::ALT), "\x1b中".as_bytes());
    }

    #[test]
    fn plain_text_is_utf8_and_logo_is_swallowed() {
        assert_eq!(enc(KeyInput::Char('x'), Mods::NONE), b"x");
        assert_eq!(enc(KeyInput::Char('X'), Mods::SHIFT), b"X");
        assert_eq!(enc(KeyInput::Char('中'), Mods::NONE), [0xe4, 0xb8, 0xad]);
        assert_eq!(enc(KeyInput::Text("é".into()), Mods::NONE), "é".as_bytes());
        assert_eq!(
            encode(&KeyInput::Char('c'), Mods::LOGO, TermModes::empty()),
            None
        );
        assert_eq!(encode(&KeyInput::Up, Mods::LOGO, TermModes::empty()), None);
        assert_eq!(
            encode(
                &KeyInput::Text(String::new()),
                Mods::NONE,
                TermModes::empty()
            ),
            None
        );
    }

    #[test]
    fn winit_keys_map_to_inputs() {
        assert_eq!(
            key_from_winit(&WinitKey::Named(NamedKey::ArrowUp), None),
            Some(KeyInput::Up)
        );
        assert_eq!(
            key_from_winit(&WinitKey::Named(NamedKey::Space), Some(" ")),
            Some(KeyInput::Char(' '))
        );
        assert_eq!(
            key_from_winit(&WinitKey::Named(NamedKey::F11), None),
            Some(KeyInput::F(11))
        );
        assert_eq!(
            key_from_winit(&WinitKey::Named(NamedKey::Enter), Some("\r")),
            Some(KeyInput::Enter)
        );
        assert_eq!(
            key_from_winit(&WinitKey::Named(NamedKey::Shift), None),
            None
        );
        assert_eq!(
            key_from_winit(&WinitKey::Character(SmolStr::new("a")), Some("a")),
            Some(KeyInput::Char('a'))
        );
        assert_eq!(
            key_from_winit(&WinitKey::Character(SmolStr::new("ab")), Some("ab")),
            Some(KeyInput::Text("ab".into()))
        );
    }

    #[test]
    fn modifier_state_respects_alt_as_meta() {
        let m = ModifiersState::ALT | ModifiersState::CONTROL;
        assert_eq!(
            Mods::from_winit(m, true),
            Mods {
                alt: true,
                ctrl: true,
                ..Mods::NONE
            }
        );
        assert_eq!(
            Mods::from_winit(m, false),
            Mods {
                alt: false,
                ctrl: true,
                ..Mods::NONE
            }
        );
        assert!(Mods::from_winit(ModifiersState::SUPER, true).logo);
    }

    #[test]
    fn caret_notation_renders_controls() {
        assert_eq!(caret_notation(b"\x1b[A"), "^[[A");
        assert_eq!(caret_notation(b"a\r\x7f"), "a^M^?");
        assert_eq!(caret_notation("中".as_bytes()), "中");
    }

    #[test]
    fn describe_chords() {
        assert_eq!(describe(&KeyInput::Char('c'), Mods::CTRL), "Ctrl+C");
        assert_eq!(describe(&KeyInput::Up, Mods::SHIFT), "Shift+Up");
        assert_eq!(describe(&KeyInput::Char('A'), Mods::SHIFT), "A");
        assert_eq!(describe(&KeyInput::Char(' '), Mods::NONE), "Space");
        assert_eq!(describe(&KeyInput::F(5), Mods::ALT), "Alt+F5");
    }

    fn press(key: &WinitKey, text: Option<&'static str>, mods: ModifiersState) -> KeyAction {
        press_with(key, text, mods, COMPOSING)
    }

    fn press_with(
        key: &WinitKey,
        text: Option<&'static str>,
        mods: ModifiersState,
        ime: ImeGate,
    ) -> KeyAction {
        let p = KeyPress {
            logical: key,
            text,
            pressed: true,
            synthetic: false,
            mods,
        };
        decide_key(&p, ime, TermModes::SHOW_CURSOR)
    }

    const COMPOSING: ImeGate = ImeGate {
        enabled: true,
        composing: true,
    };
    const IME_IDLE: ImeGate = ImeGate {
        enabled: true,
        composing: false,
    };

    fn editing_keys() -> Vec<(
        WinitKey,
        Option<&'static str>,
        ModifiersState,
        &'static [u8],
    )> {
        vec![
            (
                WinitKey::Character(SmolStr::new("a")),
                Some("a"),
                ModifiersState::empty(),
                b"a",
            ),
            (
                WinitKey::Named(NamedKey::Enter),
                Some("\r"),
                ModifiersState::empty(),
                b"\r",
            ),
            (
                WinitKey::Named(NamedKey::Backspace),
                None,
                ModifiersState::empty(),
                b"\x7f",
            ),
            (
                WinitKey::Named(NamedKey::ArrowLeft),
                None,
                ModifiersState::empty(),
                b"\x1b[D",
            ),
            (
                WinitKey::Named(NamedKey::Space),
                Some(" "),
                ModifiersState::empty(),
                b" ",
            ),
            (
                WinitKey::Named(NamedKey::Escape),
                None,
                ModifiersState::empty(),
                b"\x1b",
            ),
            (
                WinitKey::Character(SmolStr::new("c")),
                None,
                ModifiersState::CONTROL,
                b"\x03",
            ),
        ]
    }

    #[test]
    fn composing_swallows_letters_enter_backspace_and_arrows() {
        for (key, text, mods, _) in editing_keys() {
            match press(&key, text, mods) {
                KeyAction::SwallowedByIme { .. } => {}
                other => panic!("{key:?} while composing: {other:?}"),
            }
        }
    }

    #[test]
    fn after_composition_the_same_keys_are_forwarded() {
        for (key, text, mods, bytes) in editing_keys() {
            let expected = press_with(&key, text, mods, IME_IDLE);
            match &expected {
                KeyAction::Forward { bytes: got, .. } => {
                    assert_eq!(got.as_slice(), bytes, "{key:?}")
                }
                other => panic!("{key:?} after composition: {other:?}"),
            }
            // An enabled-but-idle IME routes exactly like no IME at all.
            assert_eq!(
                press_with(&key, text, mods, ImeGate::default()),
                expected,
                "{key:?}"
            );
        }
    }

    #[test]
    fn cmd_shortcuts_pass_during_composition() {
        let k = WinitKey::Character(SmolStr::new("k"));
        let q = WinitKey::Character(SmolStr::new("q"));
        for ime in [COMPOSING, IME_IDLE] {
            assert_eq!(
                press_with(&k, Some("k"), ModifiersState::SUPER, ime),
                KeyAction::Shortcut(Shortcut::Palette)
            );
            assert_eq!(
                press_with(&q, Some("q"), ModifiersState::SUPER, ime),
                KeyAction::Shortcut(Shortcut::Quit)
            );
        }
    }

    #[test]
    fn cmd_chords_map_to_gui_shortcuts() {
        let cmd = ModifiersState::SUPER;
        let cmd_shift = ModifiersState::SUPER | ModifiersState::SHIFT;
        let cases: Vec<(&str, ModifiersState, Shortcut)> = vec![
            ("n", cmd, Shortcut::NewSession),
            ("N", cmd_shift, Shortcut::NewWorkspace),
            ("w", cmd, Shortcut::Close),
            ("c", cmd, Shortcut::Copy),
            ("v", cmd, Shortcut::Paste),
            ("1", cmd, Shortcut::Jump(1)),
            ("9", cmd, Shortcut::Jump(9)),
            ("0", cmd, Shortcut::Unbound("⌘0".into())),
            ("W", cmd_shift, Shortcut::Unbound("⌘⇧W".into())),
        ];
        for (key, mods, want) in cases {
            let k = WinitKey::Character(SmolStr::new(key));
            assert_eq!(
                press_with(&k, Some(key), mods, IME_IDLE),
                KeyAction::Shortcut(want),
                "{key} {mods:?}"
            );
        }
    }

    #[test]
    fn shift_page_keys_scroll_locally_outside_the_alternate_screen() {
        let pgup = WinitKey::Named(NamedKey::PageUp);
        let p = KeyPress {
            logical: &pgup,
            text: None,
            pressed: true,
            synthetic: false,
            mods: ModifiersState::SHIFT,
        };
        assert_eq!(
            decide_key(&p, IME_IDLE, TermModes::SHOW_CURSOR),
            KeyAction::Scroll(ScrollKey::PageUp)
        );
        // In full-screen programs the key goes to the program.
        assert!(matches!(
            decide_key(&p, IME_IDLE, TermModes::ALT_SCREEN),
            KeyAction::Forward { .. }
        ));
        // Without Shift PageUp is the program's.
        let plain = KeyPress {
            mods: ModifiersState::empty(),
            ..p
        };
        assert!(matches!(
            decide_key(&plain, IME_IDLE, TermModes::SHOW_CURSOR),
            KeyAction::Forward { .. }
        ));
    }

    #[test]
    fn releases_synthetic_and_modifier_only_keys_are_ignored() {
        let a = WinitKey::Character(SmolStr::new("a"));
        for ime in [COMPOSING, IME_IDLE] {
            let release = KeyPress {
                logical: &a,
                text: None,
                pressed: false,
                synthetic: false,
                mods: ModifiersState::empty(),
            };
            assert_eq!(
                decide_key(&release, ime, TermModes::empty()),
                KeyAction::Ignore
            );
            let synthetic = KeyPress {
                logical: &a,
                text: Some("a"),
                pressed: true,
                synthetic: true,
                mods: ModifiersState::empty(),
            };
            assert_eq!(
                decide_key(&synthetic, ime, TermModes::empty()),
                KeyAction::Ignore
            );
            let shift = WinitKey::Named(NamedKey::Shift);
            assert_eq!(
                press_with(&shift, None, ModifiersState::SHIFT, ime),
                KeyAction::Ignore
            );
        }
    }
}
