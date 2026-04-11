// superblock.rs — Foundation layer (layer 1). Superblock layout, (de)serialization,
// and the dual-superblock selection rule that makes commits atomic.
//
// Shadow-paging commit protocol (see transaction.rs for the orchestration):
//   1. Write all new/COW'd data pages; fsync the data file.
//   2. Write the new superblock to the *other* of the two superblock slots
//      (slots alternate — if txn N used slot 0, txn N+1 uses slot 1).
//   3. fsync again.
// If we crash between (1) and (2), the old superblock is still current and
// the new data pages are orphaned garbage. They are NOT actively cleaned up
// on next mount — they remain in the file as dead weight. Subsequent
// allocations overwrite them because `open_existing` reseeds next_page_id
// from the authoritative superblock's total_pages (see ISSUES.md I4), so
// the garbage tail gets reclaimed the next time the file grows through
// that range. In the meantime they are harmless (unreferenced from any
// live root). A clean rollback via `rollback()` DOES truncate the file
// immediately (ISSUES.md I3); the "leave it for later" path only applies
// to actual crashes.
// If we crash mid-write of the new superblock, its checksum will fail and
// `select()` will fall back to the previous one. Either way, exactly one
// consistent snapshot is recoverable — no WAL replay needed.
//
// Invariant: the two superblock slots occupy fixed page ids (typically 0 and
// 1; defined by the caller in page_io). Which one is "current" is determined
// solely by txn_counter + checksum validity, never by position.

use crate::page::{self, MAGIC, PAGE_SIZE};

// Named-root table (ISSUES.md F2). A small fixed-width table lives inside
// the superblock itself so that named roots get the same atomic-commit
// semantics as the handle-table root for free. Client use case (from the
// F2 design discussion): replace the "handle 0 is always the meta B-tree
// root" convention with an explicit name → handle mapping. Typical
// database needs one or two named roots; eight is deliberate overkill.
//
// Layout per entry: 24 bytes of UTF-8 name (null-padded, no terminator
// required; a name whose first byte is 0 is an UNUSED slot) followed by a
// u64 handle value = 32 bytes. 8 entries × 32 bytes = 256 bytes total.
// Placed immediately after the existing fields (starting at byte 52) so
// pre-v2 serialized layouts don't need to move.
pub const NAMED_ROOT_COUNT: usize = 8;
pub const NAMED_ROOT_NAME_LEN: usize = 24;
const NAMED_ROOT_ENTRY_SIZE: usize = NAMED_ROOT_NAME_LEN + 8;
const NAMED_ROOTS_OFFSET: usize = 52;
// End offset of the named-root table. Documented as a const for the
// reader (future fields must start at or after this byte); not referenced
// directly from code since the serialize/deserialize loops compute their
// own offsets from NAMED_ROOTS_OFFSET and the entry stride.
#[allow(dead_code)]
const NAMED_ROOTS_END: usize = NAMED_ROOTS_OFFSET + NAMED_ROOT_COUNT * NAMED_ROOT_ENTRY_SIZE;

/// A single entry in the superblock's named-root table.
///
/// `name` is a fixed 24-byte buffer holding UTF-8 bytes padded with NULs.
/// An entry with `name[0] == 0` is considered unused — this is the
/// convention that makes `new_empty()` (which zero-initializes the array)
/// produce a correctly-empty table without any extra bookkeeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NamedRoot {
    pub name: [u8; NAMED_ROOT_NAME_LEN],
    pub handle: u64,
}

impl NamedRoot {
    pub const EMPTY: NamedRoot = NamedRoot {
        name: [0u8; NAMED_ROOT_NAME_LEN],
        handle: 0,
    };

    /// Returns true if this slot is unused (name[0] is zero).
    pub fn is_empty(&self) -> bool {
        self.name[0] == 0
    }

    /// Returns the slot's name as a str, trimming trailing NULs. Returns
    /// None if the stored bytes are not valid UTF-8 (should never happen
    /// for names written through the public API, since set_root_name
    /// rejects non-UTF-8 input).
    pub fn name_str(&self) -> Option<&str> {
        let end = self
            .name
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(self.name.len());
        std::str::from_utf8(&self.name[..end]).ok()
    }
}

/// In-memory superblock. The on-disk encoding is a fixed little-endian layout:
/// bytes 0..52 hold the scalar fields, bytes 52..308 hold the named-root
/// table (8 × 32-byte entries), and bytes 308..CHECKSUM_OFFSET are reserved
/// (zero) for forward compatibility. The last 8 bytes are the checksum.
/// Adding a field requires bumping FORMAT_VERSION and updating serialize/
/// deserialize in lockstep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Superblock {
    pub magic: u32,
    pub format_version: u32,
    // Monotonically increasing per commit. Also the tiebreaker that selects
    // which of the two superblock slots is current.
    pub txn_counter: u64,
    // PAGE_ID_NONE until the first value is inserted (no handle table yet).
    pub root_handle_table_page: u64,
    // PAGE_ID_NONE if the freemap has not been materialized yet.
    pub root_freemap_page: u64,
    // Total allocated pages in the file, including both superblock slots.
    // Used to detect truncation (see FileSizeMismatch).
    pub total_pages: u64,
    // Next u64 handle id to hand out. Handles are never reused.
    pub next_handle: u64,
    // Stored so a future page-size change can be detected at open time
    // rather than silently misreading.
    pub page_size: u32,
    // Named-root table (F2). Fixed size; unused slots have name[0] == 0.
    // Serialized at byte offset 52 for 256 bytes total. These are part of
    // the transactional commit point — set_root_name writes to the
    // in-memory Roots snapshot and the whole thing gets promoted on commit,
    // so named roots survive rollback/savepoint correctly for free.
    pub named_roots: [NamedRoot; NAMED_ROOT_COUNT],
}

impl Superblock {
    /// Serialize the superblock into a full page buffer with a trailing checksum.
    /// The offsets below are part of the on-disk format contract — do not
    /// reorder fields without bumping FORMAT_VERSION.
    pub fn serialize(&self) -> [u8; PAGE_SIZE] {
        let mut buf = [0u8; PAGE_SIZE];
        buf[0..4].copy_from_slice(&self.magic.to_le_bytes());
        buf[4..8].copy_from_slice(&self.format_version.to_le_bytes());
        buf[8..16].copy_from_slice(&self.txn_counter.to_le_bytes());
        buf[16..24].copy_from_slice(&self.root_handle_table_page.to_le_bytes());
        buf[24..32].copy_from_slice(&self.root_freemap_page.to_le_bytes());
        buf[32..40].copy_from_slice(&self.total_pages.to_le_bytes());
        buf[40..48].copy_from_slice(&self.next_handle.to_le_bytes());
        buf[48..52].copy_from_slice(&self.page_size.to_le_bytes());
        // Named-root table (F2). Entries are written in array order; empty
        // slots serialize as all zeros, which round-trips correctly because
        // NamedRoot::is_empty() tests name[0] == 0.
        for (i, entry) in self.named_roots.iter().enumerate() {
            let base = NAMED_ROOTS_OFFSET + i * NAMED_ROOT_ENTRY_SIZE;
            buf[base..base + NAMED_ROOT_NAME_LEN].copy_from_slice(&entry.name);
            buf[base + NAMED_ROOT_NAME_LEN..base + NAMED_ROOT_NAME_LEN + 8]
                .copy_from_slice(&entry.handle.to_le_bytes());
        }
        // bytes NAMED_ROOTS_END..CHECKSUM_OFFSET are reserved for future
        // fields and must stay zero so existing checksums remain reproducible.
        page::stamp_checksum(&mut buf);
        buf
    }

    /// Deserialize from a page buffer. Returns None if the checksum is invalid
    /// or the magic number doesn't match.
    ///
    /// Returning `Option` (instead of `Result`) is intentional: a failed
    /// superblock is not a fatal error here — `select()` needs to be able to
    /// discard a torn slot and fall back to its sibling. Promotion to a
    /// fatal `CorruptSuperblock` happens only if *both* slots fail.
    pub fn deserialize(buf: &[u8; PAGE_SIZE]) -> Option<Superblock> {
        // Checksum first: if it fails, the rest of the buffer is untrusted
        // and we must not interpret any field (including magic).
        if !page::verify_checksum(buf) {
            return None;
        }
        let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        if magic != MAGIC {
            return None;
        }
        // NOTE: format_version is read but not validated here. Callers that
        // need version gating must check it on the returned struct (see
        // TransactionManager::open_existing and ISSUES.md I15).
        let mut named_roots = [NamedRoot::EMPTY; NAMED_ROOT_COUNT];
        for (i, entry) in named_roots.iter_mut().enumerate() {
            let base = NAMED_ROOTS_OFFSET + i * NAMED_ROOT_ENTRY_SIZE;
            entry
                .name
                .copy_from_slice(&buf[base..base + NAMED_ROOT_NAME_LEN]);
            entry.handle = u64::from_le_bytes(
                buf[base + NAMED_ROOT_NAME_LEN..base + NAMED_ROOT_NAME_LEN + 8]
                    .try_into()
                    .unwrap(),
            );
        }
        Some(Superblock {
            magic,
            format_version: u32::from_le_bytes(buf[4..8].try_into().unwrap()),
            txn_counter: u64::from_le_bytes(buf[8..16].try_into().unwrap()),
            root_handle_table_page: u64::from_le_bytes(buf[16..24].try_into().unwrap()),
            root_freemap_page: u64::from_le_bytes(buf[24..32].try_into().unwrap()),
            total_pages: u64::from_le_bytes(buf[32..40].try_into().unwrap()),
            next_handle: u64::from_le_bytes(buf[40..48].try_into().unwrap()),
            page_size: u32::from_le_bytes(buf[48..52].try_into().unwrap()),
            named_roots,
        })
    }

    /// Select the active superblock from the pair of slot buffers.
    ///
    /// Correctness of crash recovery rides on this: we deserialize both
    /// slots, discard any whose checksum/magic fail, and pick the survivor
    /// with the highest `txn_counter`. Because the commit protocol fsyncs
    /// data pages *before* writing the new superblock, the highest-counter
    /// valid superblock is guaranteed to reference a fully-durable page set.
    ///
    /// Returns None only when *every* slot is corrupt — the caller should
    /// treat that as `CorruptSuperblock` (fatal).
    pub fn select(buffers: &[[u8; PAGE_SIZE]]) -> Option<Superblock> {
        buffers
            .iter()
            .filter_map(Superblock::deserialize)
            .max_by_key(|sb| sb.txn_counter)
    }

    /// Create the initial superblock for a new, empty database.
    ///
    /// `txn_counter` starts at 1 (not 0) so that any zero-initialized region
    /// on disk cannot accidentally out-rank a legitimate superblock during
    /// `select()`. `total_pages = 2` reserves the two superblock slots.
    /// Both roots are PAGE_ID_NONE — the handle table and freemap are
    /// created lazily on first write. The named-root table starts empty
    /// (all slots zeroed; `is_empty()` returns true for every entry).
    pub fn new_empty() -> Superblock {
        Superblock {
            magic: MAGIC,
            format_version: page::FORMAT_VERSION,
            txn_counter: 1,
            root_handle_table_page: page::PAGE_ID_NONE,
            root_freemap_page: page::PAGE_ID_NONE,
            total_pages: 2,
            next_handle: 0,
            page_size: PAGE_SIZE as u32,
            named_roots: [NamedRoot::EMPTY; NAMED_ROOT_COUNT],
        }
    }
}
