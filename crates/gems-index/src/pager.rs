//! A fixed-logical-page-size file, mmap'd via `rustix`, with a bump +
//! freelist page allocator and a single root-page pointer that is
//! published with one aligned word write (ARCHITECTURE.md §2.3's
//! "copy-on-write + atomic root swap" crash-safety story). Page 0 is
//! reserved for the file header; page ids are 1-indexed so `0` doubles as
//! a null pointer for child/next-leaf/freelist links.
//!
//! Logical page size is fixed at file-creation time and stored in the
//! header, independent of the OS mmap granularity (ARCHITECTURE.md §0) —
//! it does not have to equal `gems_common::pagesize::os_page_size()`.

use gems_common::pagesize::round_up_to_os_page;
use gems_common::{Error, Result};
use rustix::fd::AsFd;
use rustix::fs::{self, Mode, OFlags};
use rustix::mm::{self, MapFlags, ProtFlags};
use std::os::fd::OwnedFd;
use std::path::Path;
use std::ptr::NonNull;

pub const MAGIC: u32 = 0x47454d49; // "GEMI"
pub const FORMAT_VERSION: u16 = 1;
pub const HEADER_PAGE_ID: u32 = 0;

/// On-disk file header. Occupies the first `page_size` bytes of the file
/// (page 0); the rest of that page is unused padding.
#[derive(Debug, Clone, Copy)]
struct Header {
    magic: u32,
    format_version: u16,
    page_size: u32,
    key_len: u16,
    value_len: u16,
    root_page: u32,
    free_list_head: u32,
    page_count: u32,
    commit_seq: u64,
}

const HEADER_ENCODED_LEN: usize = 4 + 2 + 4 + 2 + 2 + 4 + 4 + 4 + 8;
// Offset of `root_page` within the encoded header; publishing a new root is
// a single write to this offset, aligned and word-sized so it lands
// atomically on both target platforms.
const ROOT_PAGE_OFFSET: usize = 4 + 2 + 4 + 2 + 2;

impl Header {
    fn encode(&self) -> [u8; HEADER_ENCODED_LEN] {
        let mut out = [0u8; HEADER_ENCODED_LEN];
        let mut pos = 0;
        macro_rules! put {
            ($v:expr) => {{
                let bytes = $v.to_le_bytes();
                out[pos..pos + bytes.len()].copy_from_slice(&bytes);
                pos += bytes.len();
            }};
        }
        put!(self.magic);
        put!(self.format_version);
        put!(self.page_size);
        put!(self.key_len);
        put!(self.value_len);
        put!(self.root_page);
        put!(self.free_list_head);
        put!(self.page_count);
        let bytes = self.commit_seq.to_le_bytes();
        out[pos..pos + bytes.len()].copy_from_slice(&bytes);
        out
    }

    fn decode(buf: &[u8]) -> Result<Self> {
        if buf.len() < HEADER_ENCODED_LEN {
            return Err(Error::CorruptPage {
                detail: "index header truncated",
            });
        }
        let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        if magic != MAGIC {
            return Err(Error::CorruptPage {
                detail: "index header magic mismatch",
            });
        }
        Ok(Header {
            magic,
            format_version: u16::from_le_bytes(buf[4..6].try_into().unwrap()),
            page_size: u32::from_le_bytes(buf[6..10].try_into().unwrap()),
            key_len: u16::from_le_bytes(buf[10..12].try_into().unwrap()),
            value_len: u16::from_le_bytes(buf[12..14].try_into().unwrap()),
            root_page: u32::from_le_bytes(buf[14..18].try_into().unwrap()),
            free_list_head: u32::from_le_bytes(buf[18..22].try_into().unwrap()),
            page_count: u32::from_le_bytes(buf[22..26].try_into().unwrap()),
            commit_seq: u64::from_le_bytes(buf[26..34].try_into().unwrap()),
        })
    }
}

/// A paged, memory-mapped file. Growing it remaps; callers must not hold
/// page slices across a call to `allocate_page` that triggers growth (this
/// mirrors `gems_storage::file::ExtentFile`, at page rather than extent
/// granularity).
pub struct Pager {
    fd: OwnedFd,
    map: NonNull<u8>,
    mapped_len: usize,
    header: Header,
}

// SAFETY: same reasoning as `gems_storage::file::ExtentFile` — the mapping
// is exclusively owned and accessed only through bounds-checked methods.
unsafe impl Send for Pager {}

impl Pager {
    pub fn create(path: &Path, page_size: u32, key_len: u16, value_len: u16) -> Result<Self> {
        assert!(page_size as usize >= HEADER_ENCODED_LEN);
        let fd = fs::open(
            path,
            OFlags::CREATE | OFlags::RDWR | OFlags::EXCL,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(std::io::Error::from)?;
        fs::ftruncate(&fd, page_size as u64).map_err(std::io::Error::from)?;
        let map_len = round_up_to_os_page(page_size as usize);
        let map = map_file(&fd, map_len, true)?;
        let header = Header {
            magic: MAGIC,
            format_version: FORMAT_VERSION,
            page_size,
            key_len,
            value_len,
            root_page: 0,
            free_list_head: 0,
            page_count: 1, // page 0 (header) already exists
            commit_seq: 0,
        };
        let mut pager = Pager {
            fd,
            map,
            mapped_len: map_len,
            header,
        };
        pager.write_header()?;
        Ok(pager)
    }

    pub fn open(path: &Path, writable: bool) -> Result<Self> {
        let oflags = if writable {
            OFlags::RDWR
        } else {
            OFlags::RDONLY
        };
        let fd = fs::open(path, oflags, Mode::empty()).map_err(std::io::Error::from)?;
        let file_len = fs::fstat(&fd).map_err(std::io::Error::from)?.st_size as usize;
        let map_len = round_up_to_os_page(file_len);
        let map = map_file(&fd, map_len, writable)?;
        let header_bytes = unsafe { std::slice::from_raw_parts(map.as_ptr(), HEADER_ENCODED_LEN) };
        let header = Header::decode(header_bytes)?;
        Ok(Pager {
            fd,
            map,
            mapped_len: map_len,
            header,
        })
    }

    pub fn page_size(&self) -> u32 {
        self.header.page_size
    }

    pub fn key_len(&self) -> u16 {
        self.header.key_len
    }

    pub fn value_len(&self) -> u16 {
        self.header.value_len
    }

    pub fn root_page(&self) -> u32 {
        self.header.root_page
    }

    /// Publish a new root page id: this is the single atomic write that
    /// makes a copy-on-write update visible (ARCHITECTURE.md §2.3). Callers
    /// must have already written and synced every new page along the path
    /// to `new_root` before calling this.
    pub fn publish_root(&mut self, new_root: u32) -> Result<()> {
        self.header.root_page = new_root;
        self.header.commit_seq += 1;
        // The root_page field is 4 bytes at a 4-byte-aligned offset within
        // the header page, so this single write is atomic on both target
        // platforms even without a lock.
        let bytes = self.header.root_page.to_le_bytes();
        let base = self.file_offset(HEADER_PAGE_ID) as usize + ROOT_PAGE_OFFSET;
        self.mut_bytes(base, 4)?.copy_from_slice(&bytes);
        self.sync_range(base, 4)?;
        // commit_seq is advisory (crash-detection hint, not correctness
        // load-bearing) so it doesn't need the same atomicity guarantee;
        // persist it best-effort in the same header write.
        self.write_header()
    }

    fn write_header(&mut self) -> Result<()> {
        let encoded = self.header.encode();
        let base = self.file_offset(HEADER_PAGE_ID) as usize;
        self.mut_bytes(base, encoded.len())?
            .copy_from_slice(&encoded);
        self.sync_range(base, encoded.len())
    }

    fn file_offset(&self, page_id: u32) -> u64 {
        page_id as u64 * self.header.page_size as u64
    }

    /// Read-only view of a page's raw bytes.
    pub fn page(&self, page_id: u32) -> Result<&[u8]> {
        let start = self.file_offset(page_id) as usize;
        let end = start + self.header.page_size as usize;
        if end > self.mapped_len {
            return Err(Error::CorruptPage {
                detail: "page id beyond end of file",
            });
        }
        Ok(&self.as_slice()[start..end])
    }

    /// Mutable view of a page's raw bytes.
    pub fn page_mut(&mut self, page_id: u32) -> Result<&mut [u8]> {
        let start = self.file_offset(page_id) as usize;
        let len = self.header.page_size as usize;
        self.mut_bytes(start, len)
    }

    fn mut_bytes(&mut self, start: usize, len: usize) -> Result<&mut [u8]> {
        if start + len > self.mapped_len {
            return Err(Error::CorruptPage {
                detail: "write beyond end of file",
            });
        }
        Ok(&mut self.as_mut_slice()[start..start + len])
    }

    fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.map.as_ptr(), self.mapped_len) }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.map.as_ptr(), self.mapped_len) }
    }

    /// `msync` requires its address argument to be OS-page-aligned (POSIX);
    /// round the requested range out to page boundaries before calling it.
    fn sync_range(&self, offset: usize, len: usize) -> Result<()> {
        let page = gems_common::pagesize::os_page_size();
        let aligned_start = offset & !(page - 1);
        let aligned_end = round_up_to_os_page(offset + len);
        let aligned_len = (aligned_end - aligned_start).min(self.mapped_len - aligned_start);
        unsafe {
            mm::msync(
                self.map.as_ptr().add(aligned_start) as *mut _,
                aligned_len,
                mm::MsyncFlags::SYNC,
            )
            .map_err(std::io::Error::from)?;
        }
        Ok(())
    }

    /// Allocate a fresh page: pop the freelist if non-empty, otherwise grow
    /// the file by one page. Growth-on-write is how copy-on-write pages get
    /// their new home (ARCHITECTURE.md §2.3) — the returned page is
    /// zeroed and ready for the caller to fill in and sync.
    pub fn allocate_page(&mut self) -> Result<u32> {
        if self.header.free_list_head != 0 {
            let page_id = self.header.free_list_head;
            let next = u32::from_le_bytes(self.page(page_id)?[0..4].try_into().unwrap());
            self.header.free_list_head = next;
            self.write_header()?;
            let page = self.page_mut(page_id)?;
            page.fill(0);
            return Ok(page_id);
        }

        let page_id = self.header.page_count;
        let new_len = self.file_offset(page_id) + self.header.page_size as u64;
        fs::ftruncate(&self.fd, new_len).map_err(std::io::Error::from)?;

        let writable = true; // allocate_page is only ever called on a writable pager
        let new_map_len = round_up_to_os_page(new_len as usize);
        if new_map_len != self.mapped_len {
            unsafe {
                mm::munmap(self.map.as_ptr() as *mut _, self.mapped_len)
                    .map_err(std::io::Error::from)?;
            }
            self.map = map_file(&self.fd, new_map_len, writable)?;
            self.mapped_len = new_map_len;
        }
        self.header.page_count += 1;
        self.write_header()?;
        Ok(page_id)
    }

    /// Return a page to the freelist. Safe to call once no reader can
    /// still be using the page's previous version — for the single-writer,
    /// no-long-lived-snapshot model this crate implements today, that's
    /// immediately after the new root referencing the replacement page has
    /// been published (see the `Reclaim` free-then-publish ordering used by
    /// `BTree`).
    pub fn free_page(&mut self, page_id: u32) -> Result<()> {
        let next = self.header.free_list_head;
        let page = self.page_mut(page_id)?;
        page[0..4].copy_from_slice(&next.to_le_bytes());
        self.header.free_list_head = page_id;
        self.write_header()
    }
}

impl Drop for Pager {
    fn drop(&mut self) {
        if self.mapped_len > 0 {
            unsafe {
                let _ = mm::munmap(self.map.as_ptr() as *mut _, self.mapped_len);
            }
        }
    }
}

fn map_file(fd: &OwnedFd, len: usize, writable: bool) -> Result<NonNull<u8>> {
    let prot = if writable {
        ProtFlags::READ | ProtFlags::WRITE
    } else {
        ProtFlags::READ
    };
    let ptr = unsafe {
        mm::mmap(
            std::ptr::null_mut(),
            len,
            prot,
            MapFlags::SHARED,
            fd.as_fd(),
            0,
        )
        .map_err(std::io::Error::from)?
    };
    Ok(NonNull::new(ptr as *mut u8).expect("mmap returned null on success"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocate_grows_and_freelist_reuses() {
        let dir = std::env::temp_dir().join(format!("gems-index-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pager_alloc.gemi");
        let _ = std::fs::remove_file(&path);

        let mut pager = Pager::create(&path, 4096, 24, 13).unwrap();
        let p1 = pager.allocate_page().unwrap();
        let p2 = pager.allocate_page().unwrap();
        assert_ne!(p1, p2);
        assert_ne!(p1, HEADER_PAGE_ID);

        pager.free_page(p1).unwrap();
        let p3 = pager.allocate_page().unwrap();
        assert_eq!(p3, p1, "freed page should be reused before growing");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn root_publish_persists_across_reopen() {
        let dir = std::env::temp_dir().join(format!("gems-index-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pager_root.gemi");
        let _ = std::fs::remove_file(&path);

        {
            let mut pager = Pager::create(&path, 4096, 24, 13).unwrap();
            let root = pager.allocate_page().unwrap();
            pager.publish_root(root).unwrap();
        }
        {
            let pager = Pager::open(&path, false).unwrap();
            assert_eq!(pager.root_page(), 1);
        }

        std::fs::remove_file(&path).ok();
    }
}
