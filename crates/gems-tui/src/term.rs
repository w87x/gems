//! Raw-mode terminal setup via `rustix::termios`, the workspace's one
//! sanctioned dependency. Uses `std::io::stdin()`/`stdout()` as the `AsFd`
//! sources rather than rustix's own `unsafe fn take_stdin()`/`take_stdout()`
//! — those take ownership of the file descriptor, which would fight with
//! std's own stdin/stdout handles the rest of this crate also uses (for
//! `Write`/buffered reads); passing `std::io::Stdin`/`Stdout` directly works
//! because both have implemented `AsFd` since Rust 1.63, which is all
//! rustix's termios functions require.

use std::io;

use rustix::termios::{self, OptionalActions};

/// Puts the terminal into raw mode on construction and restores the
/// original settings on `Drop` — including on an early return or panic
/// unwind, so a crash mid-render doesn't leave the user's shell stuck
/// without local echo.
pub struct RawMode {
    original: termios::Termios,
}

impl RawMode {
    pub fn enable() -> io::Result<Self> {
        let stdin = io::stdin();
        let original = termios::tcgetattr(&stdin)?;
        let mut raw = original.clone();
        raw.make_raw();
        termios::tcsetattr(&stdin, OptionalActions::Now, &raw)?;
        Ok(RawMode { original })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let stdin = io::stdin();
        let _ = termios::tcsetattr(&stdin, OptionalActions::Now, &self.original);
    }
}

/// Current terminal size as `(rows, cols)`, queried on stdout (the
/// conventional fd for `TIOCGWINSZ` — it's the one actually connected to
/// the display in the common case of piped stdin).
pub fn size() -> io::Result<(u16, u16)> {
    let winsize = termios::tcgetwinsize(io::stdout())?;
    Ok((winsize.ws_row, winsize.ws_col))
}
