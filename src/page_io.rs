// page_io.rs — Raw page-level file I/O with exclusive flock.
//
// Architecture layer 2 (per CLAUDE.md): the ONLY module in the engine that
// touches the filesystem directly. Every other layer (page_cache, freemap,
// data_page, handle_table, transaction) funnels its I/O through here, which
// keeps platform-specific syscalls (flock, fsync, set_len) confined to one
// place.
//
// Invariants:
// - All reads and writes are page-aligned: offset = page_id * PAGE_SIZE.
//   Callers pass logical page IDs; this module never sees byte offsets.
// - The struct owns the `File` handle for its entire lifetime. The advisory
//   lock is tied to that file descriptor, so `flock` is released implicitly
//   when `PageIo` (and therefore `File`) is dropped. There is no explicit
//   unlock path — correctness relies on Rust's drop semantics.
// - Platform: macOS and Linux only. `libc::flock` is a BSD/Linux syscall;
//   Windows is not supported.
// - On-disk format is little-endian (see page.rs); this module is
//   format-agnostic and just moves fixed-size buffers.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::error::{ChiselError, Result};
use crate::page::PAGE_SIZE;

pub struct PageIo {
    file: File,
    // Tracked alongside the File so every mutating path can fail-fast
    // with `ReadOnlyMode` rather than letting the kernel return EBADF
    // (which would surface as a generic, fatal `IoError`). The distinction
    // matters: a ReadOnlyMode error is operational — the caller just used
    // the wrong open mode — while a fatal IoError poisons the manager.
    read_only: bool,
}

impl PageIo {
    /// Open (or create) the database file and acquire an exclusive lock.
    /// If `read_only` is true, opens for reading only.
    ///
    /// `truncate(false)` is explicit and load-bearing: we must never zero an
    /// existing database on open. `create(true)` is only set in the
    /// read-write path, so read-only opens of a missing file correctly fail
    /// rather than materializing an empty file.
    ///
    /// The exclusive flock is taken even for `read_only` opens. This is
    /// intentional: even a reader needs to block concurrent writers, because
    /// shadow paging means a writer could be mid-commit (old superblock
    /// still authoritative, new pages being fsynced) and a naive reader
    /// could observe an inconsistent state. Single-process exclusive access
    /// is the v1 concurrency model.
    pub fn open(path: &Path, read_only: bool) -> Result<PageIo> {
        let file = if read_only {
            OpenOptions::new().read(true).open(path)?
        } else {
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(path)?
        };
        Self::try_lock(&file)?;
        Ok(PageIo { file, read_only })
    }

    /// True if this handle was opened read-only. Used by higher layers
    /// (e.g. `TransactionManager::begin`) to fail fast before touching
    /// any in-memory transaction state.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Acquire an exclusive advisory lock (flock). Returns LockFailed if
    /// another process holds it.
    ///
    /// `LOCK_NB` makes this non-blocking: if another process holds the lock
    /// we fail fast with `LockFailed` rather than hanging. This matches the
    /// "one writer per database file" model and gives callers a clean error
    /// to surface to the user.
    ///
    /// flock is advisory and per-open-file-description on Linux/macOS. The
    /// lock is released automatically when the underlying file descriptor is
    /// closed (i.e. when `PageIo` drops). We deliberately do NOT expose an
    /// explicit unlock — tying the lock's lifetime to the `File` guarantees
    /// we cannot leak locks on panic paths.
    fn try_lock(file: &File) -> Result<()> {
        use std::os::unix::io::AsRawFd;
        let fd = file.as_raw_fd();
        let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            return Err(ChiselError::LockFailed);
        }
        Ok(())
    }

    /// Read a single page by page ID. Returns the page contents by value.
    ///
    /// Returning `[u8; PAGE_SIZE]` by value (not a borrowed slice) is
    /// deliberate: `PageCache` will copy the bytes into its own `Box` and
    /// run checksum verification there. Keeping this layer buffer-free means
    /// callers never accidentally alias the underlying File.
    ///
    /// Reading an unallocated page is a bug in the caller, not a
    /// recoverable condition. ISSUES.md I16: we surface it as the typed
    /// `InvalidPageId` variant rather than the old generic
    /// `UnexpectedEof` wrapped as `IoError`, so upstream debugging can
    /// distinguish "caller asked for a page that doesn't exist" from
    /// "genuine disk I/O failure". The bounds check is cheap (one
    /// stat-less comparison against a cached length).
    pub fn read_page(&mut self, page_id: u64) -> Result<[u8; PAGE_SIZE]> {
        let page_count = self.page_count()?;
        if page_id >= page_count {
            return Err(ChiselError::InvalidPageId { page_id });
        }
        let offset = page_id * PAGE_SIZE as u64;
        self.file.seek(SeekFrom::Start(offset))?;
        let mut buf = [0u8; PAGE_SIZE];
        self.file.read_exact(&mut buf)?;
        Ok(buf)
    }

    /// Write a single page by page ID.
    ///
    /// If `page_id` is beyond the current end of file, the kernel extends
    /// the file to cover the write (standard POSIX behavior). This is how
    /// new pages allocated by `PageCache::new_page()` physically reach
    /// disk — we never explicitly `set_page_count()` when growing during
    /// normal operation.
    ///
    /// Note: this write is NOT durable until `fsync()` is called. The
    /// shadow-paging commit protocol relies on callers flushing all data
    /// pages with fsync BEFORE writing the superblock, and fsyncing AGAIN
    /// after the superblock. See transaction.rs::commit.
    pub fn write_page(&mut self, page_id: u64, buf: &[u8; PAGE_SIZE]) -> Result<()> {
        if self.read_only {
            return Err(ChiselError::ReadOnlyMode);
        }
        let offset = page_id * PAGE_SIZE as u64;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(buf)?;
        Ok(())
    }

    /// Flush all writes to durable storage.
    ///
    /// `sync_all` translates to `fsync` (Linux) or `fcntl(F_FULLFSYNC)` on
    /// macOS via Rust's stdlib. F_FULLFSYNC is important for crash safety
    /// on Apple hardware — a plain `fsync` on macOS does NOT flush the
    /// drive's own write cache. Rust's `sync_all` does the right thing.
    ///
    /// Ordering invariant: the transaction manager calls this twice per
    /// commit — once after writing all data pages, once after writing the
    /// superblock. Reversing or dropping either fsync breaks durability.
    pub fn fsync(&self) -> Result<()> {
        if self.read_only {
            return Err(ChiselError::ReadOnlyMode);
        }
        self.file.sync_all()?;
        Ok(())
    }

    /// Return the number of whole pages in the file.
    ///
    /// Uses `seek(End(0))` rather than `metadata()` because the former is
    /// guaranteed to reflect the current, post-write length even for files
    /// just extended via `write_all`. A partial trailing page (file length
    /// not a multiple of PAGE_SIZE) is silently floored — such a file is
    /// corrupt, but detecting it is the superblock layer's job.
    pub fn page_count(&mut self) -> Result<u64> {
        let len = self.file.seek(SeekFrom::End(0))?;
        Ok(len / PAGE_SIZE as u64)
    }

    /// Truncate (or extend) the file to exactly `n` pages.
    ///
    /// Used by defrag/truncate paths. Shrinking is destructive: pages at
    /// id >= n become unreadable immediately. Callers must ensure those
    /// pages are not referenced from any committed root before calling.
    pub fn set_page_count(&mut self, n: u64) -> Result<()> {
        if self.read_only {
            return Err(ChiselError::ReadOnlyMode);
        }
        self.file.set_len(n * PAGE_SIZE as u64)?;
        Ok(())
    }
}

#[cfg(test)]
mod read_only_tests {
    use super::*;
    use tempfile::NamedTempFile;

    // Defense-in-depth regression tests. The upper layer
    // (`TransactionManager::begin`) fails fast on a read-only handle,
    // so in normal operation these guards are never reached — but they
    // exist precisely so a hypothetical new caller that reached for
    // `PageIo` directly cannot silently scribble on a read-only file.
    //
    // Each test exercises exactly one mutating entry point so that a
    // future refactor which removes one of the three guards will fail
    // its corresponding test specifically, rather than being masked by
    // the `begin()` check at the transaction layer.

    /// Create a seeded file (one page of zeros) that the read-only
    /// opens below can then exercise without tripping the "zero length
    /// ⇒ create_new" fallback.
    fn seeded_file() -> NamedTempFile {
        let f = NamedTempFile::new().unwrap();
        // Write one page of zeros via a write-capable PageIo, then drop
        // it to release the flock.
        {
            let mut io = PageIo::open(f.path(), false).unwrap();
            io.write_page(0, &[0u8; PAGE_SIZE]).unwrap();
            io.fsync().unwrap();
        }
        f
    }

    #[test]
    fn write_page_on_read_only_returns_read_only_mode() {
        let f = seeded_file();
        let mut io = PageIo::open(f.path(), true).unwrap();
        let err = io.write_page(0, &[0u8; PAGE_SIZE]).unwrap_err();
        assert!(
            matches!(err, ChiselError::ReadOnlyMode),
            "expected ReadOnlyMode, got {err:?}"
        );
    }

    #[test]
    fn fsync_on_read_only_returns_read_only_mode() {
        let f = seeded_file();
        let io = PageIo::open(f.path(), true).unwrap();
        let err = io.fsync().unwrap_err();
        assert!(
            matches!(err, ChiselError::ReadOnlyMode),
            "expected ReadOnlyMode, got {err:?}"
        );
    }

    #[test]
    fn set_page_count_on_read_only_returns_read_only_mode() {
        let f = seeded_file();
        let mut io = PageIo::open(f.path(), true).unwrap();
        let err = io.set_page_count(0).unwrap_err();
        assert!(
            matches!(err, ChiselError::ReadOnlyMode),
            "expected ReadOnlyMode, got {err:?}"
        );
    }

    #[test]
    fn read_page_on_read_only_succeeds() {
        // Sanity: the guards are per-mutator, not a blanket refusal.
        // Opening read-only must still permit reads.
        let f = seeded_file();
        let mut io = PageIo::open(f.path(), true).unwrap();
        let buf = io.read_page(0).unwrap();
        assert_eq!(buf, [0u8; PAGE_SIZE]);
    }
}
