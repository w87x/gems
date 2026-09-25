//! Types shared across every gems crate: entity IDs, page-size detection,
//! checksums, and the workspace-wide error type. See `ARCHITECTURE.md` at
//! the repo root for the rationale behind each of these.

pub mod crc32c;
pub mod filelock;
pub mod pagesize;
pub mod rand;
pub mod tuid;

pub use tuid::Tuid;

/// Errors shared across storage/index/codec layers. Frontend crates get
/// their own error types that wrap this one.
#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    CorruptExtent {
        detail: &'static str,
    },
    CorruptPage {
        detail: &'static str,
    },
    OutOfSpace,
    NotFound,
    InvalidValue {
        detail: &'static str,
    },
    /// A writable open/create found another process already holding this
    /// store directory's exclusive lock (see `gems-engine::Store`'s module
    /// doc). Two writers racing on the same store's CoW page allocation
    /// and root-pointer publish would corrupt it, so this is refused
    /// rather than attempted.
    AlreadyLocked {
        path: std::path::PathBuf,
    },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io error: {e}"),
            Error::CorruptExtent { detail } => write!(f, "corrupt extent: {detail}"),
            Error::CorruptPage { detail } => write!(f, "corrupt page: {detail}"),
            Error::OutOfSpace => write!(f, "out of space"),
            Error::NotFound => write!(f, "not found"),
            Error::InvalidValue { detail } => write!(f, "invalid value: {detail}"),
            Error::AlreadyLocked { path } => write!(
                f,
                "store at {} is already open (writable) in another process",
                path.display()
            ),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
