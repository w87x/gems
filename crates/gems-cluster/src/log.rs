//! `ReplicationLog`: an append-only file of `LogRecord`s. No mmap here —
//! this is purely sequential access (append at the end, read forward from
//! some offset), so plain buffered file I/O is the right tool; per
//! ARCHITECTURE.md §1.1, `std::fs::File` is fine for that, the "no
//! `std::fs`" guidance is specifically about needing explicit control over
//! growth/mmap, neither of which applies to a sequential log.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use gems_common::Result;

use crate::record::LogRecord;

pub struct ReplicationLog {
    path: PathBuf,
}

impl ReplicationLog {
    /// Open the log at `path`, creating an empty one if it doesn't exist.
    pub fn open(path: &Path) -> Result<Self> {
        OpenOptions::new().create(true).append(true).open(path)?;
        Ok(ReplicationLog {
            path: path.to_path_buf(),
        })
    }

    /// Append one record, `fsync`-ing before returning — a replica must
    /// never be told about a record the primary itself hasn't durably
    /// committed.
    pub fn append(&self, record: &LogRecord) -> Result<()> {
        let mut f = OpenOptions::new().append(true).open(&self.path)?;
        f.write_all(&record.encode())?;
        f.sync_data()?;
        Ok(())
    }

    /// Current length of the log in bytes — the offset a new reader should
    /// start from to see only future records.
    pub fn len(&self) -> Result<u64> {
        Ok(std::fs::metadata(&self.path)?.len())
    }

    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every complete record starting at byte offset `from`, plus the new
    /// offset to resume from next time. The returned offset can be less
    /// than the file's current length if the tail holds a record that's
    /// still being written (append isn't atomic across the length prefix
    /// and payload) — callers should re-request from the returned offset
    /// rather than assuming they've drained the file.
    pub fn read_from(&self, from: u64) -> Result<(Vec<LogRecord>, u64)> {
        let mut f = File::open(&self.path)?;
        f.seek(SeekFrom::Start(from))?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;

        let mut records = Vec::new();
        let mut pos = 0;
        while let Some((record, consumed)) = LogRecord::decode(&buf[pos..])? {
            records.push(record);
            pos += consumed;
        }
        Ok((records, from + pos as u64))
    }

    /// Like `read_from`, but returns the raw bytes of only the complete
    /// records (no partial tail) instead of decoding them — what a
    /// replication server actually streams to a connected replica, so the
    /// replica does its own decoding rather than trusting a
    /// re-serialization on the server side to match byte-for-byte.
    pub fn read_raw_from(&self, from: u64) -> Result<(Vec<u8>, u64)> {
        let mut f = File::open(&self.path)?;
        f.seek(SeekFrom::Start(from))?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;

        let mut pos = 0;
        while let Some((_, consumed)) = LogRecord::decode(&buf[pos..])? {
            pos += consumed;
        }
        Ok((buf[..pos].to_vec(), from + pos as u64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gems_common::Tuid;

    fn tmp_path(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("gems-cluster-log-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let _ = std::fs::remove_file(&path);
        path
    }

    fn delete_record(n: u8) -> LogRecord {
        LogRecord::Delete {
            id: Tuid::new([n; 16], n as u64),
        }
    }

    #[test]
    fn append_and_read_from_zero() {
        let path = tmp_path("append.gemlog");
        let log = ReplicationLog::open(&path).unwrap();
        log.append(&delete_record(1)).unwrap();
        log.append(&delete_record(2)).unwrap();

        let (records, offset) = log.read_from(0).unwrap();
        assert_eq!(records, vec![delete_record(1), delete_record(2)]);
        assert_eq!(offset, log.len().unwrap());

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn read_from_nonzero_offset_skips_earlier_records() {
        let path = tmp_path("offset.gemlog");
        let log = ReplicationLog::open(&path).unwrap();
        log.append(&delete_record(1)).unwrap();
        let midpoint = log.len().unwrap();
        log.append(&delete_record(2)).unwrap();

        let (records, offset) = log.read_from(midpoint).unwrap();
        assert_eq!(records, vec![delete_record(2)]);
        assert_eq!(offset, log.len().unwrap());

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn empty_log_reads_nothing() {
        let path = tmp_path("empty.gemlog");
        let log = ReplicationLog::open(&path).unwrap();
        assert!(log.is_empty().unwrap());
        let (records, offset) = log.read_from(0).unwrap();
        assert!(records.is_empty());
        assert_eq!(offset, 0);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn read_raw_from_matches_what_read_from_would_decode() {
        let path = tmp_path("raw.gemlog");
        let log = ReplicationLog::open(&path).unwrap();
        log.append(&delete_record(1)).unwrap();
        log.append(&delete_record(2)).unwrap();

        let (raw, raw_offset) = log.read_raw_from(0).unwrap();
        let mut pos = 0;
        let mut decoded = Vec::new();
        while let Some((record, consumed)) = LogRecord::decode(&raw[pos..]).unwrap() {
            decoded.push(record);
            pos += consumed;
        }
        assert_eq!(decoded, vec![delete_record(1), delete_record(2)]);
        assert_eq!(raw_offset, log.len().unwrap());

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn reopening_an_existing_log_preserves_its_contents() {
        let path = tmp_path("reopen.gemlog");
        {
            let log = ReplicationLog::open(&path).unwrap();
            log.append(&delete_record(1)).unwrap();
        }
        {
            let log = ReplicationLog::open(&path).unwrap();
            let (records, _) = log.read_from(0).unwrap();
            assert_eq!(records, vec![delete_record(1)]);
        }
        std::fs::remove_file(&path).ok();
    }
}
