//! `gems-webui`: the WebUI frontend from ARCHITECTURE.md §9. That section
//! names Svelte for the frontend; this instead hand-writes vanilla HTML/
//! CSS/JS with no build step, consistent with this workspace's broader
//! choice throughout (the CLI's own arg parser, the TUI's own terminal
//! handling, this crate's own HTTP server) to hand-roll rather than take
//! on a dependency graph — here that graph would be a whole JS build
//! toolchain (npm, a bundler, Svelte itself) for a page this size (a
//! two-panel query tool), where the difference from hand-written JS is
//! developer ergonomics at a scale this UI doesn't reach yet. Revisit if
//! the WebUI grows enough real interactivity that hand-written DOM
//! manipulation stops being the more legible choice.
//!
//! **Scope for this pass: read-only.** `/api/query`, `/api/types`,
//! `/api/entity` are all `GET`. Writing entities goes through `gems-cli`
//! or `gems-mcp` for now — a real create/edit UI needs form generation
//! from `EntityType`/`EntityAttribute` schema, which is its own
//! integration project, not a small addition to a browsing tool.
//!
//! Server: a small hand-rolled HTTP/1.1 server (`http.rs`), blocking I/O,
//! one thread per connection — ARCHITECTURE.md §9's own words for this
//! exact case: "admin-tool traffic levels don't need an async runtime."
//!
//! **Authentication is required by default** (see `api.rs`'s module doc):
//! every request must carry a valid `Authorization: Bearer <token>` header,
//! a token issued via `gems-abac::token::issue` with a secret matching
//! this server's `GEMS_WEBUI_SECRET` environment variable. Refuses to
//! start without that variable set, unless `--insecure` is passed —
//! which restores raw, unauthenticated access and prints a loud warning,
//! for local testing only. There is no default secret: a server that
//! silently fell back to one would give every deployment the same
//! effective password.
//!
//! **Graceful shutdown**: `SIGTERM`/`SIGINT` (via `gems_common::shutdown`)
//! stop the accept loop from taking new connections — already-accepted
//! requests still get a response (each runs on its own detached thread,
//! independent of the accept loop) — rather than the default "die
//! mid-response" behavior an orchestrator's `SIGTERM` would otherwise
//! cause. The listener is polled non-blocking rather than true
//! signal-interrupted blocking `accept()`, so shutdown is prompt (within
//! one poll interval) rather than instantaneous — a deliberate, documented
//! simplicity/latency tradeoff, not a correctness concern (no lock or
//! other resource is held across requests for a delayed shutdown to put
//! at risk — see `gems-engine::Store`'s concurrency contract: this
//! server's `Store::open` calls are always read-only, so they never take
//! the store's directory lock in the first place).
//!
//! **Logging**: diagnostic/operational messages go through
//! `gems_common::logging` (leveled, controlled by `$GEMS_LOG`) rather than
//! ad hoc `println!`/`eprintln!` — this binary has no other output that
//! needs to stay a stable, parseable protocol the way `gems-cli`'s stdout
//! does, so all of it can be leveled diagnostic output.

mod api;
mod http;

use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

use gems_abac::token::AuthMode;
use gems_common::{log_error, log_info, log_warn};
use http::{parse_request, write_response, Request};

const INDEX_HTML: &str = include_str!("../assets/index.html");
const SECRET_ENV_VAR: &str = "GEMS_WEBUI_SECRET";
const LOG_TARGET: &str = "gems-webui";

/// How often the accept loop wakes up to check for a shutdown signal when
/// no connection is pending. Small enough that `SIGTERM` feels prompt,
/// large enough not to busy-loop.
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(100);

fn main() {
    gems_common::shutdown::install_handler();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let insecure = args.iter().any(|a| a == "--insecure");
    let addr = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .cloned()
        .unwrap_or_else(|| "127.0.0.1:8080".to_string());

    let auth = if insecure {
        log_warn!(
            LOG_TARGET,
            "running with --insecure — every request gets raw, unauthenticated, unenforced \
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
                     verify Authorization: Bearer tokens). Set it, or pass --insecure to \
                     explicitly run without authentication (local testing only)."
                );
                std::process::exit(1);
            }
        }
    };

    let listener = TcpListener::bind(&addr).unwrap_or_else(|e| {
        log_error!(LOG_TARGET, "failed to bind {addr}: {e}");
        std::process::exit(1);
    });
    listener
        .set_nonblocking(true)
        .expect("failed to set the listener non-blocking");
    log_info!(LOG_TARGET, "listening on http://{addr}");

    for stream in listener.incoming() {
        if gems_common::shutdown::shutdown_requested() {
            log_info!(
                LOG_TARGET,
                "shutdown signal received, no longer accepting new connections"
            );
            break;
        }
        let stream = match stream {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(ACCEPT_POLL_INTERVAL);
                continue;
            }
            Err(_) => continue,
        };
        // An accepted connection's non-blocking status isn't guaranteed to
        // be independent of the listener's across all platforms — set it
        // explicitly rather than relying on that, since the rest of this
        // connection's handling (parse_request's read timeout, blocking
        // writes) assumes ordinary blocking I/O.
        if stream.set_nonblocking(false).is_err() {
            continue;
        }
        let auth = auth.clone();
        thread::spawn(move || handle_connection(stream, &auth));
    }
}

fn handle_connection(mut stream: TcpStream, auth: &AuthMode) {
    let request = match parse_request(&stream) {
        Ok(r) => r,
        Err(_) => {
            write_response(&mut stream, 400, "text/plain", b"bad request");
            return;
        }
    };
    route(&mut stream, &request, auth);
}

fn route(stream: &mut TcpStream, request: &Request, auth: &AuthMode) {
    if request.method != "GET" {
        write_response(stream, 400, "text/plain", b"only GET is supported");
        return;
    }
    match request.path.as_str() {
        "/" | "/index.html" => write_response(
            stream,
            200,
            "text/html; charset=utf-8",
            INDEX_HTML.as_bytes(),
        ),
        "/api/types" => respond_json(stream, api::list_types(request, auth)),
        "/api/query" => respond_json(stream, api::query(request, auth)),
        "/api/entity" => respond_json(stream, api::get_entity(request, auth)),
        _ => write_response(stream, 404, "text/plain", b"not found"),
    }
}

fn respond_json(stream: &mut TcpStream, result: api::ApiResult) {
    let status = match &result {
        Err((401, _)) => 401,
        _ => 200,
    };
    let body = api::to_response_body(result);
    write_response(stream, status, "application/json", body.as_bytes());
}
