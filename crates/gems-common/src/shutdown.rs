//! A minimal SIGTERM/SIGINT handler for graceful shutdown of long-running
//! services (`gems-webui`; `gems-mcp`'s stdin-driven loop already exits
//! cleanly on EOF and holds no long-lived lock between calls, so it has
//! nothing extra to drain). Sets a flag a service's accept/poll loop
//! checks periodically, rather than trying to do real work inside the
//! signal handler itself (POSIX async-signal-safety rules restrict what a
//! handler may safely call; a single atomic store is one of the operations
//! those rules do allow).
//!
//! Registered via a raw `extern "C"` binding to libc's `signal()` rather
//! than a crate like `signal-hook` — every Rust binary on Unix already
//! links libc for its C runtime, so this needs no new dependency, just
//! this one FFI declaration, consistent with this workspace's "avoid
//! third-party crates" rule.

use std::sync::atomic::{AtomicBool, Ordering};

const SIGINT: i32 = 2;
const SIGTERM: i32 = 15;

extern "C" {
    fn signal(signum: i32, handler: usize) -> usize;
}

static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_shutdown_signal(_signum: i32) {
    // A plain atomic store is the one thing this handler does — safe to
    // call from a signal handler, unlike almost everything else (no
    // allocation, no locking, no I/O).
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
}

/// Installs handlers for `SIGTERM` and `SIGINT` that set a flag
/// `shutdown_requested()` reports, instead of the default behavior
/// (immediate termination). Call once, near the start of `main`.
pub fn install_handler() {
    unsafe {
        signal(SIGTERM, handle_shutdown_signal as *const () as usize);
        signal(SIGINT, handle_shutdown_signal as *const () as usize);
    }
}

/// Whether a shutdown signal has been received since `install_handler`
/// was called. A service's accept/poll loop should check this on every
/// iteration and exit cleanly (stop accepting new work; let in-flight
/// work finish) once it's `true`.
pub fn shutdown_requested() -> bool {
    SHUTDOWN_REQUESTED.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A single test, not two: `SHUTDOWN_REQUESTED` is a process-global
    // static, and cargo test runs tests within one binary in parallel
    // threads by default — a separate "starts false" test would race
    // against this one flipping the flag, with no defined ordering.
    #[test]
    fn install_handler_does_not_set_the_flag_but_the_handler_function_does() {
        // Installing the handler itself must not flip the flag.
        install_handler();
        assert!(!shutdown_requested());

        // Exercises the handler function directly rather than actually
        // raising a process signal (which would race with, and could be
        // masked/ignored differently by, the test harness's own signal
        // handling across parallel test threads). What matters for
        // correctness is that the handler function itself sets the flag;
        // wiring an OS signal to call it is libc's own well-tested job.
        handle_shutdown_signal(SIGTERM);
        assert!(shutdown_requested());
    }
}
