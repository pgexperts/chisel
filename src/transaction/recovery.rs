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
        argon2_params: Option<crate::crypto::Argon2Params>,
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
            Some(k) => Some(build_create_cipher(&k, argon2_params)?),
        };

        // Uniform-stride layout (encryption spec): an encrypted DB uses the
        // ENC_PAGE_SIZE (8232) stride for EVERY page — superblock slots
        // included — FROM BIRTH. Set the stride BEFORE writing the initial
        // slots so the file is a clean multiple of 8232 (= total_pages *
        // stride). Without this, fresh slots would be written at PAGE_SIZE and
        // a never-committed encrypted DB's file length (N*8192) would not
        // divide evenly by the open-time stride (8232), breaking the
        // page_count == total_pages invariant that open_existing relies on.
        // A superblock slot still holds the 8192-byte superblock image; the
        // trailing 40 bytes of its 8232-byte unit stay zero. The cipher is
        // installed on the cache AFTER this loop so data-page writes seal
        // through PageCache::write_sealed (superblock images are NOT
        // PageCipher-sealed — their body is sealed by serialize_encrypted).
        if create_crypto.is_some() {
            cache.io_mut().set_stride(crate::crypto::ENC_PAGE_SIZE);
        }

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
            match &create_crypto {
                None => {
                    // Plaintext: stride == PAGE_SIZE, write_page is correct.
                    let buf = sb.serialize();
                    cache.io_mut().write_page(i as u64, &buf)?;
                }
                Some(cc) => {
                    // Encrypted: zero-pad the 8192-byte image into an 8232-byte
                    // unit (trailing 40 bytes stay zero) and write at the now-set
                    // ENC_PAGE_SIZE stride. write_page would reject this stride —
                    // trips its debug_assert, and returns IoError from
                    // write_page_unit in release.
                    let buf = sb.serialize_encrypted(&cc.page_cipher);
                    let mut unit = [0u8; crate::crypto::ENC_PAGE_SIZE];
                    unit[..buf.len()].copy_from_slice(&buf);
                    cache.io_mut().write_page_unit(i as u64, &unit)?;
                }
            }
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
        // The stride was already set to ENC_PAGE_SIZE above (before the slot
        // loop) for the uniform-stride layout. Here we only install the cipher
        // on the cache so subsequent DATA-page writes seal through
        // PageCache::write_sealed. Superblock images are NOT PageCipher-sealed
        // (their body is sealed by serialize_encrypted), which is why the slot
        // loop above wrote them directly rather than through the cache.
        let (session_cipher, session_header) = match create_crypto {
            None => (None, None),
            Some(cc) => {
                // Clone: both the session manager and the page cache need their
                // own PageCipher instance. Both hold independent Zeroizing DEK
                // copies and wipe independently on drop.
                cache.set_cipher(cc.page_cipher.clone());
                (Some(cc.page_cipher), Some(cc.header))
            }
        };

        Ok(TransactionManager {
            cache: RefCell::new(cache),
            committed_roots: roots.clone(),
            current_roots: roots,
            handle_table: HandleTable::new(),
            membership_index: MembershipIndex::new(),
            // Slot 0 was written first in the loop above (i=0), at counter
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
        // Step 1: read up to MAX_SUPERBLOCKS slots as candidates.
        //
        // Bootstrap stride (encryption spec): an encrypted DB uses a uniform
        // ENC_PAGE_SIZE (8232) stride for EVERY page, superblock slots
        // included, but we do not yet know whether THIS file is encrypted, and
        // we must NOT assume page 0 is intact — the commit protocol overwrites
        // slot `txn_counter % N`, so slot 0 IS a torn-write target, and R4
        // crash recovery must still find a valid sibling. Each candidate slot
        // image must therefore be read at the file's true stride; reading an
        // encrypted slot at the wrong (8192) stride lands mid-unit and yields
        // garbage that would defeat sibling fallback.
        //
        // Probe the stride by scanning slots at the default PAGE_SIZE stride
        // first: a plaintext DB (all slots at 8192) deserializes immediately,
        // and an encrypted DB's page 0 lives at offset 0 so it deserializes at
        // 8192 too (its 40-byte trailing padding is ignored by deserialize). If
        // ANY slot deserializes with an encryption header, switch the IO stride
        // to header.stride and RE-READ all candidate slots at that stride so
        // siblings 1..N land on their true 8232-byte boundaries. The re-read is
        // what preserves torn-slot-0 recovery for encrypted DBs: even if page 0
        // is torn, a sibling read at 8232 supplies the header that unlocks the
        // correct stride. This switch also happens BEFORE the total_pages /
        // file-size validation below, so page_count = file_len/stride == N.
        let read_candidates = |cache: &mut PageCache| -> Result<Vec<[u8; PAGE_SIZE]>> {
            let mut out: Vec<[u8; PAGE_SIZE]> = Vec::new();
            for i in 0..MAX_SUPERBLOCKS as u64 {
                // read_page_unit honors the current stride and returns a
                // `stride`-byte unit; take the first PAGE_SIZE bytes as the
                // slot's superblock image (trailing encrypted padding ignored).
                // A file shorter than MAX_SUPERBLOCKS stops the probe at EOF.
                match cache.io_mut().read_page_unit(i) {
                    Ok(unit) => {
                        let mut buf = [0u8; PAGE_SIZE];
                        buf.copy_from_slice(&unit[..PAGE_SIZE]);
                        out.push(buf);
                    }
                    Err(ChiselError::InvalidPageId { .. }) => break,
                    Err(e) => return Err(e),
                }
            }
            Ok(out)
        };
        // Anchor on page 0: it always lives at byte offset 0 regardless of
        // stride, so read it at the default PAGE_SIZE stride (read_page reads
        // bytes 0..8192 = the page-0 image; an encrypted slot's 40-byte padding
        // is ignored by deserialize). An intact, encrypted page 0 tells us the
        // stride directly via its cleartext crypto-header — the common case.
        let page0 = cache.io_mut().read_page(0)?;
        if let Some(stride) =
            Superblock::deserialize(&page0).and_then(|sb| sb.encryption.map(|h| h.stride as usize))
        {
            // I144: the stride comes from the plaintext crypto-header, guarded
            // only by the forgeable (non-cryptographic) XXH3 page checksum — the
            // same trust boundary decrypt_body already bounds-checks ct_len at.
            // ENC_PAGE_SIZE is the only value ever written; reject anything else
            // BEFORE set_stride, which divides the file length by stride (a forged
            // 0 is a division-by-zero panic; a huge value drives multi-GiB read
            // allocations). Fail as CorruptSuperblock, honoring poison-not-panic.
            if stride != crate::crypto::ENC_PAGE_SIZE {
                return Err(ChiselError::CorruptSuperblock {
                    defects: Vec::new(),
                });
            }
            cache.io_mut().set_stride(stride);
        }
        // With an intact page 0 the stride is already correct here, so this
        // first read picks up every slot at its true boundary. The fallback
        // below only fires when page 0 is torn (see its comment).
        let mut candidates = read_candidates(&mut cache)?;
        // Helper: the encrypted stride advertised by the first candidate that
        // deserializes with an encryption header (None for plaintext/torn).
        let encrypted_stride = |cands: &[[u8; PAGE_SIZE]]| -> Option<usize> {
            cands
                .iter()
                .filter_map(Superblock::deserialize)
                .find_map(|sb| sb.encryption.map(|h| h.stride as usize))
        };
        if encrypted_stride(&candidates).is_none() && Superblock::deserialize(&page0).is_none() {
            // Torn-slot-0 recovery for encrypted DBs: when slot 0 is a torn
            // write, page 0 fails to deserialize so the anchor above could not
            // learn the stride, and the default-stride candidate scan finds
            // nothing (siblings 1..N sit at 8232 offsets — misaligned garbage
            // when read at 8192). Speculatively retry at the encrypted stride:
            // every slot carries the cleartext crypto-header, so an intact
            // sibling read at its true boundary supplies the stride and lets
            // select() fall back to it. If this read ALSO yields no encryption
            // header (a genuinely plaintext-but-torn file), restore the
            // default-stride candidates so the CorruptSuperblock diagnosis
            // below reflects the real bytes.
            cache.io_mut().set_stride(crate::crypto::ENC_PAGE_SIZE);
            let enc_candidates = read_candidates(&mut cache)?;
            if encrypted_stride(&enc_candidates).is_some() {
                candidates = enc_candidates;
            } else {
                cache.io_mut().set_stride(PAGE_SIZE);
            }
        }

        // Step 2 + 3: pick the winner. We need BOTH the deserialized superblock
        // AND the raw buffer it came from (for decrypt_body's AAD reconstruction),
        // plus the buffer's own position, which is the slot's page id
        // (read_candidates pushes units 0..N in order) and lets a body-decrypt
        // failure name the damaged slot. select() discards all of that, so we
        // re-select here as a (slot index, superblock, &buf) triple. Tie-break is
        // identical to select(): max_by_key returns the last maximum, matching the
        // plaintext path's behavior.
        //
        // IMPORTANT: do NOT reconstruct the buffer index from
        // `txn_counter % superblock_count`. That formula matches the commit
        // write-slot rule but is INVERTED for the create-seeding rule (slot i
        // gets counter N-1-i), so a freshly-created encrypted DB that has never
        // been committed would yield slot_idx pointing at the LOSER buffer —
        // decrypt_body builds its AAD from the winner's txn_counter but reads
        // the sealed body from the wrong buffer, causing an AAD mismatch →
        // CryptoError::Auth → the correct key wrongly fails to open.
        let (winner_slot, mut sb, raw) = candidates
            .iter()
            .enumerate()
            .filter_map(|(i, b)| Superblock::deserialize(b).map(|sb| (i as u64, sb, b)))
            .max_by_key(|(_, sb, _)| sb.txn_counter)
            .ok_or_else(|| ChiselError::CorruptSuperblock {
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
        // Checked BEFORE the key-presence match, so an unopenable-algorithm file
        // says so whether or not a key was supplied. Inside the match it would
        // sit behind `(Some(_), None) => NoEncryptionKey`, telling the user to
        // go find a key for a file that no key can open, and only revealing the
        // real reason once they produced one.
        if let Some(header) = &sb.encryption {
            if header.algorithm != crate::superblock::ALGO_XCHACHA20POLY1305 {
                return Err(ChiselError::EncryptionNotSupported);
            }
            // SUPERBLOCK-RECOVERY-7 (issue #118): validate the WINNER's stride,
            // whichever path found it. The anchor check above only ever inspects
            // page 0; when page 0 is torn the fallback hardcodes ENC_PAGE_SIZE and
            // calls `encrypted_stride` purely as an is-this-file-encrypted probe,
            // discarding the value. The same forged field therefore got two
            // different verdicts, decided by nothing but whether slot 0 happened
            // to be torn: rejected when it was intact, opened when it was not.
            //
            // Tolerating it is not harmless, because the forged value does not stay
            // in the slot we read it from: the TransactionManager below captures
            // this header verbatim into `crypto_header`, and every commit
            // republishes it into the slot it writes. Commit round-robins
            // (`txn_counter % N`), so after N commits every slot carries the forged
            // stride under a freshly stamped, valid checksum — and the NEXT open
            // takes the anchor path, rejects it, and has no clean sibling left to
            // fall back to. The engine would have bricked the file itself.
            //
            // Accepted narrowing: this also fires in a case the anchor never
            // covered — page 0 intact and clean, but a sibling with a higher
            // txn_counter carrying a forged stride wins selection. Such a file
            // opens today and will not after this check. It is tampered by
            // construction (XXH3 is non-cryptographic and publicly computable, so
            // producing this state needs write access to the file, not the key),
            // and the winning slot is the one whose sealed body we are about to
            // trust, so refusing is the honest answer rather than a regression.
            //
            // Keep the anchor check as well: it is the fast pre-check that keeps a
            // forged 0 or a huge value out of `set_stride`'s division (I144). This
            // one runs after the stride has already been applied, so it cannot
            // replace it.
            //
            // `defects: Vec::new()` mirrors the anchor path. The winner passed
            // checksum and magic by construction, so there is no slot defect that
            // explains this refusal — `validate` does not cover the crypto header.
            if header.stride as usize != crate::crypto::ENC_PAGE_SIZE {
                return Err(ChiselError::CorruptSuperblock {
                    defects: Vec::new(),
                });
            }
        }
        let cipher = match (&sb.encryption, &key) {
            (None, None) => None,
            (Some(_), None) => return Err(ChiselError::NoEncryptionKey),
            (None, Some(_)) => return Err(ChiselError::EncryptionNotSupported),
            (Some(header), Some(k)) => {
                // CRYPTO-4 / SUPERBLOCK-RECOVERY-3: the algorithm byte was
                // written but never read back. `deserialize` gates only on
                // zero ("not encrypted"), so ANY nonzero value was accepted and
                // fed to `PageCipher`, which is hardcoded to XChaCha20-Poly1305.
                //
                // The field exists precisely to prevent that. Without this check
                // a file stamped `algorithm = 2` — a future cipher, or a forged
                // byte — opens, and every page read then fails as
                // `DecryptionFailed`, which `is_fatal()` and poisons: an
                // algorithm mismatch reported to the user as data corruption.
                //
                // `EncryptionNotSupported` is the right variant rather than a new
                // one: its doc already covers "an encrypted DB is opened by a
                // build that the on-disk crypto-header algorithm id is unknown
                // to". It is operational, not fatal, which is correct — the file
                // is intact, this build simply cannot read it. (The check itself
                // now runs just above, before the key-presence match.)
                // `raw` is provably the buffer the winner was deserialized from —
                // correct regardless of create-seed vs commit write-slot ordering.
                //
                // `unlock` is the same trial loop `add_key`/`rotate_key`/
                // `remove_key` use. This site used to carry a second copy
                // (`unwrap_first_matching_slot`) that behaved identically but was
                // separately maintained; see `unlock`'s doc for why that was
                // worth collapsing even though this particular pair had not
                // drifted. The slot index is discarded here — the open path only
                // needs the DEK.
                let (_slot, dek) = header.unlock(k)?;
                let cipher = crate::crypto::PageCipher::new(dek);
                // decrypt_body fills total_pages, named_roots, etc.
                //
                // PUBLIC-API-7 (issue #119): a tag failure HERE is corruption,
                // not a wrong key, and the error must say so. An AEAD failure is
                // uniform in isolation — a wrong key and a tampered ciphertext
                // both just fail the tag check — so the discriminator is not the
                // failure but the SEQUENCING. `unlock` above is itself an AEAD
                // open, and its success is cryptographic proof that the supplied
                // credential derived the KEK that wrapped this DEK. Every
                // wrong-credential open dies there and never reaches this line.
                // What is left is a damaged or forged body, a body sealed under a
                // different DEK, or an AAD that no longer matches the cleartext
                // bootstrap fields — none of which another passphrase fixes.
                //
                // The honest limit, which this does NOT address: if the KEY SLOT
                // itself is damaged, `unlock` skips it as non-matching and a
                // CORRECT credential still surfaces as `InvalidEncryptionKey`.
                // That case genuinely is indistinguishable from a wrong key
                // (see `tampered_key_slot_cost_params_do_not_reach_the_allocator`).
                //
                // `DecryptionFailed`, not `CorruptSuperblock`: the latter means
                // "no readable superblock at all" and is documented as
                // recoverable by reopening, because slot selection may pick a
                // different slot next time. That is false here — `max_by_key` is
                // deterministic, so a reopen fails identically forever. Same call
                // the repo already made for `InvalidFreemapDepth`.
                //
                // The fatal classification poisons nothing on this path: no
                // TransactionManager exists yet, and the only POISON-relevant
                // `is_fatal()` consultation is `poison_on_fatal`, a method on a
                // live manager (transaction/lifecycle.rs). The Python binding
                // also reads `is_fatal()`, but only to pick an exception tier,
                // and `DecryptionFailed` has an explicit arm there. The previous
                // comment's "rather than poisoning" justified the mapping with a
                // mechanism that does not run here.
                sb.decrypt_body(&cipher, raw)
                    .map_err(|_| ChiselError::DecryptionFailed {
                        page_id: winner_slot,
                    })?;
                // Whenever page 0 and the winning slot agree the DB is encrypted,
                // the IO stride is already the constant ENC_PAGE_SIZE (so slots
                // 1..N and the file-size checks used the 8232-byte unit): either
                // set from page 0's crypto-header on the anchor path, which only
                // proceeds once that header's stride equals the constant, or
                // hardcoded by the torn-slot-0 fallback, which never consults the
                // header at all. Deliberately NOT "whatever header.stride said" —
                // the SUPERBLOCK-RECOVERY-7 check above proves the two are equal,
                // but the constant is what was actually applied to the IO layer.
                //
                // They can disagree only under a whole-slot transplant: page 0
                // deserializes as a valid PLAINTEXT superblock (so the anchor
                // never calls set_stride and the fallback never fires) while a
                // planted encrypted slot outranks it on txn_counter. The stride
                // is then still 8192 here and the reads below misalign into
                // DecryptionFailed. That needs file write access, costs
                // availability only, and predates this check.
                // Here we only install the cipher on the cache so subsequent
                // data-page reads go through PageCipher::open.
                cache.set_cipher(cipher.clone());
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
        //
        // SUPERBLOCK-RECOVERY-2 (issue #106): the MINOR expectation is derived
        // HERE, in the same branch, and not read off the plaintext constant
        // further down. The two MINOR series are independent — page.rs says so
        // outright: "A new MAJOR series starts its MINOR count at 0, so
        // encrypted DBs stamp (2, 0) — NOT (2, FORMAT_MINOR_VERSION)". The
        // write-gate below used to compare an encrypted file's minor against
        // the PLAINTEXT FORMAT_MINOR_VERSION (currently 1), so the first
        // encrypted minor bump — a file stamped (2, 1) by a newer binary —
        // evaluated `1 > 1` as false, the gate did not fire, and this binary
        // would have opened it read-write and committed superblocks stamped at
        // its own (2, 0), silently dropping every field the newer minor added.
        // That is exactly the data loss the gate exists to prevent; it failed
        // safe only by the accident of the encrypted minor being 0 today.
        //
        // Deriving all three expectations from one predicate is the point.
        // Three separate `if sb.encryption.is_some()` copies is how the minor
        // one came to be missed in the first place.
        let (expected_major, expected_minor, expected_version) = if sb.encryption.is_some() {
            (
                page::FORMAT_MAJOR_VERSION_ENCRYPTED,
                page::FORMAT_MINOR_VERSION_ENCRYPTED,
                page::format_version_encrypted(),
            )
        } else {
            (
                page::FORMAT_MAJOR_VERSION,
                page::FORMAT_MINOR_VERSION,
                page::FORMAT_VERSION,
            )
        };
        if page::format_major(sb.format_version) != expected_major {
            // `expected_version` reports the version the caller should expect
            // for THIS kind of DB (encrypted = MAJOR 2, plaintext = MAJOR 1);
            // reporting the plaintext FORMAT_VERSION for an encrypted file
            // would be misleading.
            return Err(ChiselError::UnsupportedFormatVersion {
                found: sb.format_version,
                expected: expected_version,
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

        // Same trust boundary, same reasoning: `freemap_depth` comes off the
        // superblock unvalidated (`deserialize` reads the field, `validate`
        // checks only checksum/magic/superblock_count) and is then handed
        // verbatim to `FreeMapTree::from_roots`. Depth 5 already covers every
        // u64 page id, so anything larger is corrupt by construction.
        //
        // This bound is load-bearing, not tidiness. Without it a hostile file
        // reaches two process-killing paths that the poison model cannot
        // intercept, because neither is a Rust error:
        //
        //   * `scan_node` recurses once per level. A forged depth of ~500k plus
        //     one self-referential FreeMapInterior page overflows the stack —
        //     SIGSEGV — from any allocation that consults the freemap, and from
        //     `defrag`'s `collect_reachable`.
        //   * `cow_descend` loops over the depth and calls `child_span`, itself
        //     O(level), inside — O(depth^2), ~10^19 operations at u32::MAX —
        //     while materializing a page per absent child, extending the file
        //     without bound. That one fires on the ordinary commit path.
        //
        // The handle table and the membership index both already cap their
        // depth this way; the freemap was the odd one out. Its own comment
        // claimed `capacity()` saturation "fails closed (rejects the descent)",
        // but saturation only prevents arithmetic overflow — it rejects nothing.
        if sb.freemap_depth > crate::freemap_tree::MAX_DEPTH {
            return Err(ChiselError::InvalidFreemapDepth {
                stored: sb.freemap_depth,
                max: crate::freemap_tree::MAX_DEPTH,
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
        //
        // `expected_minor` comes from the encryption-aware branch above, NOT
        // from page::FORMAT_MINOR_VERSION — the plaintext and encrypted MINOR
        // series are independent counts and must never be cross-compared.
        // See SUPERBLOCK-RECOVERY-2 there for what cross-comparing cost.
        if page::format_minor(sb.format_version) > expected_minor {
            cache.io_mut().force_read_only();
        }

        let page_count = cache.io_mut().page_count();
        if page_count < sb.total_pages {
            // Stride-correct byte counts: encrypted DBs use ENC_PAGE_SIZE
            // (8232) for every page including superblock slots, so multiply by
            // the CURRENT stride, not the hardcoded PAGE_SIZE. Both page_count
            // and total_pages are logical page counts under that uniform
            // stride; reporting them as PAGE_SIZE bytes would understate an
            // encrypted file's true size.
            let stride = cache.io_mut().stride() as u64;
            return Err(ChiselError::FileSizeMismatch {
                // saturating_mul: `sb.total_pages` comes from a checksum-valid but
                // otherwise untrusted superblock — `Superblock::deserialize` bounds
                // only the checksum, MAGIC, and superblock_count, NOT total_pages.
                // A crafted/edited file with total_pages near u64::MAX would
                // overflow `* stride` here: a panic in debug builds (how CI
                // runs) and a silent wrap in release. Saturating keeps the public
                // `Chisel::open` a typed-error path; "as many bytes as a u64 can
                // represent" is the right report for an absurd page count (mirrors
                // the I47 saturation in `Chisel::stats`/`file_size_bytes`).
                // `page_count` is file-length-bounded and cannot realistically
                // overflow, but it is saturated too for symmetry.
                expected: sb.total_pages.saturating_mul(stride),
                actual: page_count.saturating_mul(stride),
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
/// random DEK, wrap it into slot 0 under a KEK derived from the client key, and
/// assemble the crypto-header. Returns the live PageCipher and the header to
/// stamp into every superblock slot.
///
/// The wrap itself is `CryptoHeader::wrap_into`, the same function `add_key`
/// and `rotate_key` use. It used to be a second, hand-maintained implementation
/// here — and the duplication had already produced a real divergence (CRYPTO-6:
/// this copy wrote the OWASP argon2 defaults into HKDF slots, which the format
/// doc says must be zero, while `wrap_into` correctly wrote zeros). A comment
/// asserting two copies agree is not a mechanism that keeps them agreeing; one
/// function is.
fn build_create_cipher(
    key: &crate::crypto::Key,
    argon2_override: Option<crate::crypto::Argon2Params>,
) -> Result<CreateCrypto> {
    use crate::superblock::{CryptoHeader, KeySlot, ALGO_XCHACHA20POLY1305, KEY_SLOT_COUNT};

    let dek = crate::crypto::random_dek();
    let mut header = CryptoHeader {
        algorithm: ALGO_XCHACHA20POLY1305,
        stride: crate::crypto::ENC_PAGE_SIZE as u32,
        slots: [KeySlot::EMPTY; KEY_SLOT_COUNT],
    };
    header.wrap_into(0, key, &dek, argon2_override)?;

    Ok(CreateCrypto {
        page_cipher: crate::crypto::PageCipher::new(dek),
        header,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::{self, ENCRYPTED_FORMAT_VERSION, FORMAT_VERSION, PAGE_SIZE};
    use crate::superblock::Superblock;

    /// Prove that an encryption-unaware open (no key, no crypto header
    /// expected) hard-rejects a MAJOR=2 superblock with
    /// `UnsupportedFormatVersion`. This is the gate-rejection guarantee: an
    /// old binary (FORMAT_MAJOR_VERSION == 1) can never accidentally read an
    /// encrypted DB's 8232-byte-stride data as plaintext 8192-byte pages.
    ///
    /// The mechanism: `open_existing` computes `expected_major = if
    /// sb.encryption.is_some() { 2 } else { 1 }`. A MAJOR=2 file with no
    /// crypto header has `sb.encryption = None`, so `expected_major = 1`, but
    /// `format_major(sb.format_version) = 2` — mismatch → error.
    #[test]
    fn encrypted_major_version_is_rejected_by_plaintext_binary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("enc_major.chsl");

        // Build a valid plaintext superblock (superblock_count=2 for the
        // default layout), then overwrite its format_version with the
        // encrypted-DB MAJOR=2 value. Everything else is a normal empty DB —
        // in particular, `sb.encryption` stays None, so the gate's
        // `expected_major` resolves to 1 (plaintext), not 2.
        let mut sb = Superblock::new_empty(2);
        sb.format_version = ENCRYPTED_FORMAT_VERSION; // pack(2, 0)
        assert_eq!(
            page::format_major(sb.format_version),
            2,
            "sanity: ENCRYPTED_FORMAT_VERSION must have MAJOR=2"
        );

        // Write the superblock as page 0 at plaintext stride (the format-
        // version gate fires before stride-dependent reads; page 0 is always
        // at offset 0).
        let bytes = sb.serialize();
        assert_eq!(bytes.len(), PAGE_SIZE);
        std::fs::write(&path, bytes).unwrap();

        // Open without a key: simulates an encryption-unaware binary.
        // Must be rejected with UnsupportedFormatVersion.
        let err = crate::Chisel::open(&path, crate::Options::default())
            .err()
            .expect("MAJOR=2 file opened without a key must fail");
        match err {
            ChiselError::UnsupportedFormatVersion { found, expected } => {
                // The exact stamped value is pack(2, 0) = 0x0002_0000 — the
                // single canonical encrypted format version. MAJOR byte is 2.
                assert_eq!(
                    found, ENCRYPTED_FORMAT_VERSION,
                    "error must report the exact MAJOR=2 MINOR=0 version found on disk"
                );
                assert_eq!(page::format_major(found), 2);
                assert_eq!(
                    expected,
                    FORMAT_VERSION, // pack(1, 1) — what a plaintext binary expects
                    "error must report FORMAT_VERSION as the expected version for a no-key open"
                );
            }
            other => panic!("expected UnsupportedFormatVersion, got {other:?}"),
        }
    }

    /// SUPERBLOCK-RECOVERY-2 (issue #106): the I29 write-gate must compare an
    /// encrypted file's MINOR against the ENCRYPTED minor series, not the
    /// plaintext one.
    ///
    /// The two series are independent counts — page.rs: "A new MAJOR series
    /// starts its MINOR count at 0, so encrypted DBs stamp (2, 0) — NOT
    /// (2, FORMAT_MINOR_VERSION)". The gate used to test against the plaintext
    /// `FORMAT_MINOR_VERSION`, currently 1, so a file stamped (2, 1) by a newer
    /// binary evaluated `1 > 1` as false: the gate did not fire, the file
    /// opened READ-WRITE, and this binary would have committed superblocks
    /// stamped at its own (2, 0) — silently dropping every field the newer
    /// minor added. That is the data loss the gate exists to prevent. It failed
    /// safe only by the accident of the encrypted minor being 0 today, which is
    /// why this test exercises minor 1 specifically: it is the very next bump,
    /// and the only value at which the old comparison was wrong.
    ///
    /// The superblock has to be BUILT with the bumped minor and then sealed —
    /// byte-patching `format_version` the way the plaintext sibling test does
    /// would not work here, because those bytes are part of `sb_identity_aad`.
    /// Patching them invalidates the sealed body, so `decrypt_body` fails with
    /// `CryptoError::Auth` and the open dies as `DecryptionFailed` long
    /// before reaching the gate.
    #[test]
    fn encrypted_file_with_newer_encrypted_minor_is_forced_read_only() {
        use crate::crypto::{Key, ENC_PAGE_SIZE};
        use zeroize::Zeroizing;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("enc_minor.chsl");

        // Raw (not passphrase) key: `wrap_into` selects HKDF for raw keys, so
        // this avoids a 19 MiB Argon2id derivation in a debug-profile test.
        let key = Key::Raw(Zeroizing::new(vec![0x5au8; 32]));

        // Create a real encrypted database, then borrow its cipher and crypto
        // header to seal a superblock this binary would otherwise never write.
        let (header, sealed_slots) = {
            let io = crate::page_io::PageIo::open(&path, false).unwrap();
            let cache = PageCache::new(
                io,
                1024 * PAGE_SIZE as u64,
                0,
                crate::DrainInsertion::LruTail,
                crate::SpillwayLocation::InMemory,
            );
            let tm = TransactionManager::create_new(cache, 2, Some(key.clone()), None).unwrap();
            let header = tm.crypto_header.expect("encrypted DB has a crypto header");
            let cipher = tm.cipher.as_ref().expect("encrypted DB has a cipher");

            let mut sb = Superblock::new_empty_encrypted(2, header);
            sb.format_version = page::pack_format_version(
                page::FORMAT_MAJOR_VERSION_ENCRYPTED,
                page::FORMAT_MINOR_VERSION_ENCRYPTED + 1,
            );
            // Seal one image per slot; each slot carries its own counter, and
            // the counter is inside the AAD, so they cannot share an image.
            // Counters mirror create_new's seeding (slot i gets N-1-i).
            let mut slots = Vec::new();
            for i in 0..2u64 {
                sb.txn_counter = 1 - i;
                slots.push(sb.serialize_encrypted(cipher));
            }
            (header, slots)
        }; // TransactionManager dropped here, releasing the flock.
        let _ = header;

        // Rewrite the file at the encrypted stride (8232), zero-padding each
        // 8192-byte image out to a full unit exactly as commit.rs does.
        let mut bytes = vec![0u8; 2 * ENC_PAGE_SIZE];
        for (i, image) in sealed_slots.iter().enumerate() {
            bytes[i * ENC_PAGE_SIZE..i * ENC_PAGE_SIZE + PAGE_SIZE].copy_from_slice(image);
        }
        std::fs::write(&path, &bytes).unwrap();

        // The file must still OPEN — a newer minor within a known MAJOR is
        // readable, which is the whole premise of the gate (reject writes,
        // allow reads).
        let mut db =
            crate::Chisel::open(&path, crate::Options::default().encryption_key(key.clone()))
                .expect("a newer-minor encrypted file must still open for reading");

        // ...but every mutation must be refused.
        assert!(
            matches!(db.begin(), Err(ChiselError::ReadOnlyMode)),
            "encrypted minor {} exceeds this binary's encrypted minor {}; \
             the write-gate must force read-only",
            page::FORMAT_MINOR_VERSION_ENCRYPTED + 1,
            page::FORMAT_MINOR_VERSION_ENCRYPTED,
        );
    }

    /// The complement of the test above: a file at THIS binary's encrypted
    /// minor must NOT be forced read-only. Without this, a gate that simply
    /// fired on every encrypted database would pass the test above while
    /// making encryption unusable.
    #[test]
    fn encrypted_file_at_current_encrypted_minor_is_writable() {
        use crate::crypto::Key;
        use zeroize::Zeroizing;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("enc_minor_ok.chsl");
        let key = Key::Raw(Zeroizing::new(vec![0x5au8; 32]));

        {
            let mut db =
                crate::Chisel::open(&path, crate::Options::default().encryption_key(key.clone()))
                    .expect("create encrypted DB");
            db.begin()
                .expect("a current-minor encrypted DB must be writable");
            db.commit().expect("commit");
        }

        let mut db = crate::Chisel::open(&path, crate::Options::default().encryption_key(key))
            .expect("reopen encrypted DB");
        db.begin()
            .expect("reopened current-minor encrypted DB must still be writable");
    }
}
