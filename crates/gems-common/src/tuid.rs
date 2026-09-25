//! `Tuid`: a Timestamped UID — 16 random bytes (a UUIDv4-shaped identifier)
//! followed by an 8-byte creation timestamp (nanoseconds since the Unix
//! epoch, big-endian so byte-order comparison sorts by creation time).
//!
//! This is the primary key for every entity (see ARCHITECTURE.md §2.1): the
//! UUID half stays a normal opaque global identifier for external
//! references, while comparing the full 24 bytes also gives creation order,
//! which the B*-tree and "recently created" scans both want without a
//! second index.

use std::fmt;

pub const TUID_LEN: usize = 24;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Tuid([u8; TUID_LEN]);

impl Tuid {
    /// Build a `Tuid` from a caller-supplied random UUID and timestamp.
    /// Randomness is intentionally not sourced here: callers already have
    /// an RNG appropriate to their context (test determinism, a CSPRNG for
    /// production) and we avoid pulling one in as a dependency.
    pub fn new(uuid_bytes: [u8; 16], created_at_ns: u64) -> Self {
        let mut bytes = [0u8; TUID_LEN];
        bytes[..16].copy_from_slice(&uuid_bytes);
        bytes[16..].copy_from_slice(&created_at_ns.to_be_bytes());
        Tuid(bytes)
    }

    pub fn from_bytes(bytes: [u8; TUID_LEN]) -> Self {
        Tuid(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; TUID_LEN] {
        &self.0
    }

    pub fn uuid(&self) -> [u8; 16] {
        self.0[..16].try_into().unwrap()
    }

    pub fn created_at_ns(&self) -> u64 {
        u64::from_be_bytes(self.0[16..].try_into().unwrap())
    }

    pub const NIL: Tuid = Tuid([0u8; TUID_LEN]);
}

impl fmt::Debug for Tuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let u = self.uuid();
        write!(
            f,
            "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}@{}",
            u[0], u[1], u[2], u[3], u[4], u[5], u[6], u[7], u[8], u[9], u[10], u[11], u[12], u[13], u[14], u[15],
            self.created_at_ns()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let uuid = [1u8; 16];
        let t = Tuid::new(uuid, 42);
        assert_eq!(t.uuid(), uuid);
        assert_eq!(t.created_at_ns(), 42);
    }

    #[test]
    fn orders_by_creation_time_when_uuid_equal() {
        let a = Tuid::new([0u8; 16], 1);
        let b = Tuid::new([0u8; 16], 2);
        assert!(a < b);
    }
}
