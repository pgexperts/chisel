// page_io.rs — Raw page I/O with two backings: file (durable) and memory
// (ephemeral).
//
// Architecture layer 2 (per ARCHITECTURE.md): the ONLY module in the engine that
// touches the filesystem directly. Every other layer (page_cache, freemap,
// data_page, handle_table, transaction) funnels its I/O through here, which
// keeps platform-specific syscalls (flock, fsync, set_len) confined to one
// place.
//
// Two backings, one interface:
// - `Backing::File` — the durable path. Owns a `File` handle for its entire
//   lifetime; the advisory flock is tied to that fd and released on drop.
//   Two fsyncs per commit; shadow paging guarantees crash consistency.
// - `Backing::Memory` — the ephemeral path. Pages live in a `Vec`; fsync is
//   a no-op; no flock is taken. Used for benchmark parity with SQLite
//   `:memory:` — see the in-memory-mode spec for the design rationale.
//
// Invariants common to both backings:
// - All reads and writes are page-aligned: offset = page_id * PAGE_SIZE.
//   Callers pass logical page IDs; this module never sees byte offsets.
// - Platform: macOS and Linux only. `libc::flock` is a BSD/Linux syscall;
//   Windows is not supported.
// - On-disk format is little-endian (see page.rs); this module is
//   format-agnostic and just moves fixed-size buffers.

use std::cell::Cell;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::error::{ChiselError, Result};
use crate::page::PAGE_SIZE;

// Why an enum rather than a trait object: benchmark integrity. A `dyn PageIo`
// adds a vtable call per page read/write — exactly the cost we want excluded
// when comparing Chisel to SQLite `:memory:`. An enum branch is predictable
// and effectively free once the variant is hot. See the in-memory-mode spec.
enum Backing {
    File { file: File },
    // Memory-backed database for benchmarking against SQLite :memory:.
    // `pages.len() * PAGE_SIZE` is the on-disk "file size" equivalent;
    // allocating a new page is a `Vec::push` of a zero-filled array.
    // No fsync, no flock, no recovery — see the in-memory-mode spec.
    Memory { pages: Vec<[u8; PAGE_SIZE]> },
}

pub struct PageIo {
    backing: Backing,
    // Tracked alongside the backing so every mutating path can fail-fast
    // with `ReadOnlyMode` rather than letting the kernel return EBADF
    // (which would surface as a generic, fatal `IoError`). The distinction
    // matters: a ReadOnlyMode error is operational — the caller just used
    // the wrong open mode — while a fatal IoError poisons the manager.
    read_only: bool,
    // Cumulative fsync count. Cell<u64> because `fsync(&self)` takes &self
    // (single-writer + same-thread reads — see project memory note
    // `project_chisel_single_client_design`). Read-only opens never fsync,
    // so this stays at 0 for the read-only lifetime — a useful invariant
    // when interpreting the counter.
    fsync_calls: Cell<u64>,
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
        Ok(PageIo {
            backing: Backing::File { file },
            read_only,
            fsync_calls: Cell::new(0),
        })
    }

    /// Open a fresh memory-backed database. Non-durable by design: dropping
    /// the returned `PageIo` discards all pages. Used for benchmark parity
    /// with SQLite `:memory:`; not intended for durable workloads.
    ///
    /// No `flock` is taken — a memory-backed database is single-client by
    /// virtue of being owned by a single `PageIo` value. Never fallible in
    /// the current implementation, but the `Result` return keeps the API
    /// symmetric with `open` and leaves room for future fallible init.
    pub fn open_in_memory() -> Result<PageIo> {
        Ok(PageIo {
            backing: Backing::Memory { pages: Vec::new() },
            read_only: false,
            fsync_calls: Cell::new(0),
        })
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
    /// "genuine disk I/O failure".
    ///
    /// Cost note: `page_count()` performs a `seek(End(0))` on the File
    /// backing, so every read pays one extra lseek syscall. `PageCache`
    /// absorbs that cost on cache hits (cache only calls `read_page` on
    /// a miss), so the per-operation overhead is bounded by the miss rate.
    /// Cached page_count would be strictly faster but would need
    /// invalidation on every `write_page` past EOF and every
    /// `set_page_count` — not worth the coupling for the v1 design.
    /// `&mut self` is required on this method precisely because of the
    /// seek.
    pub fn read_page(&mut self, page_id: u64) -> Result<[u8; PAGE_SIZE]> {
        let page_count = self.page_count()?;
        if page_id >= page_count {
            return Err(ChiselError::InvalidPageId { page_id });
        }
        match &mut self.backing {
            Backing::File { file } => {
                let offset = page_id * PAGE_SIZE as u64;
                file.seek(SeekFrom::Start(offset))?;
                let mut buf = [0u8; PAGE_SIZE];
                file.read_exact(&mut buf)?;
                Ok(buf)
            }
            Backing::Memory { pages } => Ok(pages[page_id as usize]),
        }
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
        match &mut self.backing {
            Backing::File { file } => {
                let offset = page_id * PAGE_SIZE as u64;
                file.seek(SeekFrom::Start(offset))?;
                file.write_all(buf)?;
                Ok(())
            }
            Backing::Memory { pages } => {
                // Match POSIX: writing past end extends, intermediate pages
                // are zero-filled. Shadow paging and PageCache::new_page
                // rely on this growth shape.
                let idx = page_id as usize;
                if idx >= pages.len() {
                    pages.resize(idx + 1, [0u8; PAGE_SIZE]);
                }
                pages[idx] = *buf;
                Ok(())
            }
        }
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
    /// new superblock into its inactive slot (slot index =
    /// `txn_counter % superblock_count`). Reversing or dropping either
    /// fsync breaks durability: a superblock that reaches the platter
    /// before its referenced data pages can point into garbage, and the
    /// crash-recovery path has no WAL to replay.
    ///
    /// Fsyncgate note (see I1 in ISSUES.md): a FAILED fsync cannot be
    /// safely retried on Linux — the dirty pages may have already been
    /// discarded from the kernel cache. The transaction manager treats
    /// any Err from this function as poison-worthy.
    pub fn fsync(&self) -> Result<()> {
        if self.read_only {
            return Err(ChiselError::ReadOnlyMode);
        }
        match &self.backing {
            Backing::File { file } => {
                file.sync_all()?;
            }
            // No durable storage to flush. The commit protocol still calls
            // fsync twice per commit; that overhead (two method calls and
            // two matches) is preserved for benchmark fidelity.
            Backing::Memory { .. } => {}
        }
        // Increment AFTER the operation succeeds. A failed fsync is fatal
        // (fsyncgate — see I1) and the manager will be poisoned, so the
        // counter going off-by-one on a poisoned engine is the least of
        // anyone's worries — but we don't want a successful retry (which
        // we do not allow) to be undercounted by a prior failure.
        self.fsync_calls.set(self.fsync_calls.get() + 1);
        Ok(())
    }

    /// Cumulative successful fsync calls since this `PageIo` was opened.
    /// Failed fsyncs are not counted (a failed fsync poisons the engine
    /// — see I1 — so the counter on a poisoned engine has no defined
    /// meaning beyond "at least this many succeeded").
    pub fn fsync_count(&self) -> u64 {
        self.fsync_calls.get()
    }

    /// Return the number of whole pages in the file.
    ///
    /// Uses `seek(End(0))` rather than `metadata()` because the former is
    /// guaranteed to reflect the current, post-write length even for files
    /// just extended via `write_all`. A partial trailing page (file length
    /// not a multiple of PAGE_SIZE) is silently floored — such a file is
    /// corrupt, but detecting it is the superblock layer's job.
    pub fn page_count(&mut self) -> Result<u64> {
        match &mut self.backing {
            Backing::File { file } => {
                let len = file.seek(SeekFrom::End(0))?;
                Ok(len / PAGE_SIZE as u64)
            }
            Backing::Memory { pages } => Ok(pages.len() as u64),
        }
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
        match &mut self.backing {
            Backing::File { file } => {
                file.set_len(n * PAGE_SIZE as u64)?;
                Ok(())
            }
            Backing::Memory { pages } => {
                pages.resize(n as usize, [0u8; PAGE_SIZE]);
                Ok(())
            }
        }
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

    #[test]
    fn fsync_count_increments_per_successful_fsync() {
        let f = seeded_file();
        let io = PageIo::open(f.path(), false).unwrap();
        assert_eq!(io.fsync_count(), 0);
        io.fsync().unwrap();
        assert_eq!(io.fsync_count(), 1);
        io.fsync().unwrap();
        assert_eq!(io.fsync_count(), 2);
    }

    // ── Migrated 2026-05-22 from tests/basic_ops.rs (I35 reshape) ──
    //
    // Folded into the file-backed `read_only_tests` mod since these tests
    // open a NamedTempFile-backed PageIo. The mod name predates the
    // migration; the additional tests below are not read-only-specific.

    #[test]
    fn test_page_io_write_and_read() {
        let file = NamedTempFile::new().unwrap();
        let mut io = PageIo::open(file.path(), false).unwrap();
        let mut buf = [0u8; PAGE_SIZE];
        buf[0] = 0xAB;
        buf[100] = 0xCD;
        io.write_page(0, &buf).unwrap();
        let read_buf = io.read_page(0).unwrap();
        assert_eq!(read_buf[0], 0xAB);
        assert_eq!(read_buf[100], 0xCD);
    }

    #[test]
    fn test_page_io_multiple_pages() {
        let file = NamedTempFile::new().unwrap();
        let mut io = PageIo::open(file.path(), false).unwrap();
        let mut buf1 = [0u8; PAGE_SIZE];
        let mut buf2 = [0u8; PAGE_SIZE];
        buf1[0] = 1;
        buf2[0] = 2;
        io.write_page(0, &buf1).unwrap();
        io.write_page(1, &buf2).unwrap();
        assert_eq!(io.read_page(0).unwrap()[0], 1);
        assert_eq!(io.read_page(1).unwrap()[0], 2);
    }

    #[test]
    fn test_page_io_fsync() {
        let file = NamedTempFile::new().unwrap();
        let mut io = PageIo::open(file.path(), false).unwrap();
        let buf = [0u8; PAGE_SIZE];
        io.write_page(0, &buf).unwrap();
        io.fsync().unwrap();
    }

    #[test]
    fn test_page_io_file_len() {
        let file = NamedTempFile::new().unwrap();
        let mut io = PageIo::open(file.path(), false).unwrap();
        assert_eq!(io.page_count().unwrap(), 0);
        let buf = [0u8; PAGE_SIZE];
        io.write_page(0, &buf).unwrap();
        assert_eq!(io.page_count().unwrap(), 1);
        io.write_page(2, &buf).unwrap();
        assert_eq!(io.page_count().unwrap(), 3);
    }
}

#[cfg(test)]
mod memory_backing_tests {
    use super::*;

    // Tests for the Memory variant of Backing, exercised through the public
    // PageIo surface. The File variant has its own coverage in the wider
    // test suite (integration tests open a NamedTempFile); these focus on
    // semantics specific to the in-memory backing — POSIX-parity sparse
    // writes, fsync as a no-op, and shrink/grow via set_page_count.

    #[test]
    fn memory_starts_with_zero_pages() {
        let mut io = PageIo::open_in_memory().unwrap();
        assert_eq!(io.page_count().unwrap(), 0);
    }

    #[test]
    fn memory_write_then_read_roundtrip() {
        let mut io = PageIo::open_in_memory().unwrap();
        let mut buf = [0u8; PAGE_SIZE];
        buf[0] = 0x42;
        buf[PAGE_SIZE - 1] = 0xFF;
        io.write_page(0, &buf).unwrap();
        let read = io.read_page(0).unwrap();
        assert_eq!(read, buf);
    }

    #[test]
    fn memory_write_extends_page_count() {
        let mut io = PageIo::open_in_memory().unwrap();
        io.write_page(0, &[0u8; PAGE_SIZE]).unwrap();
        io.write_page(1, &[0u8; PAGE_SIZE]).unwrap();
        io.write_page(2, &[0u8; PAGE_SIZE]).unwrap();
        assert_eq!(io.page_count().unwrap(), 3);
    }

    #[test]
    fn memory_write_beyond_end_grows_with_zero_fill() {
        // Writing to page 5 on an empty backing extends pages 0..=5.
        // Pages 0..=4 must be zero-filled; page 5 carries the written bytes.
        let mut io = PageIo::open_in_memory().unwrap();
        let mut buf = [0u8; PAGE_SIZE];
        buf[42] = 0xAB;
        io.write_page(5, &buf).unwrap();
        assert_eq!(io.page_count().unwrap(), 6);
        for p in 0..5 {
            assert_eq!(io.read_page(p).unwrap(), [0u8; PAGE_SIZE]);
        }
        assert_eq!(io.read_page(5).unwrap(), buf);
    }

    #[test]
    fn memory_read_out_of_range_is_invalid_page_id() {
        let mut io = PageIo::open_in_memory().unwrap();
        io.write_page(0, &[0u8; PAGE_SIZE]).unwrap();
        let err = io.read_page(1).unwrap_err();
        assert!(
            matches!(err, ChiselError::InvalidPageId { page_id: 1 }),
            "expected InvalidPageId {{ 1 }}, got {err:?}"
        );
    }

    #[test]
    fn memory_fsync_is_noop() {
        // fsync on a memory backing must return Ok(()) and leave every
        // observable property unchanged: page count, page contents, and
        // the result of subsequent reads. A wrong implementation that
        // accidentally mutates state would fail here.
        let mut io = PageIo::open_in_memory().unwrap();
        let mut marker = [0u8; PAGE_SIZE];
        marker[0] = 0xA5;
        marker[PAGE_SIZE - 1] = 0x5A;
        io.write_page(0, &marker).unwrap();
        io.write_page(1, &[0u8; PAGE_SIZE]).unwrap();

        let before_count = io.page_count().unwrap();
        io.fsync().unwrap();
        let after_count = io.page_count().unwrap();

        assert_eq!(before_count, after_count);
        assert_eq!(io.read_page(0).unwrap(), marker);
        assert_eq!(io.read_page(1).unwrap(), [0u8; PAGE_SIZE]);
    }

    #[test]
    fn memory_set_page_count_shrinks_and_grows() {
        let mut io = PageIo::open_in_memory().unwrap();
        io.write_page(0, &[1u8; PAGE_SIZE]).unwrap();
        io.write_page(1, &[2u8; PAGE_SIZE]).unwrap();
        io.write_page(2, &[3u8; PAGE_SIZE]).unwrap();
        io.set_page_count(1).unwrap();
        assert_eq!(io.page_count().unwrap(), 1);
        assert_eq!(io.read_page(0).unwrap(), [1u8; PAGE_SIZE]);
        io.set_page_count(4).unwrap();
        assert_eq!(io.page_count().unwrap(), 4);
        // Pages 1..=3 are freshly zero-filled after re-growth.
        for p in 1..4 {
            assert_eq!(io.read_page(p).unwrap(), [0u8; PAGE_SIZE]);
        }
    }

    #[test]
    fn fsync_count_in_memory_backing_also_increments() {
        // Memory backing's fsync is a no-op for durability but still counts:
        // benchmarks against in-memory PageIo should see commit-equivalent
        // counter behaviour.
        let io = PageIo::open_in_memory().unwrap();
        assert_eq!(io.fsync_count(), 0);
        io.fsync().unwrap();
        io.fsync().unwrap();
        assert_eq!(io.fsync_count(), 2);
    }
}
