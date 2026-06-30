//! transaction::recovery — the two `TransactionManager` constructors:
//! `create_new` (fresh database, staggered superblock bank) and
//! `open_existing` (recover the highest valid superblock). Split out of
//! `transaction.rs` verbatim; see the parent module for the type and fields.

use super::*;

impl TransactionManager {
    /// Create a new database with `superblock_count` superblock slots.
    ///
    /// All N slots are initialized as VALID superblocks at staggered
    /// counters 0..N-1 (slot i gets counter N-1-i). This matters for
    /// crash safety (ISSUES.md I2 + R4):
    ///
    /// * The I2 fix for N=2: if the first user commit (which writes
    ///   slot 0 at counter N) is torn, slot 1 at counter N-2 still
    ///   holds a valid "empty database" superblock so the file stays
    ///   openable.
    /// * The R4 generalization for N>=3: multiple staggered fallback
    ///   slots survive CONSECUTIVE torn writes. For N=3, slots 1 and 2
    ///   both hold valid empty states at lower counters after slot 0
    ///   is written; a torn retry of the same commit still has slot 2
    ///   to fall back to.
    ///
    /// An fsync is issued before returning so the whole bank of slots
    /// is durable before any user data is written. Slot counters with
    /// the value 0 are SAFE even though zero bits are "the natural
    /// value of an uninitialized disk region" because `select()`
    /// filters on XXH3 checksum validity BEFORE comparing counters —
    /// a legitimate counter-0 slot has a valid checksum; a zeroed
    /// region doesn't.
    pub fn create_new(
        mut cache: PageCache,
        superblock_count: u32,
        key: Option<crate::crypto::Key>,
    ) -> Result<TransactionManager> {
        // Caller is expected to have validated bounds via Options in
        // lib.rs, but defend against direct-call misuse too.
        assert!(
            (2..=MAX_SUPERBLOCKS).contains(&superblock_count),
            "superblock_count {superblock_count} out of supported range 2..=16"
        );

        // Build the per-session cipher up front for an encrypted DB: a fresh
        // random DEK is sealed into slot 0 under a KEK derived from `key`.
        // For plaintext DBs this is None and the slot-write loop uses serialize().
        let create_crypto = match key {
            None => None,
            Some(k) => Some(build_create_cipher(&k)?),
        };

        // Write N staggered slots. Slot 0 gets the highest counter
        // (superblock_count - 1), slot N-1 gets 0. First user commit
        // bumps to N, which modulo N is 0, so slot 0 is the first to
        // be overwritten — the behavior the I2 fix established for
        // N=2 generalizes cleanly to larger N.
        //
        // Invariant after this loop: every slot is a valid superblock
        // referencing the same (empty) roots, at counters 0..N-1.
        // `select()` at open time will pick slot 0 (highest counter).
        // After the first user commit, slot 0 holds the newest data
        // and the rest remain as "rollback fallbacks".
        let mut sb = match &create_crypto {
            None => Superblock::new_empty(superblock_count),
            Some(cc) => Superblock::new_empty_encrypted(superblock_count, cc.header),
        };
        for i in 0..superblock_count {
            sb.txn_counter = (superblock_count - 1 - i) as u64;
            let buf = match &create_crypto {
                None => sb.serialize(),
                Some(cc) => sb.serialize_encrypted(&cc.page_cipher),
            };
            cache.io_mut().write_page(i as u64, &buf)?;
        }
        cache.io_mut().fsync()?;
        cache.set_next_page_id(superblock_count as u64);

        let roots = Roots {
            handle_table_page: PAGE_ID_NONE,
            // No freemap tree yet: the first allocation falls through to extend
            // (nothing to reuse) and the first free materializes the tree lazily
            // (persist_freemap calls FreeMapTree::create). Depth 0 matches the
            // single-leaf format.
            freemap_page: PAGE_ID_NONE,
            freemap_depth: 0,
            // Start at 1: handle 0 is reserved as the "no handle" sentinel and is
            // never minted (see Superblock::new_empty, which seeds the persisted
            // superblock the same way). Must match new_empty so the in-memory
            // roots and the on-disk superblock of a fresh store agree.
            next_handle: 1,
            total_pages: superblock_count as u64,
            named_roots: [NamedRoot::EMPTY; NAMED_ROOT_COUNT],
            membership_index_page: PAGE_ID_NONE,
        };

        // Split the CreateCrypto struct into its session parts before consuming.
        let (session_cipher, session_header) = match create_crypto {
            None => (None, None),
            Some(cc) => (Some(cc.page_cipher), Some(cc.header)),
        };

        Ok(TransactionManager {
            cache: RefCell::new(cache),
            committed_roots: roots.clone(),
            current_roots: roots,
            handle_table: HandleTable::new(),
            membership_index: MembershipIndex::new(),
            // Slot 0 was written last in the loop above, at counter
            // (superblock_count - 1 - 0) = superblock_count - 1. That's
            // the highest counter and therefore the winner on select().
            txn_counter: (superblock_count - 1) as u64,
            superblock_count,
            active_txn: false,
            savepoints: Vec::new(),
            txn_freed_pages: Vec::new(),
            freemap: freemap::FreemapRecycle::new(),
            // A fresh database has no data pages and no live slots yet.
            packer: packing::SlotPacker::new(),
            poisoned: Cell::new(false),
            cipher: session_cipher,
            crypto_header: session_header,
            #[cfg(test)]
            fault: fault::FaultInjector::default(),
        })
    }

    /// Open an existing database from file.
    ///
    /// This is the crash recovery path. All N superblock slots (where
    /// N is discovered from disk — see the probe below) are read,
    /// `Superblock::select()` picks the one with the highest
    /// txn_counter and a valid XXH3 checksum, and a torn write to the
    /// most-recently-targeted slot silently falls back to the next
    /// best survivor. No log replay required.
    ///
    /// R4 slot discovery:
    ///
    /// The number of superblock slots is NOT a compile-time constant.
    /// A database created with `superblock_count=4` has 4 slots at
    /// pages 0..3; a default database has 2 at pages 0..1. To find N
    /// without any external hint we:
    ///
    ///   1. Read the first MAX_SUPERBLOCKS pages of the file (bounded
    ///      by EOF — a fresh DB has exactly N pages and no more). We
    ///      deliberately do NOT short-circuit on "this page doesn't
    ///      look like a superblock": a torn write that hit the magic
    ///      bytes of an otherwise-valid slot would look like garbage,
    ///      and short-circuiting would skip past the legitimate
    ///      successor slots. Reading a few extra pages is cheap.
    ///   2. Pass all candidates to `Superblock::select`, which uses
    ///      `deserialize` to filter on XXH3 checksum + MAGIC bytes.
    ///      Data pages that happen to sit at positions < MAX_SUPERBLOCKS
    ///      (e.g., in a database where N=2 and there's a data page at
    ///      page 2) fail the magic check and are harmlessly ignored.
    ///   3. The winner's `superblock_count` field tells us N, which we
    ///      cache on the TransactionManager for commit-time slot
    ///      selection.
    ///
    /// If no valid superblock is found in the first MAX_SUPERBLOCKS
    /// pages, we return `CorruptSuperblock`. This bounds the probe
    /// cost in the pathological case where every candidate is torn.
    pub fn open_existing(
        mut cache: PageCache,
        key: Option<crate::crypto::Key>,
    ) -> Result<TransactionManager> {
        // Step 1: read up to MAX_SUPERBLOCKS pages as candidates.
        let mut candidates: Vec<[u8; PAGE_SIZE]> = Vec::new();
        for i in 0..MAX_SUPERBLOCKS as u64 {
            // If the file is shorter than MAX_SUPERBLOCKS (fresh DB
            // with small N), read_page returns InvalidPageId (I16).
            // Stop probing at EOF.
            match cache.io_mut().read_page(i) {
                Ok(buf) => candidates.push(buf),
                Err(ChiselError::InvalidPageId { .. }) => break,
                Err(e) => return Err(e),
            }
        }

        // Step 2 + 3: pick the winner via select(). select() uses
        // deserialize, which validates checksum and magic — data
        // pages in the candidate list (if any) are filtered out.
        let mut sb = Superblock::select(&candidates).ok_or_else(|| ChiselError::CorruptSuperblock {
            defects: Superblock::diagnose(&candidates),
        })?;

        // Encryption gate: the winning superblock's crypto-header (already
        // parsed by deserialize into sb.encryption) tells us whether the DB
        // is encrypted. Mismatches between "DB encrypted?" and "key
        // supplied?" are operational open errors, not torn-slot signals.
        //
        // For an encrypted DB we must decrypt the sealed body (which holds
        // total_pages, named_roots, etc.) BEFORE the page-size and
        // total_pages checks that read those fields.
        let cipher = match (&sb.encryption, &key) {
            (None, None) => None,
            (Some(_), None) => return Err(ChiselError::NoEncryptionKey),
            (None, Some(_)) => return Err(ChiselError::EncryptionNotSupported),
            (Some(header), Some(k)) => {
                // The winning slot's raw bytes: slot index is
                // txn_counter % superblock_count, which is how commit
                // selects the write slot, so the highest-counter slot is
                // always at this index in the candidates array.
                let slot_idx =
                    (sb.txn_counter % sb.superblock_count as u64) as usize;
                let raw = &candidates[slot_idx];
                let dek = unwrap_first_matching_slot(header, k)?;
                let cipher = crate::crypto::PageCipher::new(dek);
                // decrypt_body fills total_pages, named_roots, etc.
                // A tag failure here means corruption, not a wrong key
                // (the slot already authenticated the DEK), so map to
                // InvalidEncryptionKey rather than poisoning.
                sb.decrypt_body(&cipher, raw)
                    .map_err(|_| ChiselError::InvalidEncryptionKey)?;
                Some(cipher)
            }
        };

        // Format-version gate (see ISSUES.md I15 for the original check,
        // I29 for the major/minor split). Compare MAJOR only: the packed
        // u32 layout (upper 16 = major, lower 16 = minor) lets same-major
        // files open regardless of minor drift, which is what makes the
        // README's "sacred within a major version" promise enforceable.
        // Minor-newer files are accepted (read-compatible) here but forced
        // read-only by the I29 write-gate immediately below — writing them
        // would drop fields this binary doesn't know about.
        //
        // We validate AFTER select() rather than inside deserialize()
        // because the winning superblock's version is what determines
        // compatibility — silently falling back to an older-version
        // superblock would hand the user a stale snapshot with
        // mysteriously missing data.
        //
        // Encrypted DBs carry MAJOR=2; plaintext DBs carry MAJOR=1. An old
        // binary (FORMAT_MAJOR_VERSION=1) rejects MAJOR=2 as unsupported,
        // which is correct. A new binary accepts either, gated on whether
        // the encryption header is present.
        let expected_major = if sb.encryption.is_some() {
            page::FORMAT_MAJOR_VERSION_ENCRYPTED
        } else {
            page::FORMAT_MAJOR_VERSION
        };
        if page::format_major(sb.format_version) != expected_major {
            return Err(ChiselError::UnsupportedFormatVersion {
                found: sb.format_version,
                expected: page::FORMAT_VERSION,
            });
        }

        // Reject files written with a different page geometry before reading any
        // data pages — every page boundary calculation would be wrong if the page
        // size differed. The superblock.rs deserialize() reads the field but does
        // not validate it (a size mismatch is not a torn-slot signal; it must not
        // cause select() to fall back to a sibling slot). This is the right place
        // to raise it: after select() has picked the winning slot but before any
        // data is touched.
        if sb.page_size != PAGE_SIZE as u32 {
            return Err(ChiselError::UnsupportedPageSize {
                stored: sb.page_size,
                compiled: PAGE_SIZE as u32,
            });
        }

        // I29 write-gate: a file whose MINOR exceeds this binary's may contain
        // version-requiring page layouts we cannot safely write — we would
        // stamp pages at our older minor and drop the newer fields. Reads ARE
        // safe (within a MAJOR all layout changes are additive, so known fields
        // sit at stable offsets), so we open the file but force it read-only;
        // mutations then return ReadOnlyMode. The complementary I31 per-page
        // read-dispatch lets a newer binary read these older pages.
        // See docs/specs/2026-06-21-per-page-format-versioning-design.md.
        if page::format_minor(sb.format_version) > page::FORMAT_MINOR_VERSION {
            cache.io_mut().force_read_only();
        }

        let page_count = cache.io_mut().page_count()?;
        if page_count < sb.total_pages {
            return Err(ChiselError::FileSizeMismatch {
                // saturating_mul: `sb.total_pages` comes from a checksum-valid but
                // otherwise untrusted superblock — `Superblock::deserialize` bounds
                // only the checksum, MAGIC, and superblock_count, NOT total_pages.
                // A crafted/edited file with total_pages near u64::MAX would
                // overflow `* PAGE_SIZE` here: a panic in debug builds (how CI
                // runs) and a silent wrap in release. Saturating keeps the public
                // `Chisel::open` a typed-error path; "as many bytes as a u64 can
                // represent" is the right report for an absurd page count (mirrors
                // the I47 saturation in `Chisel::stats`/`file_size_bytes`).
                // `page_count` is file-length-bounded and cannot realistically
                // overflow, but it is saturated too for symmetry.
                expected: sb.total_pages.saturating_mul(PAGE_SIZE as u64),
                actual: page_count.saturating_mul(PAGE_SIZE as u64),
            });
        }
        // Reset next_page_id from the authoritative superblock, NOT from
        // the on-disk file length (ISSUES.md I4). This matters because a
        // crash mid-rollback could leave the file extended past the
        // committed superblock's `total_pages` — those trailing pages are
        // unreferenced garbage, and letting `new_page()` allocate above
        // them would mean the next commit's new pages live at the very
        // end of the file while the garbage sits in the middle. Reseeding
        // from `sb.total_pages` causes the next allocations to overwrite
        // the garbage, which is exactly what we want. The rollback-path
        // truncation added by I3 also prevents this situation from
        // arising in the first place, but the reseed is a defense-in-
        // depth guarantee against any crash that happened before I3 or
        // against external truncation/corruption tools.
        cache.set_next_page_id(sb.total_pages);

        let roots = Roots {
            handle_table_page: sb.root_handle_table_page,
            // The committed freemap tree IS {root, depth} from the superblock —
            // no separate in-memory mirror is loaded. Depth 0 (the default for
            // pre-multi-page databases) reaches today's single-leaf format.
            freemap_page: sb.root_freemap_page,
            freemap_depth: sb.freemap_depth,
            next_handle: sb.next_handle,
            total_pages: sb.total_pages,
            named_roots: sb.named_roots,
            // Normalize old files (pre-chunk-tags bytes were zeroed) so the
            // rest of the engine has a single "empty" sentinel: PAGE_ID_NONE.
            membership_index_page: if sb.root_membership_index_page == 0 {
                PAGE_ID_NONE
            } else {
                sb.root_membership_index_page
            },
        };

        // The HandleTable struct keeps only its depth in memory; physical pages
        // live in the cache, reached via the root page_id in the superblock.
        // Reconstruct depth by walking the left spine (see HandleTable::recover_depth).
        let mut ht = HandleTable::new();
        ht.set_depth(HandleTable::recover_depth(
            &mut cache,
            sb.root_handle_table_page,
        )?);

        // Mirror the handle-table depth recovery for the membership index: its
        // outer RadixU64 keeps only depth in memory, rebuilt by walking the
        // persisted spine from the root recorded in the superblock. Uses the
        // normalized roots.membership_index_page (PAGE_ID_NONE for legacy files).
        let mut membership_index = MembershipIndex::new();
        if roots.membership_index_page != PAGE_ID_NONE {
            let depth = RadixU64::recover_depth(&mut cache, roots.membership_index_page)?;
            membership_index.set_outer_depth(depth);
        }

        // The committed freemap tree is reconstructed on demand from
        // {root_freemap_page, freemap_depth} via FreeMapTree::from_roots — no
        // eager in-memory mirror is loaded here. A DB created under v1 (pre-R2)
        // or a fresh one has root_freemap_page == PAGE_ID_NONE, which
        // from_roots treats as the empty (nothing-free) tree. Tree pages are
        // checksum-validated on cache miss as they are descended, so a torn or
        // corrupt freemap surfaces as a fatal error rather than silent reuse.

        // Rebuild the live-slot count map (ISSUES.md R1) by scanning the
        // handle table. Every Live entry contributes one live slot to
        // its target data page; Overflow and Deleted entries don't
        // count. Cost is O(live handles), paid once at open. In-memory
        // only — the alternative (storing the count on the data page
        // itself) would require COWing pages on every delete, which
        // shadow paging cannot afford.
        let mut committed_live_slots: FxHashMap<u64, u32> = FxHashMap::default();
        if sb.root_handle_table_page != PAGE_ID_NONE {
            let entries = ht.iter_live(&mut cache, sb.root_handle_table_page)?;
            for (_, entry) in entries {
                if entry.flags == HandleFlags::Live {
                    *committed_live_slots.entry(entry.page_id).or_insert(0) += 1;
                }
            }
        }
        Ok(TransactionManager {
            cache: RefCell::new(cache),
            committed_roots: roots.clone(),
            current_roots: roots,
            handle_table: ht,
            membership_index,
            txn_counter: sb.txn_counter,
            // R4: discovered from the winning superblock's own
            // `superblock_count` field. Cached so commit doesn't have
            // to re-look it up for slot selection.
            superblock_count: sb.superblock_count,
            active_txn: false,
            savepoints: Vec::new(),
            txn_freed_pages: Vec::new(),
            freemap: freemap::FreemapRecycle::new(),
            packer: packing::SlotPacker::from_committed(committed_live_slots),
            poisoned: Cell::new(false),
            cipher,
            // The CryptoHeader (key-slot table + algorithm) is preserved from the
            // winning superblock so commit can write it back verbatim. It never
            // changes between opens: slots are only mutated by key-rotation (a
            // future operation). None for plaintext DBs.
            crypto_header: sb.encryption,
            #[cfg(test)]
            fault: fault::FaultInjector::default(),
        })
    }
}

/// Output of the create-time key setup: the live cipher for this session and
/// the crypto-header to stamp into every superblock slot.
struct CreateCrypto {
    page_cipher: crate::crypto::PageCipher,
    header: crate::superblock::CryptoHeader,
}

/// Build the session PageCipher for a freshly-created encrypted DB: generate a
/// random DEK + slot-0 salt, derive the KEK from the client key, wrap the DEK
/// into slot 0, and assemble the crypto-header. Returns the live PageCipher and
/// the header to stamp into every superblock slot.
///
/// The AAD passed to `wrap_dek` is `slot.aad()` — the same bytes that Task 2.4
/// reconstructs at unwrap time from the persisted slot fields. Keeping the AAD
/// construction in one place (`KeySlot::aad`) ensures wrap and unwrap agree.
fn build_create_cipher(key: &crate::crypto::Key) -> Result<CreateCrypto> {
    use crate::crypto::{
        derive_kek, random_array, random_dek, wrap_dek, Argon2Params, KdfId, NONCE_LEN, SALT_LEN,
    };
    use crate::superblock::{CryptoHeader, KeySlot, ALGO_XCHACHA20POLY1305, KEY_SLOT_COUNT};

    let dek = random_dek();
    let salt: [u8; SALT_LEN] = random_array();
    let wrap_nonce: [u8; NONCE_LEN] = random_array();

    // KDF choice: a Raw key uses HKDF (fast, key-material quality);
    // a Passphrase uses Argon2id (memory-hard, brute-force resistant).
    let (kdf, params) = match key {
        crate::crypto::Key::Raw(_) => (KdfId::Hkdf, Argon2Params::default()),
        crate::crypto::Key::Passphrase(_) => (KdfId::Argon2id, Argon2Params::default()),
    };
    let kek = derive_kek(key, kdf, &salt, &params)?;

    // Populate slot 0: state=active, KDF metadata, the wrapped DEK.
    // slot.aad() is the canonical AAD bytes; it MUST be the same value
    // used by Task 2.4 to unwrap — both sides call KeySlot::aad() on
    // the populated-but-pre-wrap slot so the bytes are identical.
    let mut slot = KeySlot::EMPTY;
    slot.state = 1; // active
    slot.kdf_id = kdf as u8;
    slot.argon2 = params;
    slot.salt = salt;
    slot.wrap_nonce = wrap_nonce;
    let aad = slot.aad();
    let (wrapped, tag) = wrap_dek(&kek, &dek, &wrap_nonce, &aad);
    slot.wrapped_dek = wrapped;
    slot.wrap_tag = tag;

    let mut slots = [KeySlot::EMPTY; KEY_SLOT_COUNT];
    slots[0] = slot;
    let header = CryptoHeader { algorithm: ALGO_XCHACHA20POLY1305, stride: 8232, slots };

    Ok(CreateCrypto { page_cipher: crate::crypto::PageCipher::new(dek), header })
}

/// Try every ACTIVE key-slot in turn: derive the KEK from `key` + the slot's
/// salt/params, then attempt to unwrap the DEK. The first slot whose AEAD tag
/// verifies yields the DEK. If no slot matches, the caller's key is wrong.
///
/// Trying every active slot (rather than a slot-index hint) is what makes
/// multi-key support possible: a DB may have the same DEK wrapped under
/// several KEKs (one per trusted key), and the caller's key matches exactly
/// one of them.
///
/// The AAD passed to `unwrap_dek` is `slot.aad()` — the identical bytes
/// that `build_create_cipher` used at wrap time. Both sides call
/// `KeySlot::aad()` on the fully populated (but pre-wrap) slot, so the
/// bytes agree even if the slot layout changes in a future format version.
fn unwrap_first_matching_slot(
    header: &crate::superblock::CryptoHeader,
    key: &crate::crypto::Key,
) -> Result<crate::crypto::Dek> {
    use crate::crypto::{derive_kek, unwrap_dek, KdfId};

    for slot in header.slots.iter().filter(|s| s.is_active()) {
        let kdf = match slot.kdf_id {
            1 => KdfId::Hkdf,
            2 => KdfId::Argon2id,
            _ => continue, // unknown KDF id: skip, treat as non-matching
        };
        let kek = match derive_kek(key, kdf, &slot.salt, &slot.argon2) {
            Ok(k) => k,
            Err(_) => continue,
        };
        let aad = slot.aad();
        if let Ok(dek) =
            unwrap_dek(&kek, &slot.wrapped_dek, &slot.wrap_tag, &slot.wrap_nonce, &aad)
        {
            return Ok(dek);
        }
    }
    Err(ChiselError::InvalidEncryptionKey)
}
