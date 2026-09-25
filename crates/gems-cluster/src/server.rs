//! `ReplicationServer`: the primary side of log shipping. A replica
//! connects, sends `SYNC <offset>\n`, and the server streams every
//! complete record from that offset onward — including ones appended
//! *after* the connection opened, via a short poll loop (no OS-specific
//! file-change notification; a 50ms poll is simple, portable, and more
//! than fast enough for a replication lag budget measured in seconds).
//!
//! One thread per connection, blocking I/O throughout — consistent with
//! ARCHITECTURE.md §9's take on the WebUI ("admin-tool traffic levels
//! don't need an async runtime"); replication traffic between a handful of
//! shard replicas is the same story.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use gems_common::{Error, Result};

use crate::log::ReplicationLog;

const POLL_INTERVAL: Duration = Duration::from_millis(50);

pub struct ReplicationServer {
    log_path: PathBuf,
}

impl ReplicationServer {
    pub fn new(log_path: PathBuf) -> Self {
        ReplicationServer { log_path }
    }

    /// Accept connections on `listener` forever, one thread per
    /// connection. Returns only if `accept` itself errors (e.g. the
    /// listener was closed) — a real deployment runs this on a dedicated
    /// thread for the process's lifetime.
    pub fn serve(&self, listener: TcpListener) -> Result<()> {
        for stream in listener.incoming() {
            let stream = stream?;
            let log_path = self.log_path.clone();
            thread::spawn(move || {
                let _ = Self::handle_connection(stream, &log_path);
            });
        }
        Ok(())
    }

    /// Convenience for tests and simple setups: bind `addr`, spawn the
    /// accept loop on a background thread, and return the actual bound
    /// address (useful with port `0` for an OS-assigned ephemeral port).
    pub fn spawn(log_path: PathBuf, addr: &str) -> Result<std::net::SocketAddr> {
        let listener = TcpListener::bind(addr)?;
        let bound = listener.local_addr()?;
        let server = ReplicationServer::new(log_path);
        thread::spawn(move || {
            let _ = server.serve(listener);
        });
        Ok(bound)
    }

    fn handle_connection(stream: TcpStream, log_path: &Path) -> Result<()> {
        let mut writer = stream.try_clone()?;
        let mut reader = BufReader::new(stream);

        let mut line = String::new();
        reader.read_line(&mut line)?;
        let offset: u64 = line
            .trim()
            .strip_prefix("SYNC ")
            .and_then(|s| s.parse().ok())
            .ok_or(Error::InvalidValue {
                detail: "expected 'SYNC <offset>' as the first line",
            })?;

        let log = ReplicationLog::open(log_path)?;
        let mut pos = offset;
        loop {
            let (raw, new_pos) = log.read_raw_from(pos)?;
            if raw.is_empty() {
                thread::sleep(POLL_INTERVAL);
                continue;
            }
            writer.write_all(&raw)?;
            pos = new_pos;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::LogRecord;
    use gems_common::Tuid;
    use std::io::Read;
    use std::time::Duration as StdDuration;

    fn tmp_path(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("gems-cluster-server-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn streams_records_present_before_connecting() {
        let log_path = tmp_path("presync.gemlog");
        let log = ReplicationLog::open(&log_path).unwrap();
        log.append(&LogRecord::Delete {
            id: Tuid::new([1u8; 16], 1),
        })
        .unwrap();

        let addr = ReplicationServer::spawn(log_path.clone(), "127.0.0.1:0").unwrap();
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(StdDuration::from_secs(5)))
            .unwrap();
        stream.write_all(b"SYNC 0\n").unwrap();

        let mut buf = vec![0u8; 4096];
        let n = stream.read(&mut buf).unwrap();
        let (record, _) = LogRecord::decode(&buf[..n]).unwrap().unwrap();
        assert_eq!(
            record,
            LogRecord::Delete {
                id: Tuid::new([1u8; 16], 1)
            }
        );

        std::fs::remove_file(&log_path).ok();
    }

    #[test]
    fn streams_records_appended_after_connecting() {
        let log_path = tmp_path("postsync.gemlog");
        let log = ReplicationLog::open(&log_path).unwrap();

        let addr = ReplicationServer::spawn(log_path.clone(), "127.0.0.1:0").unwrap();
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(StdDuration::from_secs(5)))
            .unwrap();
        stream.write_all(b"SYNC 0\n").unwrap();

        // Give the server a moment to enter its poll loop, then append —
        // the point of this test is that the record is still delivered
        // even though it didn't exist at connect time.
        thread::sleep(StdDuration::from_millis(100));
        log.append(&LogRecord::Delete {
            id: Tuid::new([2u8; 16], 2),
        })
        .unwrap();

        let mut buf = vec![0u8; 4096];
        let n = stream.read(&mut buf).unwrap();
        let (record, _) = LogRecord::decode(&buf[..n]).unwrap().unwrap();
        assert_eq!(
            record,
            LogRecord::Delete {
                id: Tuid::new([2u8; 16], 2)
            }
        );

        std::fs::remove_file(&log_path).ok();
    }

    #[test]
    fn resumes_from_a_nonzero_offset() {
        let log_path = tmp_path("resume.gemlog");
        let log = ReplicationLog::open(&log_path).unwrap();
        log.append(&LogRecord::Delete {
            id: Tuid::new([1u8; 16], 1),
        })
        .unwrap();
        let midpoint = log.len().unwrap();
        log.append(&LogRecord::Delete {
            id: Tuid::new([2u8; 16], 2),
        })
        .unwrap();

        let addr = ReplicationServer::spawn(log_path.clone(), "127.0.0.1:0").unwrap();
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(StdDuration::from_secs(5)))
            .unwrap();
        stream
            .write_all(format!("SYNC {midpoint}\n").as_bytes())
            .unwrap();

        let mut buf = vec![0u8; 4096];
        let n = stream.read(&mut buf).unwrap();
        let (record, _) = LogRecord::decode(&buf[..n]).unwrap().unwrap();
        assert_eq!(
            record,
            LogRecord::Delete {
                id: Tuid::new([2u8; 16], 2)
            },
            "must not re-send the record before the requested offset"
        );

        std::fs::remove_file(&log_path).ok();
    }
}
