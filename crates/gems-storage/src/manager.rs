//! `ExtentManager`: turns the extent/bitmap format in `extent.rs` and the
//! raw mmap'd file in `file.rs` into an actual allocator — `allocate`
//! picks a block class for a requested size, finds (or creates) an extent
//! of that class with a free slot, and hands back a `SlotPointer`; `write`/
//! `read`/`free` operate on a previously allocated slot.
//!
//! Two v1 scope cuts, consistent with the rest of this workspace:
//!
//! - **One data file** (`file_id` is fixed at construction). Sharding
//!   across multiple files is cluster-layer work (ARCHITECTURE.md §6), not
//!   needed to prove out the single-node allocator.
//! - **`ExtentHeader.free_count` isn't maintained precisely.** It's
//!   documented in `extent.rs` as "cached, recomputed on scrub if in
//!   doubt" — this manager doesn't yet have a scrub path, so the field is
//!   only written once at extent-creation time and not kept in sync with
//!   individual allocate/free calls. Nothing here depends on its value.

use gems_common::{Error, Result};

use crate::extent::{
    Bitmap, BlockClass, ExtentHeader, BITMAP_BYTES, DATA_OFFSET, EXTENT_HEADER_LEN, EXTENT_SIZE,
};
use crate::file::ExtentFile;
use crate::slot::SlotPointer;

use std::path::Path;

pub struct ExtentManager {
    file: ExtentFile,
    file_id: u32,
    /// `extents[i]` is the block class of extent `i`, rebuilt from each
    /// extent's on-disk header when opening an existing file.
    extents: Vec<BlockClass>,
}

impl ExtentManager {
    pub fn create(path: &Path, file_id: u32) -> Result<Self> {
        let file = ExtentFile::create(path)?;
        Ok(ExtentManager {
            file,
            file_id,
            extents: Vec::new(),
        })
    }

    pub fn open(path: &Path, file_id: u32, writable: bool) -> Result<Self> {
        let file = ExtentFile::open(path, writable)?;
        let extent_count = file.len() as u64 / EXTENT_SIZE;
        let mut extents = Vec::with_capacity(extent_count as usize);
        for i in 0..extent_count {
            let base = (i * EXTENT_SIZE) as usize;
            let header = file.as_slice()[base..base + EXTENT_HEADER_LEN].to_vec();
            let class_byte = header[6]; // ExtentHeader::block_class field offset
            extents.push(BlockClass::from_index(class_byte)?);
        }
        Ok(ExtentManager {
            file,
            file_id,
            extents,
        })
    }

    fn extent_offset(&self, extent_index: u32) -> usize {
        extent_index as usize * EXTENT_SIZE as usize
    }

    /// Allocate a slot large enough for `size` bytes, growing the file with
    /// a fresh extent if every existing extent of the right class is full.
    pub fn allocate(&mut self, size: usize) -> Result<SlotPointer> {
        let class = BlockClass::for_size(size)?;

        let candidate_indices: Vec<u32> = self
            .extents
            .iter()
            .enumerate()
            .filter(|(_, &c)| c == class)
            .map(|(idx, _)| idx as u32)
            .collect();
        for idx in candidate_indices {
            if let Some(slot) = self.try_allocate_in_extent(idx, class)? {
                return Ok(SlotPointer {
                    file_id: self.file_id,
                    extent_index: idx,
                    slot_index: slot,
                    block_class: class.index(),
                });
            }
        }

        let offset = self.file.grow_by_one_extent()?;
        let extent_index = (offset / EXTENT_SIZE) as u32;
        self.init_extent_header(extent_index, class)?;
        self.extents.push(class);
        let slot = self
            .try_allocate_in_extent(extent_index, class)?
            .expect("a freshly initialized extent always has a free slot");
        Ok(SlotPointer {
            file_id: self.file_id,
            extent_index,
            slot_index: slot,
            block_class: class.index(),
        })
    }

    fn init_extent_header(&mut self, extent_index: u32, class: BlockClass) -> Result<()> {
        let header = ExtentHeader::new(class);
        let base = self.extent_offset(extent_index);
        let buf = self.file.as_mut_slice()?;
        buf[base..base + 4].copy_from_slice(&header.magic.to_le_bytes());
        buf[base + 4..base + 6].copy_from_slice(&header.format_version.to_le_bytes());
        buf[base + 6] = header.block_class;
        buf[base + 7] = header._reserved;
        buf[base + 8..base + 12].copy_from_slice(&header.slot_count.to_le_bytes());
        buf[base + 12..base + 16].copy_from_slice(&header.free_count.to_le_bytes());
        // The bitmap region is already zero: `grow_by_one_extent` extends
        // the file via `ftruncate`, and newly extended bytes read as zero.
        self.file.sync_range(base, EXTENT_HEADER_LEN)
    }

    fn try_allocate_in_extent(
        &mut self,
        extent_index: u32,
        class: BlockClass,
    ) -> Result<Option<u32>> {
        let base = self.extent_offset(extent_index);
        let bitmap_base = base + EXTENT_HEADER_LEN;
        let slot_count = class.slots_per_extent();
        let buf = self.file.as_mut_slice()?;
        let mut bitmap = Bitmap::new(&mut buf[bitmap_base..bitmap_base + BITMAP_BYTES]);
        let allocated = bitmap.allocate(slot_count);
        if allocated.is_some() {
            self.file.sync_range(bitmap_base, BITMAP_BYTES)?;
        }
        Ok(allocated)
    }

    fn slot_offset(&self, ptr: SlotPointer) -> Result<(usize, BlockClass)> {
        if ptr.file_id != self.file_id {
            return Err(Error::InvalidValue {
                detail: "SlotPointer belongs to a different file",
            });
        }
        let class = BlockClass::from_index(ptr.block_class)?;
        let base = self.extent_offset(ptr.extent_index)
            + DATA_OFFSET
            + ptr.slot_index as usize * class.block_size() as usize;
        Ok((base, class))
    }

    pub fn write(&mut self, ptr: SlotPointer, data: &[u8]) -> Result<()> {
        let (base, class) = self.slot_offset(ptr)?;
        if data.len() > class.block_size() as usize {
            return Err(Error::InvalidValue {
                detail: "data does not fit in the slot's block class",
            });
        }
        let buf = self.file.as_mut_slice()?;
        buf[base..base + data.len()].copy_from_slice(data);
        self.file.sync_range(base, data.len())
    }

    /// Read back `len` bytes from `ptr` (the caller knows how much of the
    /// slot is meaningful — e.g. from the entity header's `body_len`).
    pub fn read(&self, ptr: SlotPointer, len: usize) -> Result<&[u8]> {
        let (base, class) = self.slot_offset(ptr)?;
        if len > class.block_size() as usize {
            return Err(Error::InvalidValue {
                detail: "requested length exceeds the slot's block class",
            });
        }
        Ok(&self.file.as_slice()[base..base + len])
    }

    pub fn free(&mut self, ptr: SlotPointer) -> Result<()> {
        if ptr.file_id != self.file_id {
            return Err(Error::InvalidValue {
                detail: "SlotPointer belongs to a different file",
            });
        }
        let base = self.extent_offset(ptr.extent_index);
        let bitmap_base = base + EXTENT_HEADER_LEN;
        let buf = self.file.as_mut_slice()?;
        let mut bitmap = Bitmap::new(&mut buf[bitmap_base..bitmap_base + BITMAP_BYTES]);
        bitmap.free(ptr.slot_index);
        self.file.sync_range(bitmap_base, BITMAP_BYTES)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("gems-storage-manager-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn allocate_write_read_roundtrip() {
        let path = tmp_path("roundtrip.gemx");
        let mut mgr = ExtentManager::create(&path, 0).unwrap();
        let ptr = mgr.allocate(100).unwrap();
        mgr.write(ptr, b"hello world").unwrap();
        assert_eq!(mgr.read(ptr, 11).unwrap(), b"hello world");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn different_sizes_land_in_different_classes() {
        let path = tmp_path("classes.gemx");
        let mut mgr = ExtentManager::create(&path, 0).unwrap();
        let small = mgr.allocate(10).unwrap();
        let large = mgr.allocate(10_000).unwrap();
        assert_ne!(small.block_class, large.block_class);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn fills_extent_then_grows_a_new_one_of_the_same_class() {
        let path = tmp_path("grow.gemx");
        let mut mgr = ExtentManager::create(&path, 0).unwrap();
        let class = BlockClass::for_size(100).unwrap();
        let slots_per_extent = class.slots_per_extent();

        let mut pointers = Vec::new();
        for _ in 0..slots_per_extent {
            pointers.push(mgr.allocate(100).unwrap());
        }
        assert!(pointers.iter().all(|p| p.extent_index == 0));

        // One more allocation of the same size must land in a second extent.
        let overflow = mgr.allocate(100).unwrap();
        assert_eq!(overflow.extent_index, 1);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn free_then_reallocate_reuses_the_slot() {
        let path = tmp_path("free_reuse.gemx");
        let mut mgr = ExtentManager::create(&path, 0).unwrap();
        let ptr = mgr.allocate(100).unwrap();
        mgr.write(ptr, b"first").unwrap();
        mgr.free(ptr).unwrap();

        let ptr2 = mgr.allocate(100).unwrap();
        assert_eq!(ptr2.extent_index, ptr.extent_index);
        assert_eq!(ptr2.slot_index, ptr.slot_index);
        mgr.write(ptr2, b"second").unwrap();
        assert_eq!(&mgr.read(ptr2, 6).unwrap(), b"second");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn reopen_after_close_preserves_extent_classes_and_data() {
        let path = tmp_path("reopen.gemx");
        let ptr;
        {
            let mut mgr = ExtentManager::create(&path, 0).unwrap();
            ptr = mgr.allocate(500).unwrap();
            mgr.write(ptr, b"persisted").unwrap();
        }
        {
            let mgr = ExtentManager::open(&path, 0, false).unwrap();
            assert_eq!(mgr.read(ptr, 9).unwrap(), b"persisted");
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn rejects_value_larger_than_max_block_class() {
        let path = tmp_path("too_large.gemx");
        let mut mgr = ExtentManager::create(&path, 0).unwrap();
        assert!(mgr.allocate(32 * 1024 * 1024).is_err());
        std::fs::remove_file(&path).ok();
    }
}
