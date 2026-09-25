//! Standard MCP stdio transport: read newline-delimited JSON-RPC requests
//! from stdin, write newline-delimited responses to stdout, flushing after
//! each (an MCP client reads responses as they arrive, not in a batch).
//! All protocol/tool logic lives in the library (`gems_mcp::dispatch`) —
//! this is deliberately as thin as `gems-cli`'s `main` is over its own
//! command functions.

use std::io::{self, BufRead, Write};

fn main() {
    let stdin = io::stdin();
    let mut stdout = io::stdout();

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = gems_mcp::dispatch(&line) {
            let _ = writeln!(stdout, "{response}");
            let _ = stdout.flush();
        }
    }
}
