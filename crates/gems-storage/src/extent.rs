//! Extent layout: a 16 MiB **data region**, prefixed by a small fixed
//! header and a full 4 KiB allocation bitmap. 16 MiB / 512 B = 32768 slots
//! = exactly the number of bits in a 4 KiB bitmap, so the smallest block
//! class fills the bitmap exactly and every larger class uses a leading
//! prefix of the same bitmap. See ARCHITECTURE.md §1.2.
//!
//! On disk, one extent is laid out as:
//! ```text
//! [ EXTENT_HEADER_LEN bytes: header ][ BITMAP_BYTES: bitmap ][ DATA_REGION_SIZE: slots ]
//! ```
//! An earlier version of this file carved the header out of the same 4 KiB
//! page as the bitmap, which quietly broke the "exact fit" claim above:
//! 16 bytes of header inside a nominally-4096-byte bitmap left only 4080
//! bytes (32640 bits) of real bitmap capacity for a class that needed
//! 32768 bits, and `slots_per_extent` separately assumed the *whole*
//! 16 MiB extent was available as data when 4 KiB of it was actually the
//! header+bitmap page — both wrong in the same direction, and both caught
//! by `ExtentManager`'s tests hitting an out-of-bounds bitmap access. The
//! header now lives in its own small region ahead of the bitmap so the
//! bitmap is genuinely the full, untouched 4 KiB the math depends on.

use gems_common::{Error, Result};

/// The 16 MiB **data region** every extent's slots live in — this is the
/// "16M data block" the bitmap math is built around, separate from the
/// header/bitmap prefix ahead of it.
pub const DATA_REGION_SIZE: u64 = 16 * 1024 * 1024;

/// Fixed header fields ahead of the bitmap: magic(4) + format_version(2) +
/// block_class(1) + reserved(1) + slot_count(4) + free_count(4).
pub const EXTENT_HEADER_LEN: usize = 4 + 2 + 1 + 1 + 4 + 4;

/// The allocation bitmap: a full 4 KiB, entirely dedicated to slot bits
/// (no header fields mixed in — see the module doc).
pub const BITMAP_BYTES: usize = 4096;
pub const BITMAP_BITS: usize = BITMAP_BYTES * 8; // 32768

/// Byte offset of the data region within an extent.
pub const DATA_OFFSET: usize = EXTENT_HEADER_LEN + BITMAP_BYTES;

/// Total on-disk footprint of one extent: header + bitmap + data region.
/// This is what callers advance a file offset by per extent; it is
/// slightly more than 16 MiB (by `EXTENT_HEADER_LEN + BITMAP_BYTES`
/// bytes), not exactly 16 MiB.
pub const EXTENT_SIZE: u64 = EXTENT_HEADER_LEN as u64 + BITMAP_BYTES as u64 + DATA_REGION_SIZE;

/// Smallest and largest block sizes, per ARCHITECTURE.md §1.2. Classes are
/// powers of two in between.
pub const MIN_BLOCK_SIZE: u32 = 512;
pub const MAX_BLOCK_SIZE: u32 = DATA_REGION_SIZE as u32; // 16 MiB: one slot per extent

/// A block-size class, encoded as `log2(block_size) - log2(MIN_BLOCK_SIZE)`,
/// so 0 => 512B, 1 => 1KiB, ..., 15 => 16MiB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockClass(u8);

impl BlockClass {
    pub const COUNT: u8 = 16;

    pub fn from_block_size(block_size: u32) -> Result<Self> {
        if !block_size.is_power_of_two() || !(MIN_BLOCK_SIZE..=MAX_BLOCK_SIZE).contains(&block_size)
        {
            return Err(Error::InvalidValue {
                detail: "block size must be a power of two between 512B and 16MiB",
            });
        }
        let class = block_size.trailing_zeros() - MIN_BLOCK_SIZE.trailing_zeros();
        Ok(BlockClass(class as u8))
    }

    pub fn from_index(index: u8) -> Result<Self> {
        if index >= Self::COUNT {
            return Err(Error::InvalidValue {
                detail: "block class index out of range",
            });
        }
        Ok(BlockClass(index))
    }

    pub fn index(&self) -> u8 {
        self.0
    }

    pub fn block_size(&self) -> u32 {
        MIN_BLOCK_SIZE << self.0
    }

    /// How many slots of this class's size fit in one extent's 16 MiB data
    /// region. Exact for every class (`DATA_REGION_SIZE` is a multiple of
    /// every valid block size), and — for the smallest class — exactly
    /// `BITMAP_BITS`, by construction of the constants above.
    pub fn slots_per_extent(&self) -> u32 {
        (DATA_REGION_SIZE / self.block_size() as u64) as u32
    }

    /// The smallest block class whose slots fit at least `size` bytes.
    /// Errors if `size` exceeds the largest class (16 MiB) — chaining a
    /// value across multiple whole-extent slots (ARCHITECTURE.md §1.2's
    /// oversized-value path) isn't implemented yet.
    pub fn for_size(size: usize) -> Result<Self> {
        let mut block_size = MIN_BLOCK_SIZE as usize;
        while block_size < size {
            block_size *= 2;
            if block_size as u32 > MAX_BLOCK_SIZE {
                return Err(Error::InvalidValue {
                    detail: "value exceeds the largest block class (16MiB); \
                             oversized-value chaining is not implemented",
                });
            }
        }
        Self::from_block_size(block_size as u32)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ExtentHeader {
    pub magic: u32,
    pub format_version: u16,
    pub block_class: u8,
    pub _reserved: u8,
    pub slot_count: u32,
    pub free_count: u32,
}

pub const EXTENT_MAGIC: u32 = 0x47454d31; // "GEM1"

impl ExtentHeader {
    pub fn new(class: BlockClass) -> Self {
        let slot_count = class.slots_per_extent();
        ExtentHeader {
            magic: EXTENT_MAGIC,
            format_version: 1,
            block_class: class.index(),
            _reserved: 0,
            slot_count,
            free_count: slot_count,
        }
    }

    pub fn class(&self) -> Result<BlockClass> {
        BlockClass::from_index(self.block_class)
    }
}

/// Allocation bitmap for one extent: 1 bit per slot, 1 = allocated. Only
/// the first `slot_count` bits are meaningful; the rest of the 4 KiB is
/// zeroed and unused for classes larger than 512 B.
pub struct Bitmap<'a> {
    bytes: &'a mut [u8],
}

impl<'a> Bitmap<'a> {
    /// `bytes` must be exactly `BITMAP_BYTES` long — the extent's full,
    /// untouched bitmap region (see the module doc for why it must not
    /// overlap the header).
    pub fn new(bytes: &'a mut [u8]) -> Self {
        debug_assert_eq!(bytes.len(), BITMAP_BYTES);
        Bitmap { bytes }
    }

    fn get(&self, slot: u32) -> bool {
        let byte = (slot / 8) as usize;
        let bit = slot % 8;
        (self.bytes[byte] >> bit) & 1 != 0
    }

    fn set(&mut self, slot: u32, value: bool) {
        let byte = (slot / 8) as usize;
        let bit = slot % 8;
        if value {
            self.bytes[byte] |= 1 << bit;
        } else {
            self.bytes[byte] &= !(1 << bit);
        }
    }

    /// Find and claim the first free slot below `slot_count`. Returns the
    /// slot index, or `None` if the extent is full.
    pub fn allocate(&mut self, slot_count: u32) -> Option<u32> {
        for slot in 0..slot_count {
            if !self.get(slot) {
                self.set(slot, true);
                return Some(slot);
            }
        }
        None
    }

    pub fn free(&mut self, slot: u32) {
        self.set(slot, false);
    }

    pub fn is_allocated(&self, slot: u32) -> bool {
        self.get(slot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extent_header_len_matches_manual_byte_layout() {
        // ExtentManager hand-encodes the header at fixed byte offsets
        // (0/4/6/7/8/12, 16 bytes total) rather than through Rust's struct
        // layout; this pins EXTENT_HEADER_LEN to match that assumption so
        // a future field reorder/addition fails loudly instead of silently
        // misaligning the bitmap that follows it.
        assert_eq!(EXTENT_HEADER_LEN, 16);
    }

    #[test]
    fn min_class_fills_bitmap_exactly() {
        let class = BlockClass::from_block_size(MIN_BLOCK_SIZE).unwrap();
        assert_eq!(class.slots_per_extent() as usize, BITMAP_BITS);
    }

    #[test]
    fn max_class_is_one_slot() {
        let class = BlockClass::from_block_size(MAX_BLOCK_SIZE).unwrap();
        assert_eq!(class.slots_per_extent(), 1);
    }

    #[test]
    fn every_class_slot_count_fits_within_the_bitmap() {
        for i in 0..BlockClass::COUNT {
            let class = BlockClass::from_index(i).unwrap();
            assert!(
                class.slots_per_extent() as usize <= BITMAP_BITS,
                "class {i} needs more slots than the bitmap can represent"
            );
        }
    }

    #[test]
    fn rejects_non_power_of_two() {
        assert!(BlockClass::from_block_size(1000).is_err());
    }

    #[test]
    fn bitmap_allocate_and_free() {
        let mut buf = vec![0u8; BITMAP_BYTES];
        let mut bm = Bitmap::new(&mut buf);
        let a = bm.allocate(8).unwrap();
        let b = bm.allocate(8).unwrap();
        assert_ne!(a, b);
        assert!(bm.is_allocated(a));
        bm.free(a);
        assert!(!bm.is_allocated(a));
    }

    #[test]
    fn bitmap_exhausts() {
        let mut buf = vec![0u8; BITMAP_BYTES];
        let mut bm = Bitmap::new(&mut buf);
        for _ in 0..4 {
            bm.allocate(4).unwrap();
        }
        assert!(bm.allocate(4).is_none());
    }
}
