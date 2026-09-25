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

mod api;
mod http;

use std::net::{TcpListener, TcpStream};
use std::thread;

use http::{parse_request, write_response, Request};

const INDEX_HTML: &str = include_str!("../assets/index.html");

fn main() {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:8080".to_string());
    let listener = TcpListener::bind(&addr).unwrap_or_else(|e| {
        eprintln!("failed to bind {addr}: {e}");
        std::process::exit(1);
    });
    println!("gems-webui listening on http://{addr}");

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        thread::spawn(move || handle_connection(stream));
    }
}

fn handle_connection(mut stream: TcpStream) {
    let request = match parse_request(&stream) {
        Ok(r) => r,
        Err(_) => {
            write_response(&mut stream, 400, "text/plain", b"bad request");
            return;
        }
    };
    route(&mut stream, &request);
}

fn route(stream: &mut TcpStream, request: &Request) {
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
        "/api/types" => respond_json(stream, api::list_types(request)),
        "/api/query" => respond_json(stream, api::query(request)),
        "/api/entity" => respond_json(stream, api::get_entity(request)),
        _ => write_response(stream, 404, "text/plain", b"not found"),
    }
}

fn respond_json(stream: &mut TcpStream, result: api::ApiResult) {
    let body = api::to_response_body(result);
    write_response(stream, 200, "application/json", body.as_bytes());
}
