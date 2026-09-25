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

mod api;
mod http;

use std::net::{TcpListener, TcpStream};
use std::thread;

use gems_abac::token::AuthMode;
use http::{parse_request, write_response, Request};

const INDEX_HTML: &str = include_str!("../assets/index.html");
const SECRET_ENV_VAR: &str = "GEMS_WEBUI_SECRET";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let insecure = args.iter().any(|a| a == "--insecure");
    let addr = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .cloned()
        .unwrap_or_else(|| "127.0.0.1:8080".to_string());

    let auth = if insecure {
        eprintln!(
            "gems-webui: running with --insecure — every request gets raw, unauthenticated, \
             unenforced access. Do not use this outside local testing."
        );
        AuthMode::Insecure
    } else {
        match std::env::var(SECRET_ENV_VAR) {
            Ok(secret) if !secret.is_empty() => AuthMode::Enforced {
                secret: secret.into_bytes(),
            },
            _ => {
                eprintln!(
                    "gems-webui: refusing to start without ${SECRET_ENV_VAR} set (the HMAC \
                     secret used to verify Authorization: Bearer tokens). Set it, or pass \
                     --insecure to explicitly run without authentication (local testing only)."
                );
                std::process::exit(1);
            }
        }
    };

    let listener = TcpListener::bind(&addr).unwrap_or_else(|e| {
        eprintln!("failed to bind {addr}: {e}");
        std::process::exit(1);
    });
    println!("gems-webui listening on http://{addr}");

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
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
