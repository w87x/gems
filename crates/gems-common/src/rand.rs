//! Randomness, via `rustix`'s `rand` feature (a thin wrapper over the
//! host's `getrandom`/`getentropy` syscall) — the one thing besides file
//! I/O this workspace still needs from outside the standard library, and
//! covered by the same "borrow rustix rather than a crate" exception the
//! rest of the storage layer already takes.

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    rustix::rand::getrandom(&mut buf, rustix::rand::GetRandomFlags::empty())
        .expect("host entropy source (getrandom/getentropy) unavailable");
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn produces_nonzero_bytes() {
        // Not a strong randomness test — just a smoke test that the
        // syscall path is wired up rather than silently returning zeros.
        let a: [u8; 16] = random_bytes();
        let b: [u8; 16] = random_bytes();
        assert_ne!(a, [0u8; 16]);
        assert_ne!(a, b);
    }
}
