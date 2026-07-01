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
// - `Backing::Memory` — the ephemeral path. Pages live in a flat `Vec<u8>`
//   addressed by `page_id * stride`; fsync is a no-op; no flock is taken.
//   Used for benchmark parity with SQLite `:memory:` — see the in-memory-mode
//   spec for the design rationale.
//
// Stride: the on-disk unit size. For plaintext DBs stride == PAGE_SIZE (8192).
// For encrypted DBs stride == ENC_PAGE_SIZE (8232 = 8192 ct + 16 tag + 24 nonce).
// This module is crypto-agnostic: it only moves `stride`-byte blobs; the seal/
// open transform happens one layer up in PageCache (Task 3.3). Offset math is
// always `page_id * stride`. The engine calls `set_stride(ENC_PAGE_SIZE)` right
// after reading page 0's plaintext bootstrap header on an encrypted open, before
// any other page is read. The default at open is PAGE_SIZE, which is always
// correct for the initial page-0 read regardless of encryption.
//
// Invariants common to both backings:
// - All reads and writes are unit-aligned: offset = page_id * stride. The
//   stride defaults to PAGE_SIZE and is set to ENC_PAGE_SIZE for encrypted
//   DBs after page 0 is read. Callers pass logical page IDs only.
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
    // Flat byte vec addressed by `page_id * stride`; stride-variable so
    // encrypted (8232-byte) and plaintext (8192-byte) units both work.
    // No fsync, no flock, no recovery — see the in-memory-mode spec.
    Memory { bytes: Vec<u8> },
}

/// Test-only fault plan armed via `PageIo::arm_fault` (I112). `Copy` so it
/// lives in a `Cell` (the `io::Error` is synthesized at the failure site, not
/// stored). One fault is armed at a time; faults are one-shot except the fsync
/// countdown, which decrements on each fsync until it fires.
#[cfg(test)]
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Fault {
    #[default]
    None,
    /// Fail an fsync via a countdown: `FailFsync(0)` fails the NEXT fsync;
    /// `FailFsync(n)` lets `n` fsyncs succeed (decrementing each) then fails the
    /// one after. Lets a test target a specific fsync in commit's three-fsync
    /// protocol (pre-drain / data-flush / superblock).
    FailFsync(u32),
    /// Fail `write_page` for exactly this page id (one-shot).
    FailWritePage(u64),
    /// Fail `read_page` for exactly this page id (one-shot).
    FailReadPage(u64),
}

pub struct PageIo {
    backing: Backing,
    // Tracked alongside the backing so every mutating path can fail-fast
    // with `ReadOnlyMode` rather than letting the kernel return EBADF
    // (which would surface as a generic, fatal `IoError`). The distinction
    // matters: a ReadOnlyMode error is operational — the caller just used
    // the wrong open mode — while a fatal IoError poisons the manager.
    read_only: bool,
    // On-disk unit size in bytes. PAGE_SIZE for a plaintext DB;
    // ENC_PAGE_SIZE (8232 = 8192 ct + 16 tag + 24 nonce) for an encrypted
    // DB. This module is crypto-agnostic: it only moves `stride`-byte blobs
    // and computes offset = page_id * stride. The engine calls set_stride to
    // ENC_PAGE_SIZE immediately after reading page 0's plaintext bootstrap
    // header on an encrypted open, before any other page is read.
    // page_count() is always reported in stride-units.
    stride: usize,
    // Cumulative fsync count. Cell<u64> because `fsync(&self)` takes &self
    // (single-writer + same-thread reads — see project memory note
    // `project_chisel_single_client_design`). Read-only opens never fsync,
    // so this stays at 0 for the read-only lifetime — a useful invariant
    // when interpreting the counter.
    fsync_calls: Cell<u64>,
    // I51 (ISSUES.md, 2026-05-22): cached file length in stride-units.
    // Eliminates the `lseek(End(0))` syscall that every read_page() used to
    // issue through `page_count()`. Maintained by:
    //   - `open()` / `open_in_memory()` — seed from initial file length
    //   - `write_page_unit()` — extend if page_id+1 > cached value
    //   - `set_page_count(n)` — overwrite to n (both grow and shrink)
    //   - `set_stride(s)` — re-seeds from true file length in new units
    // Safe under the single-writer flock contract: no other process can
    // mutate the file behind our back, so the cached value never goes
    // stale.
    cached_page_count: Cell<u64>,
    // I112: test-only fault injector. Checked at the top of read_page/
    // write_page/fsync; cfg(test) so it is compiled out of production builds
    // entirely (same discipline as transaction.rs::fail_next_membership_op).
    #[cfg(test)]
    fault: Cell<Fault>,
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
        let mut file = if read_only {
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
        // I51: seed the page-count cache from the current file length.
        // After this, every page_count() call returns the cached value
        // without a syscall; the cache is kept in sync by write_page()
        // and set_page_count().
        let initial_len = file.seek(SeekFrom::End(0))?;
        // Seed with PAGE_SIZE (the default stride). set_stride() is called
        // by the engine immediately after reading the encrypted superblock on
        // page 0, at which point it re-seeds the count in ENC_PAGE_SIZE units.
        let initial_page_count = initial_len / PAGE_SIZE as u64;
        Ok(PageIo {
            backing: Backing::File { file },
            read_only,
            stride: PAGE_SIZE,
            fsync_calls: Cell::new(0),
            cached_page_count: Cell::new(initial_page_count),
            #[cfg(test)]
            fault: Cell::new(Fault::None),
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
            backing: Backing::Memory { bytes: Vec::new() },
            read_only: false,
            stride: PAGE_SIZE,
            fsync_calls: Cell::new(0),
            // I51: seeded to 0; write_page_unit() and set_page_count() keep
            // it in sync with bytes.len() / stride as the Vec grows or shrinks.
            cached_page_count: Cell::new(0),
            #[cfg(test)]
            fault: Cell::new(Fault::None),
        })
    }

    /// True if this handle was opened read-only. Used by higher layers
    /// (e.g. `TransactionManager::begin`) to fail fast before touching
    /// any in-memory transaction state.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Force this handle read-only after open. Used by the I29 format-MINOR
    /// write-gate: a file whose MINOR exceeds this binary's may be READ (within
    /// a MAJOR all layout changes are additive, so known fields are at stable
    /// offsets) but must not be WRITTEN, since this binary would stamp pages at
    /// its older minor and drop fields it cannot see. Idempotent. The OS file
    /// handle is unchanged (still O_RDWR) — this only flips the in-memory guard
    /// that `write_page` / `fsync` / `set_page_count` already honor. See I29.
    pub fn force_read_only(&mut self) {
        self.read_only = true;
    }

    /// On-disk unit size in bytes (PAGE_SIZE plaintext, ENC_PAGE_SIZE encrypted).
    pub fn stride(&self) -> usize {
        self.stride
    }

    /// Set the on-disk stride and re-seed the page-count cache against the new
    /// unit size. Must be called BEFORE the first unit read on an encrypted DB.
    /// The engine does this immediately after the superblock initialization so
    /// data-page I/O uses the 8232-byte encrypted stride. Re-seeds from the
    /// true file length so page_count() is reported in the new stride-units.
    pub fn set_stride(&mut self, stride: usize) {
        self.stride = stride;
        let len = match &mut self.backing {
            Backing::File { file } => file.seek(SeekFrom::End(0)).unwrap_or(0),
            Backing::Memory { bytes } => bytes.len() as u64,
        };
        self.cached_page_count.set(len / stride as u64);
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
        // SAFETY:
        //   * `fd` is valid for the duration of this call: we hold a borrow
        //     of `&File`, so the descriptor cannot be closed concurrently.
        //   * `LOCK_EX | LOCK_NB` is a fixed bitflag combination that
        //     flock(2) accepts on every supported platform (Linux, macOS).
        //   * Return contract: 0 on success, -1 on failure with errno set.
        //     We don't read errno — `LockFailed` is sufficient diagnostic
        //     for the "someone else holds the lock" case, the only failure
        //     mode in practice for a path we can open. EINVAL / EBADF would
        //     indicate a programming error and would surface as the same
        //     LockFailed return; that's acceptable because the next user
        //     action will fail with a more specific error.
        //   * No resources are leaked. The lock is released when the
        //     underlying fd is closed, which happens when the `PageIo`'s
        //     `Drop` runs (the `File` is owned by `PageIo` and dropped
        //     with it). We deliberately don't expose an explicit unlock —
        //     tying lock lifetime to the `File` guarantees we cannot leak
        //     locks on panic paths.
        let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            return Err(ChiselError::LockFailed);
        }
        Ok(())
    }

    /// Read the raw on-disk unit (`stride` bytes) for `page_id`.
    ///
    /// Crypto-agnostic: for a plaintext DB the returned blob IS the page
    /// image (stride == PAGE_SIZE). For an encrypted DB the blob is the
    /// sealed ciphertext‖tag‖nonce, which PageCache hands to PageCipher::open.
    ///
    /// Reading an unallocated page is a caller bug. I16: surfaced as the
    /// typed `InvalidPageId` rather than a generic `IoError` so that
    /// upstream debugging can distinguish "wrong page id" from "disk I/O
    /// failure".
    ///
    /// `&mut self` is required because the File branch does `file.seek` +
    /// `read_exact` — side-effectful operations on the File handle. The
    /// bounds check is effectively free post-I51 (cached page count).
    pub fn read_page_unit(&mut self, page_id: u64) -> Result<Vec<u8>> {
        let page_count = self.page_count()?;
        if page_id >= page_count {
            return Err(ChiselError::InvalidPageId { page_id });
        }
        #[cfg(test)]
        if self.fault.get() == Fault::FailReadPage(page_id) {
            self.fault.set(Fault::None);
            return Err(ChiselError::IoError(std::io::Error::other(
                "fault-injected read failure",
            )));
        }
        let stride = self.stride;
        match &mut self.backing {
            Backing::File { file } => {
                let offset = page_id * stride as u64;
                file.seek(SeekFrom::Start(offset))?;
                let mut buf = vec![0u8; stride];
                file.read_exact(&mut buf)?;
                Ok(buf)
            }
            // Sound: the `page_id >= page_count` guard above already rejected
            // out-of-range ids, and the cached page count equals
            // bytes.len() / stride (maintained by write_page_unit and
            // set_page_count). A passing check guarantees the slice exists.
            Backing::Memory { bytes } => {
                let off = (page_id * stride as u64) as usize;
                Ok(bytes[off..off + stride].to_vec())
            }
        }
    }

    /// Read the raw on-disk unit into a caller-provided buffer.
    ///
    /// `buf` must be exactly `stride` bytes. Avoids the heap allocation of
    /// `read_page_unit`; used by PageCache::load_page with a stack-allocated
    /// `[u8; ENC_PAGE_SIZE]` buffer so the cold-load hot path allocates nothing.
    ///
    /// Shares all validation (bounds check, fault injection) with `read_page_unit`.
    pub fn read_page_unit_into(&mut self, page_id: u64, buf: &mut [u8]) -> Result<()> {
        let page_count = self.page_count()?;
        if page_id >= page_count {
            return Err(ChiselError::InvalidPageId { page_id });
        }
        #[cfg(test)]
        if self.fault.get() == Fault::FailReadPage(page_id) {
            self.fault.set(Fault::None);
            return Err(ChiselError::IoError(std::io::Error::other(
                "fault-injected read failure",
            )));
        }
        let stride = self.stride;
        debug_assert_eq!(
            buf.len(),
            stride,
            "read_page_unit_into: buf.len() {} != stride {}",
            buf.len(),
            stride
        );
        match &mut self.backing {
            Backing::File { file } => {
                let offset = page_id * stride as u64;
                file.seek(SeekFrom::Start(offset))?;
                file.read_exact(buf)?;
            }
            Backing::Memory { bytes } => {
                let off = (page_id * stride as u64) as usize;
                buf.copy_from_slice(&bytes[off..off + stride]);
            }
        }
        Ok(())
    }

    /// Write a raw on-disk unit (must be exactly `stride` bytes) for `page_id`.
    ///
    /// Past-EOF writes extend the file; intermediate units are zero-filled
    /// (POSIX behavior, matching how new pages reach disk via new_page()).
    /// The blob length must equal `stride` — a mismatch is a caller bug
    /// returned as an `IoError` rather than silently truncating/padding.
    ///
    /// Note: not durable until `fsync()` is called. See the shadow-paging
    /// commit protocol in transaction.rs.
    pub fn write_page_unit(&mut self, page_id: u64, blob: &[u8]) -> Result<()> {
        if self.read_only {
            return Err(ChiselError::ReadOnlyMode);
        }
        if blob.len() != self.stride {
            return Err(ChiselError::IoError(std::io::Error::other(format!(
                "page unit length {} != stride {}",
                blob.len(),
                self.stride
            ))));
        }
        #[cfg(test)]
        if self.fault.get() == Fault::FailWritePage(page_id) {
            self.fault.set(Fault::None);
            return Err(ChiselError::IoError(std::io::Error::other(
                "fault-injected write failure",
            )));
        }
        let stride = self.stride;
        match &mut self.backing {
            Backing::File { file } => {
                let offset = page_id * stride as u64;
                file.seek(SeekFrom::Start(offset))?;
                file.write_all(blob)?;
            }
            Backing::Memory { bytes } => {
                // Match POSIX: writing past end extends, intermediate units
                // are zero-filled. Shadow paging and PageCache::new_page
                // rely on this growth shape.
                let off = (page_id * stride as u64) as usize;
                let needed = off + stride;
                if bytes.len() < needed {
                    bytes.resize(needed, 0);
                }
                bytes[off..off + stride].copy_from_slice(blob);
            }
        }
        // I51: maintain the page-count cache. Writing past the current
        // end extends the file (POSIX behavior); intra-cache writes
        // don't change it.
        let needed = page_id + 1;
        if needed > self.cached_page_count.get() {
            self.cached_page_count.set(needed);
        }
        Ok(())
    }

    /// Read a single plaintext page by page ID. Returns the page contents
    /// by value.
    ///
    /// Only valid when `stride == PAGE_SIZE` (the plaintext path). Encrypted
    /// DBs go through `read_page_unit` directly from PageCache. Returning
    /// `[u8; PAGE_SIZE]` by value keeps the layer buffer-free so callers
    /// never accidentally alias the underlying File.
    pub fn read_page(&mut self, page_id: u64) -> Result<[u8; PAGE_SIZE]> {
        debug_assert_eq!(
            self.stride, PAGE_SIZE,
            "read_page called on an encrypted stride; use read_page_unit"
        );
        let blob = self.read_page_unit(page_id)?;
        let mut buf = [0u8; PAGE_SIZE];
        buf.copy_from_slice(&blob);
        Ok(buf)
    }

    /// Write a single plaintext page by page ID.
    ///
    /// Only valid when `stride == PAGE_SIZE` (the plaintext path). See
    /// `write_page_unit` for the encrypted path. Not durable until `fsync()`.
    pub fn write_page(&mut self, page_id: u64, buf: &[u8; PAGE_SIZE]) -> Result<()> {
        debug_assert_eq!(
            self.stride, PAGE_SIZE,
            "write_page called on an encrypted stride; use write_page_unit"
        );
        self.write_page_unit(page_id, buf)
    }

    /// Flush all writes to durable storage.
    ///
    /// `sync_all` translates to `fsync` (Linux) or `fcntl(F_FULLFSYNC)` on
    /// macOS via Rust's stdlib. F_FULLFSYNC is important for crash safety
    /// on Apple hardware — a plain `fsync` on macOS does NOT flush the
    /// drive's own write cache. Rust's `sync_all` does the right thing.
    ///
    /// Ordering invariant: the transaction manager calls this three times
    /// per commit — the I28 pre-drain flush, then after writing all data
    /// pages, then after writing the
    /// new superblock into its inactive slot (slot index =
    /// `txn_counter % superblock_count`). Reversing or dropping the data or
    /// superblock fsync breaks durability: a superblock that reaches the platter
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
        #[cfg(test)]
        match self.fault.get() {
            Fault::FailFsync(0) => {
                self.fault.set(Fault::None);
                return Err(ChiselError::IoError(std::io::Error::other(
                    "fault-injected fsync failure",
                )));
            }
            Fault::FailFsync(n) => self.fault.set(Fault::FailFsync(n - 1)),
            _ => {}
        }
        match &self.backing {
            Backing::File { file } => {
                file.sync_all()?;
            }
            // No durable storage to flush. The commit protocol still calls
            // fsync three times per commit; that overhead (three method calls
            // and three matches) is preserved for benchmark fidelity.
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

    /// Arm a test fault (I112). The next matching I/O op returns `IoError`.
    #[cfg(test)]
    pub(crate) fn arm_fault(&self, f: Fault) {
        self.fault.set(f);
    }

    /// Return the number of whole stride-units (pages) in the file.
    ///
    /// I51 (2026-05-22): returns the cached value in stride-units. The cache
    /// is seeded at `open()` from the initial file length divided by PAGE_SIZE
    /// (the default stride), and re-seeded by `set_stride()` when the stride
    /// changes. Maintained by `write_page_unit()` (extend on writes past EOF)
    /// and `set_page_count()` (resync on truncate/grow). Single-writer flock +
    /// private-process ownership of the file makes the cache always coherent.
    ///
    /// I123 (ISSUES.md, 2026-06-21): takes `&self` — pure Cell read, no
    /// syscall. Dropping `&mut` removes the latent double-borrow risk.
    pub fn page_count(&self) -> Result<u64> {
        Ok(self.cached_page_count.get())
    }

    /// Truncate (or extend) the file to exactly `n` stride-units (pages).
    ///
    /// File length is `n * stride` bytes. Used by defrag/truncate paths.
    /// Shrinking is destructive: pages at id >= n become unreadable
    /// immediately. Callers must ensure those pages are not referenced
    /// from any committed root before calling.
    pub fn set_page_count(&mut self, n: u64) -> Result<()> {
        if self.read_only {
            return Err(ChiselError::ReadOnlyMode);
        }
        let stride = self.stride;
        match &mut self.backing {
            Backing::File { file } => {
                file.set_len(n * stride as u64)?;
            }
            Backing::Memory { bytes } => {
                bytes.resize((n * stride as u64) as usize, 0);
            }
        }
        // I51: resync the page-count cache to the authoritative new
        // length. Unlike write_page_unit (which only grows), set_page_count
        // can shrink too — overwrite the cache rather than max(cache, n).
        self.cached_page_count.set(n);
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

    #[test]
    fn force_read_only_blocks_writes_after_a_read_write_open() {
        let tmp = NamedTempFile::new().unwrap();
        let mut io = PageIo::open(tmp.path(), false).unwrap(); // opened read-WRITE
        assert!(!io.is_read_only());

        io.force_read_only();

        assert!(io.is_read_only());
        let buf = [0u8; PAGE_SIZE];
        assert!(matches!(
            io.write_page(0, &buf),
            Err(ChiselError::ReadOnlyMode)
        ));
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

    // ── I51 page-count cache regressions (file-backed half) ───────
    //
    // Companion to the memory-backed cache tests in
    // `memory_backing_tests` below. These exercise the on-disk seed
    // path (open reads the file length once and caches it) and the
    // drop+reopen flow (cache is rebuilt from the on-disk length on
    // the second open, not carried over in memory).

    #[test]
    fn page_count_cache_seeded_from_file_length_on_open() {
        // The whole point of the cache: on a freshly-opened file with
        // N pages already on disk, page_count() must return N without
        // any maintenance call from the test. Without the seed, the
        // cache would be 0 and read_page bounds checks would fail.
        let f = seeded_file();
        let mut io = PageIo::open(f.path(), false).unwrap();
        // seeded_file wrote one page; cache should be 1 immediately.
        assert_eq!(io.page_count().unwrap(), 1);
        // And read_page(0) — which uses the cached page_count for its
        // bounds check — must succeed without tripping InvalidPageId.
        assert_eq!(io.read_page(0).unwrap(), [0u8; PAGE_SIZE]);
    }

    #[test]
    fn page_count_cache_survives_drop_and_reopen() {
        // Persistence proof: write three pages, drop the PageIo
        // (releasing the flock), reopen the same path, the cache
        // seed picks up the on-disk length.
        let f = NamedTempFile::new().unwrap();
        {
            let mut io = PageIo::open(f.path(), false).unwrap();
            io.write_page(0, &[0u8; PAGE_SIZE]).unwrap();
            io.write_page(1, &[0u8; PAGE_SIZE]).unwrap();
            io.write_page(2, &[0u8; PAGE_SIZE]).unwrap();
            assert_eq!(io.page_count().unwrap(), 3);
            // io drops here, releasing the flock.
        }
        let io = PageIo::open(f.path(), false).unwrap();
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
        let io = PageIo::open_in_memory().unwrap();
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

    // ── I51 page-count cache regressions ───────────────────────────
    //
    // Pre-I51 every PageIo::read_page() called page_count() which did
    // a seek(End(0)) syscall to ask the kernel for the file length.
    // Post-I51 page_count() returns a cached value seeded at open()
    // and maintained by write_page() / set_page_count(). The tests
    // below pin the cache-coherence contract: any operation that
    // changes the file's logical page count must update the cache.

    #[test]
    fn page_count_cache_extends_on_write_past_eof() {
        // Fresh memory PageIo starts at 0; writing page 4 extends to 5.
        let mut io = PageIo::open_in_memory().unwrap();
        assert_eq!(io.page_count().unwrap(), 0);
        io.write_page(4, &[0u8; PAGE_SIZE]).unwrap();
        assert_eq!(io.page_count().unwrap(), 5);
    }

    #[test]
    fn page_count_cache_does_not_shrink_on_intra_cache_write() {
        // After write_page(5), the cache is 6. An idempotent write to
        // page 2 (already in-range) must NOT shrink the cache to 3.
        // The pre-fix would have had no cache at all and called seek
        // each time, which couldn't shrink either; the test pins
        // that the new write_page logic gets the same answer.
        let mut io = PageIo::open_in_memory().unwrap();
        io.write_page(5, &[0u8; PAGE_SIZE]).unwrap();
        assert_eq!(io.page_count().unwrap(), 6);
        io.write_page(2, &[0u8; PAGE_SIZE]).unwrap();
        assert_eq!(io.page_count().unwrap(), 6);
    }

    #[test]
    fn page_count_cache_tracks_set_page_count_both_directions() {
        // set_page_count is the only operation that can SHRINK the
        // cache. write_page can only grow it. Round-trip a few sizes
        // to verify both directions.
        let mut io = PageIo::open_in_memory().unwrap();
        io.set_page_count(10).unwrap();
        assert_eq!(io.page_count().unwrap(), 10);
        io.set_page_count(3).unwrap();
        assert_eq!(io.page_count().unwrap(), 3);
        io.set_page_count(7).unwrap();
        assert_eq!(io.page_count().unwrap(), 7);
    }

    // The remaining two I51 tests are file-backed and live in
    // `read_only_tests` below (which already imports NamedTempFile
    // and defines the `seeded_file` helper).
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn armed_fsync_fault_returns_io_error_once() {
        let io = PageIo::open_in_memory().unwrap();
        io.arm_fault(Fault::FailFsync(0));
        assert!(matches!(io.fsync(), Err(ChiselError::IoError(_))));
        assert!(io.fsync().is_ok(), "one-shot: next fsync succeeds");
    }

    #[test]
    fn armed_fsync_countdown_fires_on_the_nth() {
        let io = PageIo::open_in_memory().unwrap();
        io.arm_fault(Fault::FailFsync(2));
        assert!(io.fsync().is_ok(), "1st succeeds (2 -> 1)");
        assert!(io.fsync().is_ok(), "2nd succeeds (1 -> 0)");
        assert!(
            matches!(io.fsync(), Err(ChiselError::IoError(_))),
            "3rd fires"
        );
    }

    #[test]
    fn armed_write_fault_targets_one_page_then_clears() {
        let mut io = PageIo::open_in_memory().unwrap();
        let buf = [0u8; PAGE_SIZE];
        io.write_page(0, &buf).unwrap();
        io.arm_fault(Fault::FailWritePage(0));
        assert!(matches!(
            io.write_page(0, &buf),
            Err(ChiselError::IoError(_))
        ));
        assert!(io.write_page(0, &buf).is_ok(), "one-shot cleared");
    }

    #[test]
    fn armed_read_fault_targets_one_page_then_clears() {
        let mut io = PageIo::open_in_memory().unwrap();
        let buf = [0u8; PAGE_SIZE];
        io.write_page(0, &buf).unwrap();
        io.arm_fault(Fault::FailReadPage(0));
        assert!(matches!(io.read_page(0), Err(ChiselError::IoError(_))));
        assert!(io.read_page(0).is_ok(), "one-shot cleared");
    }
}

#[cfg(test)]
mod stride_tests {
    use super::*;
    use crate::crypto::ENC_PAGE_SIZE;

    // Offset math must use the on-disk stride, not PAGE_SIZE. With an
    // 8232-byte stride, page 2's blob lives at byte 16464, and page_count
    // is reported in stride-units. In-memory backing so the test is
    // filesystem-free.
    #[test]
    fn stride_8232_offsets_and_unit_roundtrip() {
        let mut io = PageIo::open_in_memory().unwrap();
        io.set_stride(ENC_PAGE_SIZE);
        assert_eq!(io.stride(), ENC_PAGE_SIZE);

        // Distinct 8232-byte blobs per page id.
        let mut blob0 = vec![0u8; ENC_PAGE_SIZE];
        blob0[0] = 0xA0;
        blob0[ENC_PAGE_SIZE - 1] = 0x0A;
        let mut blob2 = vec![0u8; ENC_PAGE_SIZE];
        blob2[0] = 0xC2;
        blob2[ENC_PAGE_SIZE - 1] = 0x2C;

        io.write_page_unit(0, &blob0).unwrap();
        io.write_page_unit(2, &blob2).unwrap(); // page 1 zero-filled by growth

        // page_count is in stride-units: writing page 2 extends to 3.
        assert_eq!(io.page_count().unwrap(), 3);

        assert_eq!(io.read_page_unit(0).unwrap(), blob0);
        assert_eq!(io.read_page_unit(2).unwrap(), blob2);
        // The zero-filled gap page reads back as all zeros.
        assert_eq!(io.read_page_unit(1).unwrap(), vec![0u8; ENC_PAGE_SIZE]);
    }

    // The plaintext stride (default) keeps PAGE_SIZE offset math intact.
    #[test]
    fn default_stride_is_page_size() {
        let io = PageIo::open_in_memory().unwrap();
        assert_eq!(io.stride(), PAGE_SIZE);
    }

    // A blob whose length != stride is a caller bug, not silent truncation.
    #[test]
    fn write_unit_wrong_length_is_invalid() {
        let mut io = PageIo::open_in_memory().unwrap();
        io.set_stride(ENC_PAGE_SIZE);
        let short = vec![0u8; PAGE_SIZE]; // wrong: 8192 != 8232
        assert!(io.write_page_unit(0, &short).is_err());
    }

    // set_stride re-seeds page_count in the new unit size.
    #[test]
    fn set_stride_reseeds_page_count_in_memory() {
        let mut io = PageIo::open_in_memory().unwrap();
        // Write 2 plaintext pages (stride == PAGE_SIZE by default).
        io.write_page(0, &[0u8; PAGE_SIZE]).unwrap();
        io.write_page(1, &[0u8; PAGE_SIZE]).unwrap();
        assert_eq!(io.page_count().unwrap(), 2);

        // After switching to ENC_PAGE_SIZE stride: 2 * 8192 = 16384 bytes.
        // 16384 / 8232 = 1 full unit (remainder discarded by integer div).
        io.set_stride(ENC_PAGE_SIZE);
        assert_eq!(io.stride(), ENC_PAGE_SIZE);
        assert_eq!(io.page_count().unwrap(), 1);
    }

    // set_page_count uses stride so the file length is n * stride bytes.
    #[test]
    fn set_page_count_uses_stride() {
        let mut io = PageIo::open_in_memory().unwrap();
        io.set_stride(ENC_PAGE_SIZE);
        io.set_page_count(3).unwrap();
        assert_eq!(io.page_count().unwrap(), 3);
        // The flat byte vec must be exactly 3 * 8232 bytes.
        // Verify indirectly: reading page 2 (zero-filled) must succeed.
        assert_eq!(io.read_page_unit(2).unwrap(), vec![0u8; ENC_PAGE_SIZE]);
        // And page 3 must be out-of-range.
        assert!(matches!(
            io.read_page_unit(3),
            Err(ChiselError::InvalidPageId { page_id: 3 })
        ));
    }

    // Plaintext read_page/write_page still work unchanged with the default stride.
    #[test]
    fn plaintext_wrappers_unchanged_at_default_stride() {
        use tempfile::NamedTempFile;
        let f = NamedTempFile::new().unwrap();
        let mut io = PageIo::open(f.path(), false).unwrap();
        assert_eq!(io.stride(), PAGE_SIZE);

        let mut buf = [0u8; PAGE_SIZE];
        buf[0] = 0x77;
        buf[PAGE_SIZE - 1] = 0x99;
        io.write_page(0, &buf).unwrap();
        io.write_page(1, &[0xAB; PAGE_SIZE]).unwrap();

        assert_eq!(io.page_count().unwrap(), 2);
        assert_eq!(io.read_page(0).unwrap(), buf);
        assert_eq!(io.read_page(1).unwrap(), [0xAB; PAGE_SIZE]);
    }
}
