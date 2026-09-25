//! Standard MCP stdio transport: read newline-delimited JSON-RPC requests
//! from stdin, write newline-delimited responses to stdout, flushing after
//! each (an MCP client reads responses as they arrive, not in a batch).
//! All protocol/tool logic lives in the library (`gems_mcp::dispatch`) —
//! this is deliberately as thin as `gems-cli`'s `main` is over its own
//! command functions.
//!
//! **Authentication is required by default** (see `tools.rs`'s module
//! doc): refuses to start unless `$GEMS_MCP_SECRET` is set (the HMAC
//! secret used to verify each call's `auth_token` argument), or `--insecure`
//! is passed on the command line — which restores raw, unauthenticated
//! access and prints a loud warning, for local testing only.
//!
//! **Shutdown**: unlike `gems-webui`, this process needs no special
//! `SIGTERM`/`SIGINT` handling to shut down gracefully — it holds no
//! resource across calls (each tool call opens its `Store` read-only, per
//! `tools.rs`, so it never takes the store's directory lock), and EOF on
//! stdin (its MCP client disconnecting) already ends the main loop
//! cleanly. Default signal termination is already safe here.
//!
//! **Logging**: diagnostic messages go through `gems_common::logging`
//! (leveled, controlled by `$GEMS_LOG`) — this binary's stdout is the
//! MCP JSON-RPC protocol itself and must never carry anything else, so
//! all diagnostics go to stderr via the logger, never `println!`.

use std::io::{self, BufRead, Read, Write};

use gems_abac::token::AuthMode;
use gems_common::{log_error, log_warn};

/// `BufRead::read_line`/`lines()` has no length limit: a client that never
/// sends a newline makes it buffer unboundedly. stdio is normally a
/// trusted local parent process, but that's not a reason to trust it to
/// never send a malformed stream — cap it the same way the network-facing
/// frontends (gems-webui, gems-cluster) cap their own length-prefixed and
/// line-based reads.
const MAX_LINE_LEN: u64 = 16 * 1024 * 1024;
const SECRET_ENV_VAR: &str = "GEMS_MCP_SECRET";
const LOG_TARGET: &str = "gems-mcp";

fn main() {
    let insecure = std::env::args().any(|a| a == "--insecure");
    let auth = if insecure {
        log_warn!(
            LOG_TARGET,
            "running with --insecure — every tool call gets raw, unauthenticated, unenforced \
             access. Do not use this outside local testing."
        );
        AuthMode::Insecure
    } else {
        match std::env::var(SECRET_ENV_VAR) {
            Ok(secret) if !secret.is_empty() => AuthMode::Enforced {
                secret: secret.into_bytes(),
            },
            _ => {
                log_error!(
                    LOG_TARGET,
                    "refusing to start without ${SECRET_ENV_VAR} set (the HMAC secret used to \
                     verify each call's auth_token argument). Set it, or pass --insecure to \
                     explicitly run without authentication (local testing only)."
                );
                std::process::exit(1);
            }
        }
    };

    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let mut stdout = io::stdout();

    loop {
        let mut buf = Vec::new();
        let mut limited = (&mut reader).take(MAX_LINE_LEN);
        let n = match limited.read_until(b'\n', &mut buf) {
            Ok(n) => n,
            Err(_) => break,
        };
        if n == 0 {
            break; // EOF
        }
        if buf.len() as u64 >= MAX_LINE_LEN && !buf.ends_with(b"\n") {
            // Can't safely skip just this line and keep going: the rest of
            // it (past the cap) is still sitting unread on the stream, and
            // resuming line-sync without buffering it fully would misread
            // the remainder as the start of the next request. Terminate
            // instead of continuing in a desynced state.
            log_error!(
                LOG_TARGET,
                "request line exceeds the maximum allowed length, closing stdio"
            );
            break;
        }
        let Ok(line) = String::from_utf8(buf) else {
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = gems_mcp::dispatch(&line, &auth) {
            let _ = writeln!(stdout, "{response}");
            let _ = stdout.flush();
        }
    }
}
