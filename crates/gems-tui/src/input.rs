//! Decodes raw terminal bytes into `app::Key` values, including the
//! multi-byte ANSI escape sequences arrow keys send (`ESC [ A/B/C/D`).
//!
//! **Scope cut**: a bare ESC byte not followed promptly by `[` and a
//! letter is treated as `Key::Escape` immediately rather than waiting on a
//! timer to disambiguate it from the start of a slow-arriving escape
//! sequence (the standard terminal-handling ambiguity between "user
//! pressed Esc" and "an escape sequence is still arriving"). This crate
//! reads from a blocking stdin already carrying whatever bytes the
//! terminal driver delivered in one `read()`, so a genuine arrow-key
//! sequence's `[` and letter are already in the buffer by the time this
//! function runs — a real disambiguation timer is only needed for
//! character-at-a-time raw links (e.g. a slow serial console), which is
//! out of scope here.

use crate::app::{Key, Mode};

/// Decodes one input event from `bytes`, returning the event and how many
/// bytes it consumed. `mode` decides whether single letters like `j`/`k`/
/// `q`/`/` are navigation shortcuts (`Mode::Browse`) or literal characters
/// to type into the query box (`Mode::QueryInput`) — without this, typing
/// a query containing the letter "j" would move the selection instead.
pub fn decode(bytes: &[u8], mode: Mode) -> Option<(Key, usize)> {
    let &first = bytes.first()?;
    match first {
        0x1b => {
            if bytes.get(1) == Some(&b'[') {
                match bytes.get(2) {
                    Some(b'A') => Some((Key::Up, 3)),
                    Some(b'B') => Some((Key::Down, 3)),
                    _ => Some((Key::Escape, 1)),
                }
            } else {
                Some((Key::Escape, 1))
            }
        }
        b'\r' | b'\n' => Some((Key::Enter, 1)),
        0x7f | 0x08 => Some((Key::Backspace, 1)),
        0x03 => Some((Key::Quit, 1)), // Ctrl+C, always quits
        b'q' if mode == Mode::Browse => Some((Key::Quit, 1)),
        b'k' if mode == Mode::Browse => Some((Key::Up, 1)),
        b'j' if mode == Mode::Browse => Some((Key::Down, 1)),
        b'/' if mode == Mode::Browse => Some((Key::EnterQueryMode, 1)),
        b => {
            let ch = b as char;
            if ch.is_ascii() {
                Some((Key::Char(ch), 1))
            } else {
                Some((Key::Char('?'), 1))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_plain_characters() {
        assert_eq!(decode(b"x", Mode::Browse), Some((Key::Char('x'), 1)));
    }

    #[test]
    fn decodes_arrow_keys() {
        assert_eq!(decode(b"\x1b[A", Mode::Browse), Some((Key::Up, 3)));
        assert_eq!(decode(b"\x1b[B", Mode::Browse), Some((Key::Down, 3)));
    }

    #[test]
    fn bare_escape_is_escape_key() {
        assert_eq!(decode(b"\x1b", Mode::Browse), Some((Key::Escape, 1)));
    }

    #[test]
    fn decodes_enter_backspace_and_quit() {
        assert_eq!(decode(b"\r", Mode::Browse), Some((Key::Enter, 1)));
        assert_eq!(decode(&[0x7f], Mode::Browse), Some((Key::Backspace, 1)));
        assert_eq!(decode(b"q", Mode::Browse), Some((Key::Quit, 1)));
        assert_eq!(decode(&[0x03], Mode::Browse), Some((Key::Quit, 1)));
    }

    #[test]
    fn vim_style_navigation_and_query_shortcut_in_browse_mode() {
        assert_eq!(decode(b"j", Mode::Browse), Some((Key::Down, 1)));
        assert_eq!(decode(b"k", Mode::Browse), Some((Key::Up, 1)));
        assert_eq!(decode(b"/", Mode::Browse), Some((Key::EnterQueryMode, 1)));
    }

    #[test]
    fn navigation_letters_are_literal_characters_in_query_input_mode() {
        assert_eq!(decode(b"j", Mode::QueryInput), Some((Key::Char('j'), 1)));
        assert_eq!(decode(b"k", Mode::QueryInput), Some((Key::Char('k'), 1)));
        assert_eq!(decode(b"q", Mode::QueryInput), Some((Key::Char('q'), 1)));
        assert_eq!(decode(b"/", Mode::QueryInput), Some((Key::Char('/'), 1)));
        // Ctrl+C still quits even mid-query.
        assert_eq!(decode(&[0x03], Mode::QueryInput), Some((Key::Quit, 1)));
    }

    #[test]
    fn empty_input_decodes_to_none() {
        assert_eq!(decode(b"", Mode::Browse), None);
    }
}
