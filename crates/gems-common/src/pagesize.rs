//! Runtime OS page-size detection. Never hardcode 4096: on macOS/ARM the
//! native page size is 16384. This value governs `mmap()` offset/length
//! rounding only — it is deliberately *not* the same knob as the logical
//! B-tree page size (§0, §2.3 of ARCHITECTURE.md), which is fixed at
//! file-creation time and stored in the file header so files stay portable
//! across platforms with different native page sizes.

use std::sync::OnceLock;

static OS_PAGE_SIZE: OnceLock<usize> = OnceLock::new();

pub fn os_page_size() -> usize {
    *OS_PAGE_SIZE.get_or_init(rustix::param::page_size)
}

/// Round `len` up to a multiple of the OS page size, as required before
/// `mmap`/`ftruncate` calls that must land on page boundaries.
pub fn round_up_to_os_page(len: usize) -> usize {
    let page = os_page_size();
    (len + page - 1) & !(page - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_size_is_a_power_of_two() {
        let p = os_page_size();
        assert!(p >= 4096);
        assert_eq!(p & (p - 1), 0);
    }

    #[test]
    fn rounding_is_idempotent() {
        let p = os_page_size();
        assert_eq!(round_up_to_os_page(1), p);
        assert_eq!(round_up_to_os_page(p), p);
        assert_eq!(round_up_to_os_page(p + 1), 2 * p);
    }
}
