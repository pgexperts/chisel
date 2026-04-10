// page_io.rs — Raw page-level file I/O with exclusive flock.
// All reads and writes are page-aligned (PAGE_SIZE multiples).
// This is the only module that touches the filesystem directly.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::error::{ChiselError, Result};
use crate::page::PAGE_SIZE;

pub struct PageIo {
    file: File,
}

impl PageIo {
    /// Open (or create) the database file and acquire an exclusive lock.
    /// If `read_only` is true, opens for reading only.
    pub fn open(path: &Path, read_only: bool) -> Result<PageIo> {
        let file = if read_only {
            OpenOptions::new().read(true).open(path)?
        } else {
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .open(path)?
        };
        Self::try_lock(&file)?;
        Ok(PageIo { file })
    }

    /// Acquire an exclusive advisory lock (flock). Returns LockFailed if
    /// another process holds it.
    fn try_lock(file: &File) -> Result<()> {
        use std::os::unix::io::AsRawFd;
        let fd = file.as_raw_fd();
        let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            return Err(ChiselError::LockFailed);
        }
        Ok(())
    }

    /// Read a single page by page ID. Returns the page contents.
    pub fn read_page(&mut self, page_id: u64) -> Result<[u8; PAGE_SIZE]> {
        let offset = page_id * PAGE_SIZE as u64;
        self.file.seek(SeekFrom::Start(offset))?;
        let mut buf = [0u8; PAGE_SIZE];
        self.file.read_exact(&mut buf)?;
        Ok(buf)
    }

    /// Write a single page by page ID.
    pub fn write_page(&mut self, page_id: u64, buf: &[u8; PAGE_SIZE]) -> Result<()> {
        let offset = page_id * PAGE_SIZE as u64;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(buf)?;
        Ok(())
    }

    /// Flush all writes to durable storage.
    pub fn fsync(&self) -> Result<()> {
        self.file.sync_all()?;
        Ok(())
    }

    /// Return the number of whole pages in the file.
    pub fn page_count(&mut self) -> Result<u64> {
        let len = self.file.seek(SeekFrom::End(0))?;
        Ok(len / PAGE_SIZE as u64)
    }

    /// Truncate (or extend) the file to exactly `n` pages.
    pub fn set_page_count(&mut self, n: u64) -> Result<()> {
        self.file.set_len(n * PAGE_SIZE as u64)?;
        Ok(())
    }
}
