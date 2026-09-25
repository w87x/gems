//! An extent file: opened and mapped entirely through `rustix`, no
//! `std::fs`/`std::io` in the write path. See ARCHITECTURE.md §1.1 and §1.4.

use gems_common::pagesize::round_up_to_os_page;
use gems_common::{Error, Result};
use rustix::fd::AsFd;
use rustix::fs::{self, Mode, OFlags};
use rustix::mm::{self, MapFlags, ProtFlags};
use std::os::fd::OwnedFd;
use std::path::Path;
use std::ptr::NonNull;

use crate::extent::EXTENT_SIZE;

/// An open, memory-mapped extent file. Growing the file remaps it; callers
/// must not hold slices derived from `as_slice`/`as_mut_slice` across a
/// call to `grow_by_one_extent`.
pub struct ExtentFile {
    fd: OwnedFd,
    map: NonNull<u8>,
    mapped_len: usize,
    writable: bool,
}

// SAFETY: the mapping is exclusively owned by this struct and all access
// goes through `&self`/`&mut self` methods that respect Rust's aliasing
// rules; the underlying memory is not thread-confined.
unsafe impl Send for ExtentFile {}

impl ExtentFile {
    pub fn create(path: &Path) -> Result<Self> {
        let fd = fs::open(
            path,
            OFlags::CREATE | OFlags::RDWR | OFlags::EXCL,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(std::io::Error::from)?;
        Self::from_fd(fd, true, 0)
    }

    pub fn open(path: &Path, writable: bool) -> Result<Self> {
        let oflags = if writable {
            OFlags::RDWR
        } else {
            OFlags::RDONLY
        };
        let fd = fs::open(path, oflags, Mode::empty()).map_err(std::io::Error::from)?;
        let len = fs::fstat(&fd).map_err(std::io::Error::from)?.st_size as usize;
        Self::from_fd(fd, writable, len)
    }

    fn from_fd(fd: OwnedFd, writable: bool, existing_len: usize) -> Result<Self> {
        let map_len = if existing_len == 0 {
            0
        } else {
            round_up_to_os_page(existing_len)
        };
        let map = if map_len == 0 {
            // Nothing to map yet; a zero-length mmap isn't valid, so defer
            // the actual mapping until the first `grow_by_one_extent`.
            NonNull::dangling()
        } else {
            map_file(&fd, map_len, writable)?
        };
        Ok(ExtentFile {
            fd,
            map,
            mapped_len: map_len,
            writable,
        })
    }

    /// Grow the file by exactly one extent (`EXTENT_SIZE` bytes) and remap.
    /// Returns the byte offset of the newly added extent.
    pub fn grow_by_one_extent(&mut self) -> Result<u64> {
        if !self.writable {
            return Err(Error::InvalidValue {
                detail: "cannot grow a read-only extent file",
            });
        }
        let old_len = self.mapped_len as u64;
        let new_len = old_len + EXTENT_SIZE;
        fs::ftruncate(&self.fd, new_len).map_err(std::io::Error::from)?;

        if self.mapped_len > 0 {
            // SAFETY: `self.map` was produced by a prior successful mmap of
            // `self.mapped_len` bytes over the same fd, and no outstanding
            // borrows of it can exist while we hold `&mut self`.
            unsafe {
                mm::munmap(self.map.as_ptr() as *mut _, self.mapped_len)
                    .map_err(std::io::Error::from)?;
            }
        }
        let new_map_len = round_up_to_os_page(new_len as usize);
        self.map = map_file(&self.fd, new_map_len, self.writable)?;
        self.mapped_len = new_map_len;
        Ok(old_len)
    }

    pub fn len(&self) -> usize {
        self.mapped_len
    }

    pub fn is_empty(&self) -> bool {
        self.mapped_len == 0
    }

    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `self.map` is valid for `self.mapped_len` bytes for the
        // lifetime of `self`.
        unsafe { std::slice::from_raw_parts(self.map.as_ptr(), self.mapped_len) }
    }

    pub fn as_mut_slice(&mut self) -> Result<&mut [u8]> {
        if !self.writable {
            return Err(Error::InvalidValue {
                detail: "extent file is not writable",
            });
        }
        // SAFETY: same validity as `as_slice`; `&mut self` guarantees no
        // other borrow of the mapping is live.
        Ok(unsafe { std::slice::from_raw_parts_mut(self.map.as_ptr(), self.mapped_len) })
    }

    /// Flush a byte range back to disk. Callers should call this after
    /// mutating extent headers/bitmaps or index pages before treating the
    /// write as durable (ARCHITECTURE.md §1.4).
    ///
    /// `msync` requires its address argument to be OS-page-aligned
    /// (POSIX), so the requested range is rounded out to page boundaries
    /// before the call.
    pub fn sync_range(&self, offset: usize, len: usize) -> Result<()> {
        let page = gems_common::pagesize::os_page_size();
        let aligned_start = offset & !(page - 1);
        let aligned_end = round_up_to_os_page(offset + len);
        let aligned_len = (aligned_end - aligned_start).min(self.mapped_len - aligned_start);
        // SAFETY: `aligned_start..aligned_start+aligned_len` lies within
        // the current mapping by construction above.
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
}

impl Drop for ExtentFile {
    fn drop(&mut self) {
        if self.mapped_len > 0 {
            // SAFETY: valid mapping owned exclusively by this struct.
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
    // SAFETY: `fd` is a valid, open file descriptor sized to at least
    // `len` bytes (callers ftruncate before mapping); the returned pointer
    // is used only through the bounds-checked wrapper methods above.
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
