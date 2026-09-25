//! Durable persistence for Raft's persistent state — `current_term`,
//! `voted_for`, and `log[]`. Per the Raft paper §5.1, these three fields
//! "must be persisted... before responding to RPCs": a node that forgets
//! which term it's in or who it already voted for after a restart can
//! vote twice in the same term, which is exactly the safety violation
//! Raft's whole voting rule exists to prevent (two leaders elected in the
//! same term). `raft.rs`'s own module doc already flags this as
//! explicitly out of `RaftCore`'s scope — kept a pure, I/O-free state
//! machine for testability — and names it the network shell's job. This
//! module is that piece; `raft_net.rs` is the shell that calls it.
//!
//! Written as a full snapshot, not an incremental append-only log: build
//! the whole encoded state in memory, write it to a temp file, `fsync` the
//! file, then atomically `rename` it over the previous state file (a
//! same-filesystem `rename` is atomic on POSIX, so a crash mid-write
//! leaves either the complete old file or the complete new one, never a
//! torn write), then `fsync` the containing directory so the rename's
//! directory-entry update itself survives a crash too — POSIX doesn't
//! guarantee a `rename` is durable without that. Simpler and safer than
//! incremental appends, at the cost of rewriting the whole log on every
//! persisted change; acceptable since log compaction is separately out of
//! scope for this pass (an unbounded log was already a named limitation
//! before this module existed).

use std::io::Write;
use std::path::{Path, PathBuf};

use gems_common::{Error, Result};

use crate::raft::{LogEntry, NodeId, Term};
use crate::record::LogRecord;

const FILE_NAME: &str = "raft_state.bin";

pub fn state_path(dir: &Path) -> PathBuf {
    dir.join(FILE_NAME)
}

/// Loads persisted state from `dir`, or the Raft-paper-specified zero
/// state (`term 0`, no vote, empty log) if no state file exists yet — a
/// node's first boot.
pub fn load(dir: &Path) -> Result<(Term, Option<NodeId>, Vec<LogEntry>)> {
    let bytes = match std::fs::read(state_path(dir)) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((0, None, Vec::new())),
        Err(e) => return Err(Error::Io(e)),
    };
    decode(&bytes)
}

/// Overwrites `dir`'s persisted state with `term`/`voted_for`/`log`.
pub fn save(dir: &Path, term: Term, voted_for: Option<NodeId>, log: &[LogEntry]) -> Result<()> {
    let encoded = encode(term, voted_for, log);
    let final_path = state_path(dir);
    let tmp_path = dir.join(format!("{FILE_NAME}.tmp"));

    let mut file = std::fs::File::create(&tmp_path)?;
    file.write_all(&encoded)?;
    file.sync_all()?;
    drop(file);

    std::fs::rename(&tmp_path, &final_path)?;

    let dir_fd = rustix::fs::open(
        dir,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY,
        rustix::fs::Mode::empty(),
    )
    .map_err(std::io::Error::from)?;
    rustix::fs::fsync(&dir_fd).map_err(std::io::Error::from)?;

    Ok(())
}

fn encode(term: Term, voted_for: Option<NodeId>, log: &[LogEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&term.to_le_bytes());
    match voted_for {
        Some(id) => {
            out.push(1);
            out.extend_from_slice(&id.to_le_bytes());
        }
        None => {
            out.push(0);
            out.extend_from_slice(&0u32.to_le_bytes());
        }
    }
    out.extend_from_slice(&(log.len() as u32).to_le_bytes());
    for entry in log {
        out.extend_from_slice(&entry.term.to_le_bytes());
        out.extend_from_slice(&entry.command.encode()); // self-framed (u32 length prefix)
    }
    out
}

fn decode(buf: &[u8]) -> Result<(Term, Option<NodeId>, Vec<LogEntry>)> {
    let mut pos = 0;
    let term = read_u64(buf, &mut pos)?;
    let has_voted_for = read_u8(buf, &mut pos)?;
    let voted_for_raw = read_u32(buf, &mut pos)?;
    let voted_for = if has_voted_for != 0 {
        Some(voted_for_raw)
    } else {
        None
    };
    let entry_count = read_u32(buf, &mut pos)? as usize;
    // A corrupted state file (torn write despite the fsync+rename
    // discipline above — e.g. bit rot) shouldn't be able to make this
    // loop walk `pos` past the buffer; each entry is at least 9 bytes
    // (8-byte term + a LogRecord's own 4-byte length prefix, minus the
    // 3 bytes already double-counted... conservatively: 8-byte term + at
    // least 1 byte of framed LogRecord).
    if entry_count > buf.len().saturating_sub(pos) / 9 {
        return Err(Error::InvalidValue {
            detail: "raft state entry_count exceeds what the file could hold",
        });
    }
    let mut log = Vec::with_capacity(entry_count);
    for _ in 0..entry_count {
        let entry_term = read_u64(buf, &mut pos)?;
        let (command, consumed) = LogRecord::decode(&buf[pos..])?.ok_or(Error::InvalidValue {
            detail: "raft state log entry truncated",
        })?;
        pos += consumed;
        log.push(LogEntry {
            term: entry_term,
            command,
        });
    }
    Ok((term, voted_for, log))
}

fn read_u8(buf: &[u8], pos: &mut usize) -> Result<u8> {
    let b = *buf.get(*pos).ok_or(Error::InvalidValue {
        detail: "raft state truncated",
    })?;
    *pos += 1;
    Ok(b)
}

fn read_u32(buf: &[u8], pos: &mut usize) -> Result<u32> {
    if buf.len() < *pos + 4 {
        return Err(Error::InvalidValue {
            detail: "raft state truncated",
        });
    }
    let v = u32::from_le_bytes(buf[*pos..*pos + 4].try_into().unwrap());
    *pos += 4;
    Ok(v)
}

fn read_u64(buf: &[u8], pos: &mut usize) -> Result<u64> {
    if buf.len() < *pos + 8 {
        return Err(Error::InvalidValue {
            detail: "raft state truncated",
        });
    }
    let v = u64::from_le_bytes(buf[*pos..*pos + 8].try_into().unwrap());
    *pos += 8;
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gems_catalog::{EntityFlags, EntityHeader, EntityKind};
    use gems_common::Tuid;

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("gems-cluster-raft-state-test")
            .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_entry(seed: u8) -> LogEntry {
        LogEntry {
            term: seed as u64,
            command: LogRecord::Insert {
                header: EntityHeader {
                    id: Tuid::new([seed; 16], seed as u64),
                    created_by: [0u8; 16],
                    modified_by: [0u8; 16],
                    modified_at_ns: 0,
                    name: "e".to_string(),
                    description: String::new(),
                    flags: EntityFlags::NONE,
                    entity_kind: EntityKind::Data,
                    schema_ref: Tuid::NIL,
                    body_offset: 0,
                    body_len: 3,
                },
                body: b"abc".to_vec(),
            },
        }
    }

    #[test]
    fn load_with_no_file_yet_returns_the_zero_state() {
        let dir = tmp_dir("no_file");
        let (term, voted_for, log) = load(&dir).unwrap();
        assert_eq!(term, 0);
        assert_eq!(voted_for, None);
        assert!(log.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_then_load_round_trips_term_vote_and_log() {
        let dir = tmp_dir("roundtrip");
        let log = vec![sample_entry(1), sample_entry(2)];
        save(&dir, 7, Some(3), &log).unwrap();
        let (term, voted_for, loaded_log) = load(&dir).unwrap();
        assert_eq!(term, 7);
        assert_eq!(voted_for, Some(3));
        assert_eq!(loaded_log, log);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_then_load_round_trips_no_vote_and_empty_log() {
        let dir = tmp_dir("empty");
        save(&dir, 1, None, &[]).unwrap();
        let (term, voted_for, log) = load(&dir).unwrap();
        assert_eq!(term, 1);
        assert_eq!(voted_for, None);
        assert!(log.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_second_save_overwrites_the_first_rather_than_appending() {
        let dir = tmp_dir("overwrite");
        save(&dir, 1, Some(1), &[sample_entry(1)]).unwrap();
        save(&dir, 2, Some(2), &[sample_entry(1), sample_entry(2)]).unwrap();
        let (term, voted_for, log) = load(&dir).unwrap();
        assert_eq!(term, 2);
        assert_eq!(voted_for, Some(2));
        assert_eq!(log.len(), 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn no_tmp_file_is_left_behind_after_a_successful_save() {
        let dir = tmp_dir("no_leftover_tmp");
        save(&dir, 1, None, &[]).unwrap();
        assert!(!dir.join(format!("{FILE_NAME}.tmp")).exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn decode_rejects_an_entry_count_bigger_than_the_buffer_could_hold() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&1u64.to_le_bytes()); // term
        buf.push(0); // no voted_for
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&1_000_000_000u32.to_le_bytes()); // entry_count
        assert!(decode(&buf).is_err());
    }
}
