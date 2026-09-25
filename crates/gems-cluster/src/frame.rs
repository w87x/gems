//! A shared cap on the `u32` length prefix every wire/on-disk framing in
//! this crate uses (`record::LogRecord`, `raft::wire::Envelope`,
//! `gossip::wire::Envelope`).
//!
//! Without this, a peer (or a corrupted log file) can put an arbitrary
//! `u32` in the length prefix — up to 4 GiB — and the reader loops in
//! `raft_net.rs`/`swim_net.rs`/`replica.rs` accumulate bytes into a
//! `Vec<u8>` until a complete frame arrives, since a partial frame decodes
//! to `Ok(None)` rather than an error. A claimed length far larger than any
//! real message means that buffer grows without bound for as long as the
//! peer keeps the connection open and trickles bytes (or never finishes,
//! holding the memory indefinitely) — an easy memory-exhaustion DoS from a
//! single malicious or buggy peer, no authentication bypass required.
//!
//! The fix is to reject an oversized claimed length as soon as the 4-byte
//! prefix itself is available, before waiting for the rest of the frame to
//! arrive — so the accumulating buffer never grows past this cap in the
//! first place. `MAX_FRAME_LEN` is sized generously above any legitimate
//! message this crate produces (the largest is an `AppendEntries` batch of
//! `LogRecord`s, each of which embeds an entity body) while still being
//! small enough that even holding the max in memory per in-flight
//! connection is cheap.

use gems_common::{Error, Result};

pub const MAX_FRAME_LEN: usize = 64 * 1024 * 1024; // 64 MiB

/// Checks a claimed payload length against `MAX_FRAME_LEN`, returning an
/// error naming `context` if it's over the cap. Call this immediately
/// after reading a length prefix, before using it to size any buffer or
/// wait for that many bytes to arrive.
pub fn check_frame_len(payload_len: usize, context: &'static str) -> Result<()> {
    if payload_len > MAX_FRAME_LEN {
        return Err(Error::InvalidValue { detail: context });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_lengths_at_or_under_the_cap() {
        assert!(check_frame_len(0, "x").is_ok());
        assert!(check_frame_len(MAX_FRAME_LEN, "x").is_ok());
    }

    #[test]
    fn rejects_lengths_over_the_cap() {
        assert!(check_frame_len(MAX_FRAME_LEN + 1, "x").is_err());
        assert!(check_frame_len(u32::MAX as usize, "x").is_err());
    }
}
