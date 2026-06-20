// superblock.rs — Foundation layer (layer 1). Superblock layout, (de)serialization,
// and the dual-superblock selection rule that makes commits atomic.
//
// Shadow-paging commit protocol (see transaction.rs for the orchestration):
//   1. Write all new/COW'd data pages; fsync the data file.
//   2. Write the new superblock to slot `new_txn_counter % superblock_count`.
//      With the default N=2 this alternates slot 0/1; with N>2 (ISSUES.md
//      R4) it rotates through all N slots. The "previous" slot is always
//      left untouched, so a crash between (1) and (3) cannot destroy the
//      last-known-good superblock.
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
// Invariant: the superblock slots occupy fixed page ids 0..N (where N is
// the configurable `superblock_count` — see ISSUES.md R4). Which one is
// "current" is determined solely by txn_counter + checksum validity,
// never by position. N=2 is the default (matches the original v1 layout);
// higher N trades disk space for resilience against consecutive torn
// writes. N is stored inside each superblock so open-time recovery can
// discover it from the first valid slot.

use crate::page::{self, MAGIC, PAGE_SIZE};

// Superblock count bounds (ISSUES.md R4). Hardcoded limits keep the
// probe-at-open-time cost bounded and prevent obviously-broken configs.
// N=1 is disqualified because it provides no redundancy — a single torn
// write would brick the database. N > 16 is refused because any realistic
// workload's sweet spot is N=2-4; beyond that the disk-space cost grows
// without providing additional resilience worth the complexity.
pub const MIN_SUPERBLOCKS: u32 = 2;
pub const MAX_SUPERBLOCKS: u32 = 16;
pub const DEFAULT_SUPERBLOCK_COUNT: u32 = 2;

// Byte offset of the superblock_count field within the serialized
// superblock page. Placed AFTER the named-root table (which ends at
// NAMED_ROOTS_END = 308). Deserialization rejects any value outside
// MIN_SUPERBLOCKS..=MAX_SUPERBLOCKS — a slot with an out-of-range
// count is treated like a failed checksum: discarded by `select()`,
// letting the sibling slot take over. A bogus count can't be allowed
// to reach commit because it's used as the slot-selection modulus
// (txn_counter % superblock_count); an out-of-range value would
// direct superblock writes into the data region.
const SUPERBLOCK_COUNT_OFFSET: usize = 308;

// Membership-index root (chunk-tags). 8 bytes at 312..320, the first
// free reserved bytes after superblock_count. PAGE_ID_NONE when no
// tagged chunk exists (field was added as part of the chunk-tags
// feature; bytes were previously zeroed reserved space).
const ROOT_MEMBERSHIP_INDEX_OFFSET: usize = 312;

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
}

/// In-memory superblock. The on-disk encoding is a fixed little-endian layout:
/// bytes 0..52 hold the scalar fields, bytes 52..308 hold the named-root
/// table (8 × 32-byte entries), bytes 308..312 hold `superblock_count`,
/// bytes 312..320 hold `root_membership_index_page`, and bytes
/// 320..CHECKSUM_OFFSET are reserved (zero) for forward compatibility.
/// The last 8 bytes are the checksum.
///
/// `format_version` is a packed u32 (upper 16 = MAJOR, lower 16 = MINOR);
/// see `page.rs`. Within a major version, new fields may be added by
/// consuming bytes from the reserved region and bumping MINOR. A major
/// bump is reserved for structural or semantic changes that cannot be
/// expressed additively (repositioned fields, altered page formats,
/// new checksum scheme, and so on). Any change — additive or not —
/// requires updating serialize/deserialize in lockstep.
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
    // Next u64 handle id to hand out. Handles are never reused. Handle 0 is
    // reserved as the "no handle" sentinel and is never minted — a fresh store
    // seeds this at 1 (see new_empty), so the first handle handed out is 1.
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
    // Number of superblock slots in the file at pages 0..N (ISSUES.md
    // R4). Serialized at byte 308 (right after named_roots). Stored in
    // each slot so open-time recovery can discover N from the first
    // valid slot it finds. Valid range is MIN_SUPERBLOCKS..=MAX_SUPERBLOCKS;
    // any value outside that range (including 0) is rejected by
    // `deserialize` and the slot is treated as corrupt — a zero value
    // would be catastrophic because the commit path uses
    // `txn_counter % superblock_count` to pick the write slot.
    pub superblock_count: u32,
    // Root page of the membership index (chunk-tags feature). PAGE_ID_NONE
    // until the first tagged chunk is written. Serialized at bytes 312..320,
    // the first 8 bytes of the reserved region that follows superblock_count.
    // Old files (pre-chunk-tags) have these bytes zeroed; callers normalize
    // 0 → PAGE_ID_NONE on open so the rest of the engine has a single
    // "empty" sentinel.
    pub root_membership_index_page: u64,
}

impl Superblock {
    /// Serialize the superblock into a full page buffer with a trailing checksum.
    /// The offsets below are part of the on-disk format contract — do not
    /// reorder fields within a MAJOR; reordering or resizing existing fields
    /// is a major-version bump.
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
        // Superblock count (R4). Written at byte 308, within the v2
        // reserved region that follows the named-root table.
        buf[SUPERBLOCK_COUNT_OFFSET..SUPERBLOCK_COUNT_OFFSET + 4]
            .copy_from_slice(&self.superblock_count.to_le_bytes());
        // Membership-index root (chunk-tags). Written at bytes 312..320,
        // immediately after superblock_count.
        buf[ROOT_MEMBERSHIP_INDEX_OFFSET..ROOT_MEMBERSHIP_INDEX_OFFSET + 8]
            .copy_from_slice(&self.root_membership_index_page.to_le_bytes());
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
        // Superblock count (R4). Must be within MIN..=MAX; any other
        // value is treated as a corrupt slot. This is load-bearing:
        // commit uses `txn_counter % superblock_count` to pick the
        // next write slot, so an out-of-range count would direct a
        // superblock write into the data region. Validating here (not
        // later) means `select()` automatically falls back to the
        // sibling slot if only one slot has a bad count.
        let superblock_count = u32::from_le_bytes(
            buf[SUPERBLOCK_COUNT_OFFSET..SUPERBLOCK_COUNT_OFFSET + 4]
                .try_into()
                .unwrap(),
        );
        if !(MIN_SUPERBLOCKS..=MAX_SUPERBLOCKS).contains(&superblock_count) {
            return None;
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
            superblock_count,
            root_membership_index_page: u64::from_le_bytes(
                buf[ROOT_MEMBERSHIP_INDEX_OFFSET..ROOT_MEMBERSHIP_INDEX_OFFSET + 8]
                    .try_into()
                    .unwrap(),
            ),
        })
    }

    /// Select the active superblock from a list of candidate slot buffers.
    ///
    /// Correctness of crash recovery rides on this: we deserialize every
    /// candidate, discard any whose checksum/magic/count-range validation
    /// fails, and pick the survivor with the highest `txn_counter`.
    /// Because the commit protocol fsyncs data pages *before* writing the
    /// new superblock, the highest-counter valid superblock is guaranteed
    /// to reference a fully-durable page set.
    ///
    /// The caller (`TransactionManager::open_existing`) passes up to
    /// MAX_SUPERBLOCKS candidate pages without first trying to determine
    /// N: non-superblock pages (data / overflow / freemap / handle-table)
    /// that happen to land in the probed range will fail the MAGIC check
    /// inside `deserialize` and be filtered out harmlessly. This is why
    /// the open path reads blindly up to MAX_SUPERBLOCKS rather than
    /// trying to look up N first.
    ///
    /// Returns None only when *every* candidate is corrupt — the caller
    /// should treat that as `CorruptSuperblock` (fatal).
    ///
    /// Tie-break policy: on a `txn_counter` tie, `max_by_key` returns the
    /// FIRST maximum in iteration order — i.e. the slot with the lowest
    /// page id. Ties should not arise in normal operation (every
    /// successful commit bumps the counter), but they can appear during
    /// the `create_new` seeding window before the first user commit and
    /// in hand-crafted corruption-repair scenarios. Lowest-slot-wins is
    /// deterministic and matches the slot-0-is-primary intuition.
    pub fn select(buffers: &[[u8; PAGE_SIZE]]) -> Option<Superblock> {
        buffers
            .iter()
            .filter_map(Superblock::deserialize)
            .max_by_key(|sb| sb.txn_counter)
    }

    /// Create the initial superblock for a new, empty database.
    ///
    /// `txn_counter` starts at `superblock_count - 1` so the first
    /// user commit (which bumps to `superblock_count`) writes slot
    /// `superblock_count % superblock_count == 0` — the highest-
    /// counter slot, exactly as the I2 fix requires for N=2. The
    /// caller (`TransactionManager::create_new`) pairs this with
    /// additional lower-counter "fallback" slots in positions 1..N
    /// (counters superblock_count-2, superblock_count-3, ..., 0) so
    /// that every slot on disk holds a valid-empty-database
    /// superblock from the moment the file exists; this means a
    /// torn write on the FIRST commit can still fall back to one of
    /// the pre-seeded siblings rather than to garbage.
    ///
    /// `total_pages = superblock_count` reserves the N slots and
    /// nothing else. Both root pointers are PAGE_ID_NONE — the handle
    /// table and freemap are created lazily on first write. The named-
    /// root table starts empty.
    pub fn new_empty(superblock_count: u32) -> Superblock {
        Superblock {
            magic: MAGIC,
            format_version: page::FORMAT_VERSION,
            txn_counter: (superblock_count - 1) as u64,
            root_handle_table_page: page::PAGE_ID_NONE,
            root_freemap_page: page::PAGE_ID_NONE,
            total_pages: superblock_count as u64,
            // Handle 0 is reserved as the "no handle" sentinel and is never
            // minted, so a fresh store seeds the counter at 1. Must match the
            // in-memory Roots in TransactionManager's create path.
            next_handle: 1,
            page_size: PAGE_SIZE as u32,
            named_roots: [NamedRoot::EMPTY; NAMED_ROOT_COUNT],
            superblock_count,
            root_membership_index_page: page::PAGE_ID_NONE,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prop_assert_eq;

    #[test]
    fn new_empty_reserves_handle_zero() {
        // Handle 0 is the reserved "no handle" sentinel: a fresh store's
        // superblock seeds next_handle at 1 so the allocator never mints 0.
        let sb = Superblock::new_empty(2);
        assert_eq!(sb.next_handle, 1);
    }

    /// A superblock whose count field is outside [MIN, MAX] must be
    /// rejected by `deserialize`. Otherwise the recovered count would
    /// feed the `txn_counter % superblock_count` slot calculation in
    /// commit and could direct a superblock write into the data region.
    #[test]
    fn deserialize_rejects_out_of_range_superblock_count() {
        let mut sb = Superblock::new_empty(DEFAULT_SUPERBLOCK_COUNT);
        for bogus in [0u32, 1, MAX_SUPERBLOCKS + 1, 1_000_000, u32::MAX] {
            sb.superblock_count = bogus;
            // Build with the bad value, then re-stamp the checksum so
            // the only reason to reject is the count field itself.
            let mut buf = sb.serialize();
            buf[SUPERBLOCK_COUNT_OFFSET..SUPERBLOCK_COUNT_OFFSET + 4]
                .copy_from_slice(&bogus.to_le_bytes());
            page::stamp_checksum(&mut buf);
            assert!(
                Superblock::deserialize(&buf).is_none(),
                "deserialize accepted out-of-range superblock_count = {bogus}"
            );
        }
    }

    /// All in-range counts must round-trip cleanly.
    #[test]
    fn deserialize_accepts_all_valid_superblock_counts() {
        for n in MIN_SUPERBLOCKS..=MAX_SUPERBLOCKS {
            let sb = Superblock::new_empty(n);
            let buf = sb.serialize();
            let got = Superblock::deserialize(&buf).expect("valid count rejected");
            assert_eq!(got.superblock_count, n);
        }
    }

    /// If one slot has a bogus count but the sibling is valid,
    /// `select()` must pick the valid sibling rather than returning None.
    #[test]
    fn select_falls_back_when_one_slot_has_bad_count() {
        let good = Superblock::new_empty(DEFAULT_SUPERBLOCK_COUNT);
        let good_buf = good.serialize();

        let mut bad_buf = good.serialize();
        bad_buf[SUPERBLOCK_COUNT_OFFSET..SUPERBLOCK_COUNT_OFFSET + 4]
            .copy_from_slice(&1_000_000u32.to_le_bytes());
        page::stamp_checksum(&mut bad_buf);

        let picked = Superblock::select(&[bad_buf, good_buf]).expect("no slot picked");
        assert_eq!(picked.superblock_count, DEFAULT_SUPERBLOCK_COUNT);
    }

    // ── Migrated 2026-05-22 from tests/basic_ops.rs (I35 reshape) ──
    //
    // The originals used the bare `MAGIC`/`FORMAT_VERSION`/`PAGE_ID_NONE`
    // page-module constants via `use chisel::page::...`; from inside src/
    // they are reached through `crate::page::*`, which `use super::*` and
    // the existing `use crate::page::{self, MAGIC, PAGE_SIZE};` at file
    // scope make available without further imports.

    #[test]
    fn test_superblock_roundtrip() {
        let sb = Superblock {
            magic: MAGIC,
            format_version: crate::page::FORMAT_VERSION,
            txn_counter: 42,
            root_handle_table_page: 5,
            root_freemap_page: 8,
            total_pages: 100,
            next_handle: 50,
            page_size: PAGE_SIZE as u32,
            named_roots: [NamedRoot::EMPTY; NAMED_ROOT_COUNT],
            superblock_count: DEFAULT_SUPERBLOCK_COUNT,
            root_membership_index_page: crate::page::PAGE_ID_NONE,
        };
        let buf = sb.serialize();
        let sb2 = Superblock::deserialize(&buf).unwrap();
        assert_eq!(sb, sb2);
    }

    #[test]
    fn test_superblock_checksum_validation() {
        let sb = Superblock {
            magic: MAGIC,
            format_version: crate::page::FORMAT_VERSION,
            txn_counter: 1,
            root_handle_table_page: crate::page::PAGE_ID_NONE,
            root_freemap_page: crate::page::PAGE_ID_NONE,
            total_pages: 2,
            next_handle: 0,
            page_size: PAGE_SIZE as u32,
            named_roots: [NamedRoot::EMPTY; NAMED_ROOT_COUNT],
            superblock_count: DEFAULT_SUPERBLOCK_COUNT,
            root_membership_index_page: crate::page::PAGE_ID_NONE,
        };
        let mut buf = sb.serialize();
        buf[10] ^= 0xFF;
        assert!(Superblock::deserialize(&buf).is_none());
    }

    #[test]
    fn test_superblock_selection() {
        let sb1 = Superblock {
            magic: MAGIC,
            format_version: crate::page::FORMAT_VERSION,
            txn_counter: 5,
            root_handle_table_page: 2,
            root_freemap_page: 3,
            total_pages: 10,
            next_handle: 3,
            page_size: PAGE_SIZE as u32,
            named_roots: [NamedRoot::EMPTY; NAMED_ROOT_COUNT],
            superblock_count: DEFAULT_SUPERBLOCK_COUNT,
            root_membership_index_page: crate::page::PAGE_ID_NONE,
        };
        let sb2 = Superblock {
            magic: MAGIC,
            format_version: crate::page::FORMAT_VERSION,
            txn_counter: 7,
            root_handle_table_page: 4,
            root_freemap_page: 5,
            total_pages: 12,
            next_handle: 5,
            page_size: PAGE_SIZE as u32,
            named_roots: [NamedRoot::EMPTY; NAMED_ROOT_COUNT],
            superblock_count: DEFAULT_SUPERBLOCK_COUNT,
            root_membership_index_page: crate::page::PAGE_ID_NONE,
        };
        let buf1 = sb1.serialize();
        let buf2 = sb2.serialize();
        let selected = Superblock::select(&[buf1, buf2]).unwrap();
        assert_eq!(selected.txn_counter, 7);
    }

    #[test]
    fn test_superblock_selection_with_one_corrupt() {
        let sb1 = Superblock {
            magic: MAGIC,
            format_version: crate::page::FORMAT_VERSION,
            txn_counter: 5,
            root_handle_table_page: 2,
            root_freemap_page: 3,
            total_pages: 10,
            next_handle: 3,
            page_size: PAGE_SIZE as u32,
            named_roots: [NamedRoot::EMPTY; NAMED_ROOT_COUNT],
            superblock_count: DEFAULT_SUPERBLOCK_COUNT,
            root_membership_index_page: crate::page::PAGE_ID_NONE,
        };
        let sb2_buf = [0u8; PAGE_SIZE];
        let buf1 = sb1.serialize();
        let selected = Superblock::select(&[buf1, sb2_buf]).unwrap();
        assert_eq!(selected.txn_counter, 5);
    }

    #[test]
    fn test_superblock_selection_both_corrupt() {
        let buf1 = [0u8; PAGE_SIZE];
        let buf2 = [0u8; PAGE_SIZE];
        assert!(Superblock::select(&[buf1, buf2]).is_none());
    }

    /// The new root_membership_index_page field must persist across serialize/
    /// deserialize and default to PAGE_ID_NONE on new_empty(). Also verifies
    /// that an old serialized form (bytes 312..320 zeroed) reads back as 0, not
    /// as PAGE_ID_NONE — callers like open_existing normalize 0 → PAGE_ID_NONE.
    #[test]
    fn membership_root_round_trips_through_serialize() {
        let mut sb = Superblock::new_empty(DEFAULT_SUPERBLOCK_COUNT);
        assert_eq!(sb.root_membership_index_page, page::PAGE_ID_NONE);
        sb.root_membership_index_page = 1234;
        let buf = sb.serialize();
        let back = Superblock::deserialize(&buf).unwrap();
        assert_eq!(back.root_membership_index_page, 1234);
        let mut old = sb.serialize();
        old[312..320].fill(0);
        page::stamp_checksum(&mut old);
        assert_eq!(
            Superblock::deserialize(&old)
                .unwrap()
                .root_membership_index_page,
            0
        );
    }

    // I71 (ISSUES.md, 2026-05-22): property test —
    // `deserialize(serialize(sb)) == Some(sb)` for any well-formed
    // Superblock. The proptest strategy builds a Superblock by
    // sampling its individually-varying fields (txn_counter, roots,
    // page-id fields, named-root names/handles, superblock_count)
    // while pinning the format-invariant fields (magic, format_version,
    // page_size). NamedRoot.name uses a byte-array strategy so the
    // empty-slot convention (name[0] == 0) gets exercised alongside
    // populated names.
    proptest::proptest! {
        #[test]
        fn prop_serialize_deserialize_roundtrip(
            txn_counter in 0u64..u64::MAX,
            root_handle_table_page in 0u64..u64::MAX,
            root_freemap_page in 0u64..u64::MAX,
            total_pages in 0u64..u64::MAX,
            next_handle in 0u64..u64::MAX,
            // superblock_count must lie in the validated range
            // MIN_SUPERBLOCKS..=MAX_SUPERBLOCKS (2..=16); values
            // outside this range cause deserialize() to return None
            // by design (defends against torn-slot corruption that
            // would otherwise direct a superblock write into the
            // data region). Sampling only the valid range keeps the
            // round-trip property well-defined.
            superblock_count in MIN_SUPERBLOCKS..=MAX_SUPERBLOCKS,
            named_root_bytes in proptest::array::uniform8(
                proptest::array::uniform24(0u8..=255u8)
            ),
            named_root_handles in proptest::array::uniform8(0u64..u64::MAX),
            root_membership_index_page in 0u64..u64::MAX,
        ) {
            let mut named_roots = [NamedRoot::EMPTY; NAMED_ROOT_COUNT];
            for (i, slot) in named_roots.iter_mut().enumerate() {
                slot.name = named_root_bytes[i];
                slot.handle = named_root_handles[i];
            }
            let sb = Superblock {
                magic: MAGIC,
                format_version: crate::page::FORMAT_VERSION,
                txn_counter,
                root_handle_table_page,
                root_freemap_page,
                total_pages,
                next_handle,
                page_size: PAGE_SIZE as u32,
                named_roots,
                superblock_count,
                root_membership_index_page,
            };
            let buf = sb.serialize();
            let parsed = Superblock::deserialize(&buf)
                .expect("a freshly-serialized superblock must deserialize");
            // Field-by-field equality. PartialEq isn't derived on
            // Superblock, so compare structurally — easier to diagnose
            // if a single field round-trips wrong.
            prop_assert_eq!(parsed.magic, sb.magic);
            prop_assert_eq!(parsed.format_version, sb.format_version);
            prop_assert_eq!(parsed.txn_counter, sb.txn_counter);
            prop_assert_eq!(parsed.root_handle_table_page, sb.root_handle_table_page);
            prop_assert_eq!(parsed.root_freemap_page, sb.root_freemap_page);
            prop_assert_eq!(parsed.total_pages, sb.total_pages);
            prop_assert_eq!(parsed.next_handle, sb.next_handle);
            prop_assert_eq!(parsed.page_size, sb.page_size);
            prop_assert_eq!(parsed.superblock_count, sb.superblock_count);
            prop_assert_eq!(parsed.root_membership_index_page, sb.root_membership_index_page);
            for i in 0..NAMED_ROOT_COUNT {
                prop_assert_eq!(parsed.named_roots[i].name, sb.named_roots[i].name);
                prop_assert_eq!(parsed.named_roots[i].handle, sb.named_roots[i].handle);
            }
        }
    }
}
