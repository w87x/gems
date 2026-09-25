//! Extent layout: a 16 MiB region dedicated to one block-size class, with a
//! 4 KiB allocation bitmap as its first 4 KiB. 16 MiB / 512 B = 32768 slots
//! = exactly the number of bits in a 4 KiB bitmap, so the smallest block
//! class fills the bitmap exactly and every larger class uses a leading
//! prefix of the same bitmap. See ARCHITECTURE.md §1.2.

use gems_common::{Error, Result};

/// Every extent is exactly this many bytes.
pub const EXTENT_SIZE: u64 = 16 * 1024 * 1024;

/// The allocation bitmap + extent header occupy the first 4 KiB of every
/// extent, regardless of block-size class.
pub const BITMAP_BYTES: usize = 4096;
pub const BITMAP_BITS: usize = BITMAP_BYTES * 8; // 32768

/// Smallest and largest block sizes, per ARCHITECTURE.md §1.2. Classes are
/// powers of two in between.
pub const MIN_BLOCK_SIZE: u32 = 512;
pub const MAX_BLOCK_SIZE: u32 = EXTENT_SIZE as u32; // 16 MiB: one slot per extent

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

    /// How many slots of this class's size fit in one extent's data region.
    pub fn slots_per_extent(&self) -> u32 {
        (EXTENT_SIZE / self.block_size() as u64) as u32
    }
}

/// The fixed-size portion of an extent's 4 KiB header, preceding the bitmap
/// bytes. Kept deliberately small so the vast majority of the 4 KiB is
/// available to the bitmap itself.
#[repr(C)]
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
pub const HEADER_FIELDS_SIZE: usize = std::mem::size_of::<ExtentHeader>();

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
    /// `bytes` must be exactly the bitmap region: `BITMAP_BYTES -
    /// HEADER_FIELDS_SIZE` bytes long, immediately following the header
    /// fields in the extent's first page.
    pub fn new(bytes: &'a mut [u8]) -> Self {
        debug_assert_eq!(bytes.len(), BITMAP_BYTES - HEADER_FIELDS_SIZE);
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
    fn rejects_non_power_of_two() {
        assert!(BlockClass::from_block_size(1000).is_err());
    }

    #[test]
    fn bitmap_allocate_and_free() {
        let mut buf = vec![0u8; BITMAP_BYTES - HEADER_FIELDS_SIZE];
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
        let mut buf = vec![0u8; BITMAP_BYTES - HEADER_FIELDS_SIZE];
        let mut bm = Bitmap::new(&mut buf);
        for _ in 0..4 {
            bm.allocate(4).unwrap();
        }
        assert!(bm.allocate(4).is_none());
    }
}
