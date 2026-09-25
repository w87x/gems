//! A double-buffered screen with line-granularity diffing: each frame
//! builds a fresh `Screen` of fixed-width lines, and rendering compares it
//! against the previous frame, emitting ANSI cursor moves + redraws only
//! for lines whose content actually changed.
//!
//! **Scope cut**: diffing is per whole line, not per cell. A one-character
//! change on a line still repaints that entire line. True cell-level
//! diffing would cut bytes written per frame further, but for a
//! keyboard-driven browser (nothing animates, redraws happen on discrete
//! key presses) line-level diffing already avoids the real cost — a full
//! screen clear-and-redraw on every keystroke — and cells are a
//! micro-optimization this app doesn't need. Reverse-video highlighting
//! (the selected row) is folded directly into a line's stored string via
//! its ANSI SGR codes, so a selection change naturally shows up as a
//! changed line under plain string comparison.

use std::io::{self, Write};

const REVERSE_ON: &str = "\x1b[7m";
const REVERSE_OFF: &str = "\x1b[0m";

pub struct Screen {
    cols: usize,
    lines: Vec<String>,
}

impl Screen {
    pub fn new(rows: usize, cols: usize) -> Self {
        Screen {
            cols,
            lines: vec![String::new(); rows],
        }
    }

    /// Sets row `row` to `text`, truncated or space-padded to the screen's
    /// column width, optionally wrapped in reverse video.
    pub fn set_line(&mut self, row: usize, text: &str, reverse: bool) {
        let Some(slot) = self.lines.get_mut(row) else {
            return;
        };
        let mut truncated: String = text.chars().take(self.cols).collect();
        let pad = self.cols.saturating_sub(truncated.chars().count());
        truncated.push_str(&" ".repeat(pad));
        *slot = if reverse {
            format!("{REVERSE_ON}{truncated}{REVERSE_OFF}")
        } else {
            truncated
        };
    }

    /// Writes the ANSI updates needed to turn `prev` (or a blank screen,
    /// if `prev` is `None`) into `self`, moving the cursor only for lines
    /// that changed.
    pub fn render(&self, prev: Option<&Screen>, out: &mut impl Write) -> io::Result<()> {
        for (i, line) in self.lines.iter().enumerate() {
            let changed = match prev {
                Some(p) => p.lines.get(i) != Some(line),
                None => true,
            };
            if changed {
                write!(out, "\x1b[{};1H\x1b[K{}", i + 1, line)?;
            }
        }
        out.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_line_pads_to_column_width() {
        let mut screen = Screen::new(1, 10);
        screen.set_line(0, "hi", false);
        assert_eq!(screen.lines[0], "hi        ");
    }

    #[test]
    fn set_line_truncates_overlong_text() {
        let mut screen = Screen::new(1, 4);
        screen.set_line(0, "hello world", false);
        assert_eq!(screen.lines[0], "hell");
    }

    #[test]
    fn reverse_wraps_in_sgr_codes() {
        let mut screen = Screen::new(1, 4);
        screen.set_line(0, "ab", true);
        assert_eq!(screen.lines[0], "\x1b[7mab  \x1b[0m");
    }

    #[test]
    fn render_with_no_prev_redraws_every_line() {
        let mut screen = Screen::new(2, 3);
        screen.set_line(0, "a", false);
        screen.set_line(1, "b", false);
        let mut out = Vec::new();
        screen.render(None, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\x1b[1;1H"));
        assert!(text.contains("\x1b[2;1H"));
    }

    #[test]
    fn render_skips_unchanged_lines() {
        let mut prev = Screen::new(2, 3);
        prev.set_line(0, "a", false);
        prev.set_line(1, "b", false);

        let mut next = Screen::new(2, 3);
        next.set_line(0, "a", false);
        next.set_line(1, "c", false);

        let mut out = Vec::new();
        next.render(Some(&prev), &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            !text.contains("\x1b[1;1H"),
            "unchanged row 0 must not be redrawn"
        );
        assert!(text.contains("\x1b[2;1H"), "changed row 1 must be redrawn");
    }
}
