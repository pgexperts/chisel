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

use crate::crypto::{CryptoError, NONCE_LEN, TAG_LEN};
use crate::page::{self, MAGIC, PAGE_SIZE};
use std::fmt;

mod crypto_header;
// Re-export the crypto-header API for consumers (open/create code in later
// phases, key-management tools, tests). Items not yet referenced in non-test
// module code are still public API surface — the allow is intentional.
#[allow(unused_imports)]
pub use crypto_header::{
    CryptoHeader, KeySlot, ALGO_XCHACHA20POLY1305, CRYPTO_HEADER_OFFSET, CRYPTO_HEADER_SIZE,
    KEY_SLOT_COUNT, KEY_SLOT_SIZE,
};

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

// Depth of the multi-page radix freemap tree. 4 bytes at 320..324.
// 0 means the single-page freemap (pre-multi-page-freemap files, or
// a new database that has not yet grown past one leaf's capacity).
// Old files have these bytes zeroed; depth 0 is backward-compatible
// because the existing single-page path is the depth-0 case.
const FREEMAP_DEPTH_OFFSET: usize = 320;

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

/// Why a superblock slot failed validation (I106). These are exactly the three
/// torn-slot causes `deserialize` checks. `Copy` and small so the failure-path
/// `Vec<SlotDefect>` is cheap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
// Variant names intentionally share the `Bad` prefix — they describe the
// three distinct bad-slot causes and the prefix reads naturally at call
// sites (SuperblockDefect::BadChecksum, etc.). Suppressed until Task 2
// wires the hot path through these types and callers appear in non-test code.
#[allow(clippy::enum_variant_names)]
pub enum SuperblockDefect {
    BadChecksum,
    BadMagic,
    BadCount(u32), // the out-of-range superblock_count value
}

impl fmt::Display for SuperblockDefect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SuperblockDefect::BadChecksum => write!(f, "bad checksum"),
            SuperblockDefect::BadMagic => write!(f, "bad magic"),
            SuperblockDefect::BadCount(n) => write!(f, "bad superblock_count {n}"),
        }
    }
}

/// A defect tagged with the candidate-slot index it was found at (I106).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotDefect {
    pub slot: u32,
    pub defect: SuperblockDefect,
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
    // Validated against the compiled `PAGE_SIZE` at open time
    // (`TransactionManager::open_existing`). A mismatch surfaces as
    // `UnsupportedPageSize` before any data is read.
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
    // Depth of the multi-page radix freemap tree. Serialized at bytes 320..324.
    // 0 = single-page freemap (the pre-multi-page-freemap path, compatible with
    // old files whose reserved bytes are zeroed). Depth > 0 means the freemap
    // is a COW radix tree of FreeMap bitmap leaves with FreeMapInterior inner
    // nodes; root_freemap_page points to the root at that depth.
    pub freemap_depth: u32,
    /// Crypto-header for an encrypted database. `None` for plaintext DBs, in
    /// which case serialize/deserialize use the existing all-plaintext layout.
    /// `Some` means the sensitive fields are sealed in a DEK-encrypted body
    /// sub-blob; those fields are zero until the caller supplies the DEK and
    /// calls `decrypt_body`.
    pub encryption: Option<CryptoHeader>,
}

/// The three torn-slot rules, shared by the hot path (`deserialize`) and the
/// cold path (`diagnose`). Order is load-bearing: checksum first — a bad
/// checksum means the rest of the buffer (including magic) is untrusted, so it
/// is reported as `BadChecksum` even if the magic bytes also differ.
fn validate(buf: &[u8; PAGE_SIZE]) -> Result<(), SuperblockDefect> {
    if !page::verify_checksum(buf) {
        return Err(SuperblockDefect::BadChecksum);
    }
    let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    if magic != MAGIC {
        return Err(SuperblockDefect::BadMagic);
    }
    let count = u32::from_le_bytes(
        buf[SUPERBLOCK_COUNT_OFFSET..SUPERBLOCK_COUNT_OFFSET + 4]
            .try_into()
            .unwrap(),
    );
    if !(MIN_SUPERBLOCKS..=MAX_SUPERBLOCKS).contains(&count) {
        return Err(SuperblockDefect::BadCount(count));
    }
    Ok(())
}

// Offset where the DEK-sealed body sub-blob starts, immediately after the
// crypto-header's key-slot table. Layout of the sealed region:
//   SEALED_BODY_OFFSET .. +24    nonce (XChaCha20 192-bit)
//   +24 .. +40                   Poly1305 authentication tag
//   +40 .. +42                   ciphertext length (u16 LE)
//   +42 .. +42+ct_len            ciphertext
// The sealed region must fit before CHECKSUM_OFFSET (8184); at the maximum
// body length (see BODY_LEN) the region ends well inside that bound.
pub const SEALED_BODY_OFFSET: usize =
    crypto_header::CRYPTO_HEADER_OFFSET + crypto_header::CRYPTO_HEADER_SIZE; // 1356

// Plaintext body layout (the bytes fed to seal_body): the sensitive fields in
// a fixed order.
//   0..8   root_handle_table_page (u64 LE)
//   8..16  root_freemap_page (u64 LE)
//   16..24 root_membership_index_page (u64 LE)
//   24..32 total_pages (u64 LE)
//   32..40 next_handle (u64 LE)
//   40..44 freemap_depth (u32 LE)
//   44..44+NAMED_ROOT_COUNT*NAMED_ROOT_ENTRY_SIZE  named_roots
const BODY_LEN: usize = 8 * 5 + 4 + (NAMED_ROOT_COUNT * NAMED_ROOT_ENTRY_SIZE);

// Compile-time check: the sealed blob fits before the checksum.
// SEALED_BODY_OFFSET(1356) + NONCE_LEN(24) + TAG_LEN(16) + 2(len) + BODY_LEN.
const _: () = assert!(
    SEALED_BODY_OFFSET + NONCE_LEN + TAG_LEN + 2 + BODY_LEN <= page::CHECKSUM_OFFSET
);

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
        // Freemap tree depth. Written at bytes 320..324. Old files leave these
        // bytes zero (= single-page freemap, backward compatible).
        buf[FREEMAP_DEPTH_OFFSET..FREEMAP_DEPTH_OFFSET + 4]
            .copy_from_slice(&self.freemap_depth.to_le_bytes());
        page::stamp_checksum(&mut buf);
        buf
    }

    // ponytail: methods below are called from serialize_encrypted/decrypt_body
    // which in turn are called from tests and will be wired to the commit/open
    // path in Task 2.4. Suppress dead_code until that caller lands.
    #[allow(dead_code)]
    /// Build the AAD that binds the sealed body and each key-slot's DEK wrap to
    /// this superblock's plaintext identity. The four bootstrap fields that stay
    /// cleartext in both encrypted and plaintext DBs are included; this prevents
    /// transplanting a sealed body from a different DB or a different txn_counter.
    pub fn sb_identity_aad(&self) -> [u8; 24] {
        let mut a = [0u8; 24];
        a[0..4].copy_from_slice(&self.magic.to_le_bytes());
        a[4..8].copy_from_slice(&self.format_version.to_le_bytes());
        a[8..16].copy_from_slice(&self.txn_counter.to_le_bytes());
        a[16..20].copy_from_slice(&self.superblock_count.to_le_bytes());
        // bytes 20..24 are reserved (zero) for future AAD fields.
        a
    }

    /// Assemble the plaintext body for sealing: all sensitive fields in the
    /// canonical order defined by BODY_LEN. Called only for encrypted DBs.
    fn body_plaintext(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(BODY_LEN);
        b.extend_from_slice(&self.root_handle_table_page.to_le_bytes());
        b.extend_from_slice(&self.root_freemap_page.to_le_bytes());
        b.extend_from_slice(&self.root_membership_index_page.to_le_bytes());
        b.extend_from_slice(&self.total_pages.to_le_bytes());
        b.extend_from_slice(&self.next_handle.to_le_bytes());
        b.extend_from_slice(&self.freemap_depth.to_le_bytes());
        for entry in &self.named_roots {
            b.extend_from_slice(&entry.name);
            b.extend_from_slice(&entry.handle.to_le_bytes());
        }
        b
    }

    /// Unpack a decrypted body blob into `self`'s sensitive fields. The body
    /// layout must match `body_plaintext`'s encoding.
    fn load_body(&mut self, body: &[u8]) {
        // open_body returns exactly the plaintext that was sealed, which is
        // always BODY_LEN bytes (see body_plaintext); document the invariant.
        debug_assert_eq!(body.len(), BODY_LEN);
        self.root_handle_table_page = u64::from_le_bytes(body[0..8].try_into().unwrap());
        self.root_freemap_page = u64::from_le_bytes(body[8..16].try_into().unwrap());
        self.root_membership_index_page = u64::from_le_bytes(body[16..24].try_into().unwrap());
        self.total_pages = u64::from_le_bytes(body[24..32].try_into().unwrap());
        self.next_handle = u64::from_le_bytes(body[32..40].try_into().unwrap());
        self.freemap_depth = u32::from_le_bytes(body[40..44].try_into().unwrap());
        let mut off = 44;
        for entry in self.named_roots.iter_mut() {
            entry.name.copy_from_slice(&body[off..off + NAMED_ROOT_NAME_LEN]);
            entry.handle = u64::from_le_bytes(
                body[off + NAMED_ROOT_NAME_LEN..off + NAMED_ROOT_NAME_LEN + 8]
                    .try_into()
                    .unwrap(),
            );
            off += NAMED_ROOT_ENTRY_SIZE;
        }
    }

    /// Serialize an encrypted superblock: bootstrap fields + crypto-header in
    /// cleartext; sensitive fields sealed under the DEK. The byte ranges that
    /// would hold sensitive data in a plaintext page are left ZERO so nothing
    /// leaks (named_roots at 52..308, root/page-id scalars at 16..52, etc.).
    ///
    /// Panics if `self.encryption` is `None` — only call for encrypted DBs.
    #[allow(dead_code)]
    pub fn serialize_encrypted(&self, cipher: &crate::crypto::PageCipher) -> [u8; PAGE_SIZE] {
        let header = self
            .encryption
            .as_ref()
            .expect("serialize_encrypted requires Superblock.encryption = Some");
        let mut buf = [0u8; PAGE_SIZE];
        // Plaintext bootstrap fields only. Sensitive scalar fields (16..52)
        // and named_roots (52..308) are intentionally left zero.
        buf[0..4].copy_from_slice(&self.magic.to_le_bytes());
        buf[4..8].copy_from_slice(&self.format_version.to_le_bytes());
        buf[8..16].copy_from_slice(&self.txn_counter.to_le_bytes());
        buf[48..52].copy_from_slice(&self.page_size.to_le_bytes());
        buf[SUPERBLOCK_COUNT_OFFSET..SUPERBLOCK_COUNT_OFFSET + 4]
            .copy_from_slice(&self.superblock_count.to_le_bytes());
        // Crypto-header written into reserved region (plaintext).
        header.serialize_into(&mut buf);
        // Seal the sensitive body into the region immediately after the
        // key-slot table: nonce || tag || ct_len(u16 LE) || ciphertext.
        let aad = self.sb_identity_aad();
        let (nonce, tag, ct) = cipher.seal_body(&aad, &self.body_plaintext());
        let base = SEALED_BODY_OFFSET;
        buf[base..base + NONCE_LEN].copy_from_slice(&nonce);
        buf[base + NONCE_LEN..base + NONCE_LEN + TAG_LEN].copy_from_slice(&tag);
        buf[base + NONCE_LEN + TAG_LEN..base + NONCE_LEN + TAG_LEN + 2]
            .copy_from_slice(&(ct.len() as u16).to_le_bytes());
        let coff = base + NONCE_LEN + TAG_LEN + 2;
        buf[coff..coff + ct.len()].copy_from_slice(&ct);
        page::stamp_checksum(&mut buf);
        buf
    }

    /// Decrypt the sealed body into `self`'s sensitive fields. Caller must have
    /// already called `deserialize` (which fills bootstrap fields and the
    /// crypto-header from cleartext) and obtained the matching DEK. Returns
    /// `CryptoError::Auth` if the DEK or AAD is wrong, or the blob is tampered.
    #[allow(dead_code)]
    pub fn decrypt_body(
        &mut self,
        cipher: &crate::crypto::PageCipher,
        raw: &[u8; PAGE_SIZE],
    ) -> Result<(), CryptoError> {
        let base = SEALED_BODY_OFFSET;
        let mut nonce = [0u8; NONCE_LEN];
        nonce.copy_from_slice(&raw[base..base + NONCE_LEN]);
        let mut tag = [0u8; TAG_LEN];
        tag.copy_from_slice(&raw[base + NONCE_LEN..base + NONCE_LEN + TAG_LEN]);
        let ct_len = u16::from_le_bytes(
            raw[base + NONCE_LEN + TAG_LEN..base + NONCE_LEN + TAG_LEN + 2]
                .try_into()
                .unwrap(),
        ) as usize;
        let coff = base + NONCE_LEN + TAG_LEN + 2;
        // Bounds-check ct_len before slicing. The page checksum is XXH3 (non-
        // cryptographic): an attacker who edits the page can recompute it, so a
        // forged ct_len must NOT reach the slice and panic. Treat an out-of-
        // range length as an undecryptable body — same Err as a failed AEAD
        // auth, since open_body would never accept it anyway.
        if coff + ct_len > page::CHECKSUM_OFFSET {
            return Err(CryptoError::Auth);
        }
        let ct = &raw[coff..coff + ct_len];
        let aad = self.sb_identity_aad();
        let body = cipher.open_body(&aad, &nonce, &tag, ct)?;
        self.load_body(&body);
        Ok(())
    }

    /// Deserialize from a page buffer. Returns None if the checksum is invalid
    /// or the magic number doesn't match.
    ///
    /// Returning `Option` (instead of `Result`) is intentional: a failed
    /// superblock is not a fatal error here — `select()` needs to be able to
    /// discard a torn slot and fall back to its sibling. Promotion to a
    /// fatal `CorruptSuperblock` happens only if *both* slots fail.
    pub fn deserialize(buf: &[u8; PAGE_SIZE]) -> Option<Superblock> {
        // Delegates to validate() for the three torn-slot checks (checksum,
        // magic, superblock_count range). See validate()'s doc for why order
        // matters. NOTE: format_version is read but not validated here. Callers
        // that need version gating must check it on the returned struct (see
        // TransactionManager::open_existing and ISSUES.md I15). page_size is
        // likewise read-not-validated for the same reason: a mismatch against
        // the compiled PAGE_SIZE is a fatal open-time error the caller raises,
        // not a torn-slot signal that should make select() fall back.
        validate(buf).ok()?;
        // Check for an encryption header first. For encrypted DBs only the
        // bootstrap fields are in cleartext; the sensitive fields stay zero
        // until the caller supplies the DEK and calls `decrypt_body`.
        let encryption = crypto_header::CryptoHeader::deserialize(buf);
        if encryption.is_some() {
            let superblock_count = u32::from_le_bytes(
                buf[SUPERBLOCK_COUNT_OFFSET..SUPERBLOCK_COUNT_OFFSET + 4]
                    .try_into()
                    .unwrap(),
            );
            return Some(Superblock {
                magic: u32::from_le_bytes(buf[0..4].try_into().unwrap()),
                format_version: u32::from_le_bytes(buf[4..8].try_into().unwrap()),
                txn_counter: u64::from_le_bytes(buf[8..16].try_into().unwrap()),
                root_handle_table_page: 0,
                root_freemap_page: 0,
                total_pages: 0,
                next_handle: 0,
                page_size: u32::from_le_bytes(buf[48..52].try_into().unwrap()),
                named_roots: [NamedRoot::EMPTY; NAMED_ROOT_COUNT],
                superblock_count,
                root_membership_index_page: 0,
                freemap_depth: 0,
                encryption,
            });
        }
        // Plaintext path: all fields are directly readable.
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
        let superblock_count = u32::from_le_bytes(
            buf[SUPERBLOCK_COUNT_OFFSET..SUPERBLOCK_COUNT_OFFSET + 4]
                .try_into()
                .unwrap(),
        );
        Some(Superblock {
            magic: u32::from_le_bytes(buf[0..4].try_into().unwrap()),
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
            freemap_depth: u32::from_le_bytes(
                buf[FREEMAP_DEPTH_OFFSET..FREEMAP_DEPTH_OFFSET + 4]
                    .try_into()
                    .unwrap(),
            ),
            encryption: None,
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

    /// Explain why every candidate slot failed. Called only on the cold path,
    /// when `select` returned `None` — which means EVERY candidate failed
    /// `validate` (none deserialized), so this returns one `SlotDefect` per
    /// genuine superblock slot.
    ///
    /// The `buffers` slice may contain up to `MAX_SUPERBLOCKS` pages because
    /// the open path reads blindly up to that limit without first knowing N.
    /// Pages at indices >= the actual superblock_count are ordinary data
    /// pages, not superblock slots — including them in the defect list would
    /// falsely label intact data pages as corrupt superblocks.
    ///
    /// To recover the true count without a valid superblock, we scan each
    /// buffer's raw superblock_count field (byte offset 308). The first value
    /// in `MIN_SUPERBLOCKS..=MAX_SUPERBLOCKS` is used as the upper bound.
    /// If no buffer yields a plausible count (every slot is so badly mangled
    /// that even the count field is garbage), we fall back to `MIN_SUPERBLOCKS`:
    /// any real database has at least that many slots, so slots 0..MIN are
    /// always legitimate candidates to report and we never under-report.
    pub(crate) fn diagnose(buffers: &[[u8; PAGE_SIZE]]) -> Vec<SlotDefect> {
        let bound = buffers
            .iter()
            .map(|b| {
                u32::from_le_bytes(
                    b[SUPERBLOCK_COUNT_OFFSET..SUPERBLOCK_COUNT_OFFSET + 4]
                        .try_into()
                        .unwrap(),
                )
            })
            .find(|&n| (MIN_SUPERBLOCKS..=MAX_SUPERBLOCKS).contains(&n))
            .unwrap_or(MIN_SUPERBLOCKS) as usize;

        buffers[..bound.min(buffers.len())]
            .iter()
            .enumerate()
            .filter_map(|(i, b)| {
                validate(b).err().map(|defect| SlotDefect {
                    slot: i as u32,
                    defect,
                })
            })
            .collect()
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
            freemap_depth: 0,
            encryption: None,
        }
    }

    /// Like `new_empty`, but stamps MAJOR=2 and embeds the crypto-header so
    /// `serialize_encrypted` can seal the body. Called exclusively from the
    /// `create_new` encrypted path; the `CryptoHeader` carries the wrapped DEK
    /// in slot 0 and is written in cleartext into the superblock's reserved region.
    pub fn new_empty_encrypted(superblock_count: u32, header: CryptoHeader) -> Superblock {
        Superblock {
            magic: MAGIC,
            format_version: page::format_version_encrypted(),
            txn_counter: (superblock_count - 1) as u64,
            root_handle_table_page: page::PAGE_ID_NONE,
            root_freemap_page: page::PAGE_ID_NONE,
            total_pages: superblock_count as u64,
            next_handle: 1,
            page_size: PAGE_SIZE as u32,
            named_roots: [NamedRoot::EMPTY; NAMED_ROOT_COUNT],
            superblock_count,
            root_membership_index_page: page::PAGE_ID_NONE,
            freemap_depth: 0,
            encryption: Some(header),
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

    #[test]
    fn validate_classifies_each_defect() {
        let good = Superblock::new_empty(2).serialize();
        assert_eq!(validate(&good), Ok(()));

        let mut bad_checksum = good;
        bad_checksum[16] ^= 0xFF;
        assert_eq!(validate(&bad_checksum), Err(SuperblockDefect::BadChecksum));

        let mut bad_magic = good;
        bad_magic[0] ^= 0xFF;
        page::stamp_checksum(&mut bad_magic);
        assert_eq!(validate(&bad_magic), Err(SuperblockDefect::BadMagic));

        let mut bad_count = good;
        bad_count[SUPERBLOCK_COUNT_OFFSET..SUPERBLOCK_COUNT_OFFSET + 4]
            .copy_from_slice(&99u32.to_le_bytes());
        page::stamp_checksum(&mut bad_count);
        assert_eq!(validate(&bad_count), Err(SuperblockDefect::BadCount(99)));
    }

    #[test]
    fn diagnose_reports_each_slots_defect() {
        let good = Superblock::new_empty(2).serialize();
        let mut slot0 = good;
        slot0[16] ^= 0xFF;
        let mut slot1 = good;
        slot1[0] ^= 0xFF;
        page::stamp_checksum(&mut slot1);
        assert_eq!(
            Superblock::diagnose(&[slot0, slot1]),
            vec![
                SlotDefect {
                    slot: 0,
                    defect: SuperblockDefect::BadChecksum
                },
                SlotDefect {
                    slot: 1,
                    defect: SuperblockDefect::BadMagic
                },
            ]
        );
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
            freemap_depth: 0,
            encryption: None,
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
            freemap_depth: 0,
            encryption: None,
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
            freemap_depth: 0,
            encryption: None,
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
            freemap_depth: 0,
            encryption: None,
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
            freemap_depth: 0,
            encryption: None,
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

    #[test]
    fn freemap_depth_round_trips_and_defaults_zero() {
        let mut sb = Superblock::new_empty(2);
        sb.root_freemap_page = 9;
        sb.freemap_depth = 3;
        let buf = sb.serialize();
        let back = Superblock::deserialize(&buf).unwrap();
        assert_eq!(back.freemap_depth, 3);
        assert_eq!(back.root_freemap_page, 9);

        let mut legacy = sb.serialize();
        legacy[320..324].fill(0);
        page::stamp_checksum(&mut legacy);
        let back0 = Superblock::deserialize(&legacy).unwrap();
        assert_eq!(back0.freemap_depth, 0);
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
    // ── Encrypted-superblock tests (Task 2.2) ──

    /// Full encrypt→serialize→deserialize→decrypt round-trip: sensitive fields
    /// must survive the seal/open cycle and must not appear in the raw bytes.
    #[test]
    fn encrypted_superblock_hides_sensitive_fields_and_round_trips() {
        use crate::crypto::{random_dek, PageCipher};

        let cipher = PageCipher::new(random_dek());
        let mut header_slots = [KeySlot::EMPTY; KEY_SLOT_COUNT];
        header_slots[0].state = 1; // mark one slot active (simulates a real key-slot)
        let header = CryptoHeader {
            algorithm: ALGO_XCHACHA20POLY1305,
            stride: 8232,
            slots: header_slots,
        };

        let mut sb = Superblock::new_empty(DEFAULT_SUPERBLOCK_COUNT);
        sb.root_handle_table_page = 7;
        sb.next_handle = 99;
        sb.total_pages = 41;
        sb.named_roots[0].name[..5].copy_from_slice(b"users");
        sb.named_roots[0].handle = 12345;
        sb.encryption = Some(header);

        let buf = sb.serialize_encrypted(&cipher);

        // Sensitive bytes must be absent from cleartext.
        // named_roots occupy 52..308; all must be zero in the encrypted page.
        assert_eq!(&buf[52..308], &[0u8; 256][..], "named_roots leaked in cleartext");
        // Scalar sensitive fields at 16..48 must be zero.
        assert_eq!(&buf[16..48], &[0u8; 32][..], "sensitive scalars leaked");
        // root_membership_index_page (312..320) and freemap_depth (320..324)
        // are also sealed-only, so their plaintext slots must be zero. Bytes
        // 308..312 (superblock_count) are legitimately cleartext and skipped.
        assert_eq!(&buf[312..324], &[0u8; 12][..], "membership/freemap_depth leaked");
        // Bootstrap fields stay plaintext.
        assert_eq!(u32::from_le_bytes(buf[0..4].try_into().unwrap()), MAGIC);
        assert_eq!(
            u64::from_le_bytes(buf[8..16].try_into().unwrap()),
            sb.txn_counter
        );

        // Two-phase deserialize: sensitive fields are zero after deserialize.
        let mut back = Superblock::deserialize(&buf).expect("encrypted sb deserializes");
        assert!(back.encryption.is_some(), "encryption field must be populated");
        assert_eq!(back.root_handle_table_page, 0, "not yet decrypted");
        assert_eq!(back.next_handle, 0, "not yet decrypted");

        // After decrypt_body the sensitive fields are restored.
        back.decrypt_body(&cipher, &buf).expect("DEK opens body");
        assert_eq!(back.root_handle_table_page, 7);
        assert_eq!(back.next_handle, 99);
        assert_eq!(back.total_pages, 41);
        assert_eq!(&back.named_roots[0].name[..5], b"users");
        assert_eq!(back.named_roots[0].handle, 12345);
    }

    /// Wrong DEK must produce a CryptoError (authentication failure), not
    /// silently corrupt the sensitive fields.
    #[test]
    fn wrong_dek_fails_body_authentication() {
        use crate::crypto::{random_dek, PageCipher};

        let cipher = PageCipher::new(random_dek());
        let mut header_slots = [KeySlot::EMPTY; KEY_SLOT_COUNT];
        header_slots[0].state = 1;
        let header = CryptoHeader {
            algorithm: ALGO_XCHACHA20POLY1305,
            stride: 8232,
            slots: header_slots,
        };
        let mut sb = Superblock::new_empty(DEFAULT_SUPERBLOCK_COUNT);
        sb.encryption = Some(header);
        let buf = sb.serialize_encrypted(&cipher);

        let wrong = PageCipher::new(random_dek());
        let mut back = Superblock::deserialize(&buf).unwrap();
        assert!(back.decrypt_body(&wrong, &buf).is_err());
    }

    /// A forged ct_len (the XXH3 page checksum is non-cryptographic, so it
    /// cannot protect it) must surface as a recoverable Err, never a panic on
    /// the slice. Regression guard for the out-of-bounds slice fixed in review.
    #[test]
    fn forged_ct_len_returns_err_not_panic() {
        use crate::crypto::{random_dek, PageCipher};

        let cipher = PageCipher::new(random_dek());
        let mut header_slots = [KeySlot::EMPTY; KEY_SLOT_COUNT];
        header_slots[0].state = 1;
        let header = CryptoHeader {
            algorithm: ALGO_XCHACHA20POLY1305,
            stride: 8232,
            slots: header_slots,
        };
        let mut sb = Superblock::new_empty(DEFAULT_SUPERBLOCK_COUNT);
        sb.encryption = Some(header);
        let mut buf = sb.serialize_encrypted(&cipher);

        // Overwrite the 2-byte ct_len field with 0xFFFF (65535), which would
        // slice far past CHECKSUM_OFFSET, then re-stamp the checksum so the
        // page validates (simulating an attacker who recomputed XXH3).
        let len_off = SEALED_BODY_OFFSET + NONCE_LEN + TAG_LEN;
        buf[len_off..len_off + 2].copy_from_slice(&0xFFFFu16.to_le_bytes());
        page::stamp_checksum(&mut buf);

        let mut back = Superblock::deserialize(&buf).unwrap();
        // Must return Err, not panic.
        assert!(back.decrypt_body(&cipher, &buf).is_err());
    }

    /// Plaintext DBs must serialize byte-identically to the pre-encryption
    /// implementation (regression guard: `encryption: None` path is unchanged).
    #[test]
    fn plaintext_superblock_round_trips_unchanged() {
        let mut sb = Superblock::new_empty(DEFAULT_SUPERBLOCK_COUNT);
        sb.root_handle_table_page = 5;
        sb.root_freemap_page = 6;
        sb.total_pages = 20;
        sb.next_handle = 3;
        sb.named_roots[0].name[..4].copy_from_slice(b"test");
        sb.named_roots[0].handle = 42;

        let buf = sb.serialize();
        let back = Superblock::deserialize(&buf).expect("plaintext must deserialize");
        assert!(back.encryption.is_none());
        assert_eq!(back.root_handle_table_page, 5);
        assert_eq!(back.named_roots[0].handle, 42);
        assert_eq!(&back.named_roots[0].name[..4], b"test");
    }

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
                freemap_depth: 0,
                encryption: None,
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
            prop_assert_eq!(parsed.freemap_depth, sb.freemap_depth);
            prop_assert_eq!(parsed.encryption, sb.encryption);
            for i in 0..NAMED_ROOT_COUNT {
                prop_assert_eq!(parsed.named_roots[i].name, sb.named_roots[i].name);
                prop_assert_eq!(parsed.named_roots[i].handle, sb.named_roots[i].handle);
            }
        }
    }
}
