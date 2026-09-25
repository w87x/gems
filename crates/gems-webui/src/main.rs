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
//! **Reads** (`/api/query`, `/api/types`, `/api/entity`, and the
//! admin-only listing routes) are `GET` with query-string parameters and
//! go straight to a read-only `Store::open` (see `api.rs`'s module doc).
//! **Writes** (`/api/entities/save`, `/api/types/save`,
//! `/api/subjects/save`, `/api/policies/save`, their `delete` and
//! `issue-token` counterparts, and `/api/setup`) are `POST` with a JSON
//! body and go through `gems-cluster`'s Raft `propose` path instead — see
//! `write_api.rs`'s module doc for why this server never opens a store
//! writable itself. Configuring `$GEMS_SHARD_MAP`/`$GEMS_CLUSTER_SECRET`
//! is what turns the write routes on; without them this server still runs,
//! read-only, exactly as it always has.
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
mod write_api;

use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use gems_abac::token::AuthMode;
use gems_cluster::shard::ShardedClient;
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

    let cluster = match write_api::build_sharded_client_from_env() {
        Ok(client) => {
            log_info!(
                LOG_TARGET,
                "cluster write path configured — write routes are enabled"
            );
            Some(Arc::new(client))
        }
        Err(e) => {
            log_warn!(
                LOG_TARGET,
                "cluster write path not configured ({e}) — running read-only. Set \
                 $GEMS_SHARD_MAP/$GEMS_CLUSTER_SECRET to enable entity/type/subject/policy \
                 writes and /api/setup."
            );
            None
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
        let cluster = cluster.clone();
        thread::spawn(move || handle_connection(stream, &auth, cluster.as_deref()));
    }
}

fn handle_connection(mut stream: TcpStream, auth: &AuthMode, cluster: Option<&ShardedClient>) {
    let request = match parse_request(&stream) {
        Ok(r) => r,
        Err(_) => {
            write_response(&mut stream, 400, "text/plain", b"bad request");
            return;
        }
    };
    route(&mut stream, &request, auth, cluster);
}

fn route(
    stream: &mut TcpStream,
    request: &Request,
    auth: &AuthMode,
    cluster: Option<&ShardedClient>,
) {
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/" | "/index.html") => write_response(
            stream,
            200,
            "text/html; charset=utf-8",
            INDEX_HTML.as_bytes(),
        ),
        ("GET", "/api/types") => respond_json(stream, api::list_types(request, auth)),
        ("GET", "/api/query") => respond_json(stream, api::query(request, auth)),
        ("GET", "/api/entity") => respond_json(stream, api::get_entity(request, auth)),
        ("GET", "/api/subjects") => respond_json(stream, write_api::list_subjects(request, auth)),
        ("GET", "/api/roles") => respond_json(stream, write_api::list_roles(request, auth)),
        ("GET", "/api/policies") => respond_json(stream, write_api::list_policies(request, auth)),
        ("POST", "/api/setup") => match cluster {
            Some(client) => respond_json(stream, write_api::setup(request, auth, client)),
            None => respond_json(stream, Err(write_not_configured())),
        },
        ("POST", path) => match cluster {
            Some(client) => match write_route(path, request, auth, client) {
                Some(result) => respond_json(stream, result),
                None => write_response(stream, 404, "text/plain", b"not found"),
            },
            None => respond_json(stream, Err(write_not_configured())),
        },
        _ => write_response(stream, 404, "text/plain", b"not found"),
    }
}

fn write_not_configured() -> (u16, String) {
    (
        503,
        "write routes are disabled: $GEMS_SHARD_MAP/$GEMS_CLUSTER_SECRET are not configured"
            .to_string(),
    )
}

/// The `POST` routes that need the cluster write path, dispatched
/// separately from `route`'s main `match` so a 404 on an unrecognized
/// `POST` path is distinguishable from "writes aren't configured" (the
/// caller of this function already knows a client is available).
fn write_route(
    path: &str,
    request: &Request,
    auth: &AuthMode,
    client: &ShardedClient,
) -> Option<api::ApiResult> {
    Some(match path {
        "/api/entities/save" => write_api::save_entity(request, auth, client),
        "/api/entities/delete" => write_api::delete_entity(request, auth, client),
        "/api/types/save" => write_api::save_type(request, auth, client),
        "/api/types/delete" => write_api::delete_type(request, auth, client),
        "/api/subjects/save" => write_api::save_subject(request, auth, client),
        "/api/subjects/issue-token" => write_api::issue_token(request, auth),
        "/api/policies/save" => write_api::save_policy(request, auth, client),
        "/api/policies/delete" => write_api::delete_policy(request, auth, client),
        _ => return None,
    })
}

fn respond_json(stream: &mut TcpStream, result: api::ApiResult) {
    let status = match &result {
        Err((401, _)) => 401,
        _ => 200,
    };
    let body = api::to_response_body(result);
    write_response(stream, status, "application/json", body.as_bytes());
}
