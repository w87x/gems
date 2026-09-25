//! `gems-tui`: the TUI frontend from ARCHITECTURE.md §9. A read-only
//! entity browser over `gems_engine::Store` — list pane + detail pane,
//! keyboard navigation, and a `/`-triggered query box for running
//! arbitrary `gems-query` SQL-subset queries.
//!
//! Structure mirrors the "pure core + thin I/O shell" pattern used
//! throughout this workspace (`RaftCore`, `SwimCore`, `SubscriptionEngine`):
//! `app::App` is the whole state machine, independently unit-tested with
//! no terminal involved; this file is the shell that reads raw terminal
//! bytes, decodes them via `input::decode`, feeds them to `App`, and
//! renders `App`'s state through the diffing `screen::Screen` buffer.
//!
//! **Scope for this pass**: browse and query only, no ABAC subject
//! context and no entity creation — see `app.rs`'s module doc for why.
//! No mouse support, no scrolling for lists longer than the terminal
//! (the list pane simply truncates past the visible rows) — a real
//! scrolling viewport is a natural follow-up once entity counts warrant it.

mod app;
mod input;
mod screen;
mod term;

use std::io::{Read, Write};
use std::path::PathBuf;

use app::{App, Key, Mode};
use screen::Screen;

fn main() {
    let store_dir = match std::env::args().nth(1) {
        Some(dir) => PathBuf::from(dir),
        None => {
            eprintln!("usage: gems-tui <store_dir>");
            std::process::exit(1);
        }
    };

    let mut app = match App::open(&store_dir) {
        Ok(app) => app,
        Err(e) => {
            eprintln!("failed to open store at {}: {e}", store_dir.display());
            std::process::exit(1);
        }
    };

    let _raw = match term::RawMode::enable() {
        Ok(raw) => raw,
        Err(e) => {
            eprintln!("failed to enable raw mode: {e}");
            std::process::exit(1);
        }
    };

    let mut stdout = std::io::stdout();
    // Enter the alternate screen buffer and hide the cursor, so the
    // browser doesn't clutter the user's scrollback and the cursor
    // doesn't visibly jump around during redraws.
    let _ = write!(stdout, "\x1b[?1049h\x1b[?25l");
    let _ = stdout.flush();

    let result = run(&mut app, &mut stdout);

    let _ = write!(stdout, "\x1b[?25h\x1b[?1049l");
    let _ = stdout.flush();

    if let Err(e) = result {
        eprintln!("gems-tui error: {e}");
        std::process::exit(1);
    }
}

fn run(app: &mut App, stdout: &mut std::io::Stdout) -> std::io::Result<()> {
    let mut prev_screen: Option<Screen> = None;
    let mut stdin = std::io::stdin();
    let mut buf = [0u8; 32];

    loop {
        // A pty that never had `TIOCSWINSZ` applied (e.g. a bare test
        // harness) reports a successful `(0, 0)` rather than an error, so
        // guard against zero dimensions in addition to the `Err` case.
        let (rows, cols) = match term::size() {
            Ok((r, c)) if r > 0 && c > 0 => (r, c),
            _ => (24, 80),
        };
        let screen = render_screen(app, rows as usize, cols as usize);
        screen.render(prev_screen.as_ref(), stdout)?;
        prev_screen = Some(screen);

        let n = stdin.read(&mut buf)?;
        if n == 0 {
            continue;
        }
        let mut offset = 0;
        while offset < n {
            let Some((key, consumed)) = input::decode(&buf[offset..n], app.mode) else {
                break;
            };
            if matches!(key, Key::Quit) {
                return Ok(());
            }
            app.handle_key(key);
            offset += consumed;
        }
    }
}

/// Builds one frame from `app`'s current state: a header line, a
/// list/detail split below it, and (in query-input mode) a bottom line
/// showing the in-progress query text.
fn render_screen(app: &App, rows: usize, cols: usize) -> Screen {
    let mut screen = Screen::new(rows, cols);
    if rows == 0 {
        return screen;
    }

    screen.set_line(
        0,
        &format!(
            "gems-tui | {} | q:quit  j/k:move  /:query",
            app.store_dir.display()
        ),
        true,
    );

    let body_rows = rows.saturating_sub(2); // header + status/query line
    let list_width = cols / 3;
    let list_rows = body_rows.min(app.entities.len().max(body_rows));

    for row in 0..body_rows {
        let list_text = app
            .entities
            .get(row)
            .map(|e| format!("{} [{}]", e.name, e.kind))
            .unwrap_or_default();
        let selected = row == app.selected && row < app.entities.len();
        screen.set_line(row + 1, &pad_or_truncate(&list_text, list_width), selected);
    }

    if let Some(detail) = &app.detail {
        let mut detail_row = 0usize;
        let mut push = |screen: &mut Screen, text: String| {
            if detail_row < body_rows {
                let full_line = format!("{}| {}", " ".repeat(list_width), text);
                screen.set_line(detail_row + 1, &full_line, false);
                detail_row += 1;
            }
        };
        for (key, value) in &detail.header_lines {
            push(&mut screen, format!("{key}: {value}"));
        }
        if !detail.fields.is_empty() {
            push(&mut screen, String::new());
            for (key, value) in &detail.fields {
                push(&mut screen, format!("{key} = {value}"));
            }
        }
    }
    let _ = list_rows;

    let status_row = rows - 1;
    match app.mode {
        Mode::Browse => {
            let text = app
                .error
                .as_deref()
                .map(|e| format!("error: {e}"))
                .unwrap_or_else(|| format!("query: {}", app.last_query));
            screen.set_line(status_row, &text, false);
        }
        Mode::QueryInput => {
            screen.set_line(status_row, &format!("/{}", app.query_input), false);
        }
    }

    screen
}

fn pad_or_truncate(text: &str, width: usize) -> String {
    let mut s: String = text.chars().take(width).collect();
    let pad = width.saturating_sub(s.chars().count());
    s.push_str(&" ".repeat(pad));
    s
}
