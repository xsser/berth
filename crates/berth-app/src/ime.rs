//! IME composition state (DESIGN §8.2).
//!
//! Semantics follow winit 0.30.13 on macOS (`platform_impl/macos/view.rs`):
//! - `Ime::Enabled` arrives the first time marked text appears.
//! - `Ime::Preedit(text, Some((start, end)))` carries the composition and a
//!   byte range for the IME caret; empty `text` means the preedit was cleared.
//! - `Ime::Commit(text)` is preceded by an empty `Preedit`. winit documents
//!   that a key consumed by the IME does not also produce
//!   `WindowEvent::KeyboardInput`; because that is unverified with real IMEs
//!   here, `input::decide_key` also swallows every non-⌘ key while a preedit
//!   is showing, so composition keys never reach the PTY.
//! - `Ime::Disabled` when the input source changes; pending preedit is dropped.
//!
//! Only `Commit` produces bytes for the PTY; preedit is drawn at the cursor
//! (underlined) by the grid renderer and never sent.
//!
//! Set `BERTH_IME_DEBUG=1` to print every IME event to stderr for manual
//! verification with a real input method.

use winit::event::Ime;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Preedit {
    pub text: String,
    /// IME caret / selected clause as a byte range into `text`, clamped to
    /// char boundaries. `None` means the IME asked to hide the caret.
    pub cursor: Option<(usize, usize)>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ImeOutcome {
    /// Nothing visible changed.
    Nothing,
    /// The preedit changed; redraw.
    PreeditChanged,
    /// Text to deliver to the terminal.
    Commit(String),
}

#[derive(Debug, Default)]
pub struct ImeState {
    enabled: bool,
    preedit: Preedit,
    debug: bool,
}

impl ImeState {
    pub fn new(debug: bool) -> Self {
        Self {
            debug,
            ..Self::default()
        }
    }

    /// Debug printing is enabled by `BERTH_IME_DEBUG=1`.
    pub fn from_env() -> Self {
        Self::new(std::env::var("BERTH_IME_DEBUG").is_ok_and(|v| v == "1"))
    }

    pub fn debug(&self) -> bool {
        self.debug
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Current composition, if any.
    pub fn preedit(&self) -> Option<&Preedit> {
        (!self.preedit.text.is_empty()).then_some(&self.preedit)
    }

    pub fn handle(&mut self, event: &Ime) -> ImeOutcome {
        if self.debug {
            eprintln!("[ime] event {event:?}");
        }
        let outcome = match event {
            Ime::Enabled => {
                self.enabled = true;
                ImeOutcome::Nothing
            }
            Ime::Preedit(text, range) => {
                let cursor = range.map(|(start, end)| {
                    (
                        floor_boundary(text, start),
                        floor_boundary(text, end.max(start)),
                    )
                });
                let next = Preedit {
                    text: text.clone(),
                    cursor,
                };
                if next == self.preedit {
                    ImeOutcome::Nothing
                } else {
                    self.preedit = next;
                    ImeOutcome::PreeditChanged
                }
            }
            Ime::Commit(text) => {
                let had_preedit = !self.preedit.text.is_empty();
                self.preedit = Preedit::default();
                if !text.is_empty() {
                    ImeOutcome::Commit(text.clone())
                } else if had_preedit {
                    ImeOutcome::PreeditChanged
                } else {
                    ImeOutcome::Nothing
                }
            }
            Ime::Disabled => {
                self.enabled = false;
                let had_preedit = !self.preedit.text.is_empty();
                self.preedit = Preedit::default();
                if had_preedit {
                    ImeOutcome::PreeditChanged
                } else {
                    ImeOutcome::Nothing
                }
            }
        };
        if self.debug {
            match &outcome {
                ImeOutcome::Commit(text) => {
                    eprintln!("[ime] commit {text:?} -> {:02x?}", text.as_bytes())
                }
                ImeOutcome::PreeditChanged => {
                    eprintln!(
                        "[ime] preedit {:?} cursor {:?}",
                        self.preedit.text, self.preedit.cursor
                    )
                }
                ImeOutcome::Nothing => {}
            }
        }
        outcome
    }
}

/// Largest char boundary `<= idx` (IMEs occasionally report offsets past the
/// end or inside a multi-byte character).
fn floor_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preedit(text: &str, cursor: Option<(usize, usize)>) -> Ime {
        Ime::Preedit(text.to_string(), cursor)
    }

    #[test]
    fn pinyin_composition_then_commit() {
        let mut ime = ImeState::new(false);
        assert_eq!(ime.handle(&Ime::Enabled), ImeOutcome::Nothing);
        assert!(ime.enabled());
        assert_eq!(
            ime.handle(&preedit("n", Some((1, 1)))),
            ImeOutcome::PreeditChanged
        );
        assert_eq!(
            ime.handle(&preedit("ni", Some((2, 2)))),
            ImeOutcome::PreeditChanged
        );
        assert_eq!(
            ime.handle(&preedit("ni hao", Some((6, 6)))),
            ImeOutcome::PreeditChanged
        );
        assert_eq!(ime.preedit().unwrap().text, "ni hao");
        // winit sends an empty preedit right before the commit.
        assert_eq!(ime.handle(&preedit("", None)), ImeOutcome::PreeditChanged);
        assert_eq!(
            ime.handle(&Ime::Commit("你好".into())),
            ImeOutcome::Commit("你好".into())
        );
        assert!(ime.preedit().is_none());
    }

    #[test]
    fn commit_clears_pending_preedit() {
        let mut ime = ImeState::new(false);
        ime.handle(&preedit("zhong", Some((5, 5))));
        assert_eq!(
            ime.handle(&Ime::Commit("中".into())),
            ImeOutcome::Commit("中".into())
        );
        assert!(ime.preedit().is_none());
        // An empty commit with nothing pending changes nothing.
        assert_eq!(ime.handle(&Ime::Commit(String::new())), ImeOutcome::Nothing);
    }

    #[test]
    fn duplicate_preedit_is_not_a_change() {
        let mut ime = ImeState::new(false);
        assert_eq!(
            ime.handle(&preedit("a", Some((1, 1)))),
            ImeOutcome::PreeditChanged
        );
        assert_eq!(ime.handle(&preedit("a", Some((1, 1)))), ImeOutcome::Nothing);
        assert_eq!(ime.handle(&preedit("a", None)), ImeOutcome::PreeditChanged);
    }

    #[test]
    fn cursor_is_clamped_to_char_boundaries() {
        let mut ime = ImeState::new(false);
        // "中" is 3 bytes; offsets 1 and 99 are invalid.
        ime.handle(&preedit("中文", Some((1, 99))));
        assert_eq!(ime.preedit().unwrap().cursor, Some((0, 6)));
        ime.handle(&preedit("中文", Some((5, 2))));
        assert_eq!(ime.preedit().unwrap().cursor, Some((3, 3)));
    }

    #[test]
    fn disabled_drops_preedit() {
        let mut ime = ImeState::new(false);
        ime.handle(&Ime::Enabled);
        ime.handle(&preedit("ka", Some((2, 2))));
        assert_eq!(ime.handle(&Ime::Disabled), ImeOutcome::PreeditChanged);
        assert!(!ime.enabled());
        assert!(ime.preedit().is_none());
        assert_eq!(ime.handle(&Ime::Disabled), ImeOutcome::Nothing);
    }
}
