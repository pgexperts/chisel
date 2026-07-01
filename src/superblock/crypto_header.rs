// superblock/crypto_header.rs — the plaintext crypto-header that lives in the
// superblock's reserved region for encrypted databases. Holds the algorithm id,
// the on-disk page stride, and the 8-slot key-slot table (each slot wraps the
// per-DB DEK under a KEK derived from one client key). For PLAINTEXT databases
// the reserved region stays zeroed and `deserialize` returns None (algorithm 0).
//
// Consumed by superblock/mod.rs (serialize_encrypted, deserialize, decrypt_body).
//
// On-disk layout (all inside the superblock's reserved region, after freemap_depth):
//   324..325   algorithm (u8; 1 = XChaCha20-Poly1305, 0 = none/plaintext)
//   325..329   stride (u32 LE; 8232 for encrypted, validated by the engine)
//   329..332   reserved (zero)
//   332..332+8*128  the 8 key-slot records, 128 bytes each
// Total = 8 + 8*128 = 1032 bytes, ending at 1356 — well inside CHECKSUM_OFFSET (8184).
//
// Key-slot record (128 bytes; trailing bytes reserved/zero):
//   0      state (u8; 1 = active, 0 = empty)
//   1      kdf_id (u8; 1 = HKDF, 2 = Argon2id)
//   2..14  argon2 params: m_cost(u32) | t_cost(u32) | p_cost(u32)  (zero for HKDF)
//   14..30 salt (16)
//   30..54 wrap_nonce (24)
//   54..86 wrapped_dek (32)
//   86..102 wrap_tag (16)
//   102..128 reserved

use crate::crypto::{Argon2Params, DEK_LEN, NONCE_LEN, SALT_LEN, TAG_LEN};
use crate::error::ChiselError;
use crate::page::{self, PAGE_SIZE};

pub const KEY_SLOT_COUNT: usize = 8;
pub const KEY_SLOT_SIZE: usize = 128;
/// State byte value for an occupied, usable slot.
const KEY_SLOT_ACTIVE: u8 = 1;
// Immediately after freemap_depth (bytes 320..324). Keep in lockstep with
// superblock/mod.rs's FREEMAP_DEPTH_OFFSET (320) + 4.
pub const CRYPTO_HEADER_OFFSET: usize = 324;
pub const CRYPTO_HEADER_SIZE: usize = 8 + KEY_SLOT_COUNT * KEY_SLOT_SIZE;

const SLOT_TABLE_OFFSET: usize = CRYPTO_HEADER_OFFSET + 8;
// Compile-time proof that the crypto header fits inside the reserved region.
const _: () = assert!(CRYPTO_HEADER_OFFSET + CRYPTO_HEADER_SIZE <= page::CHECKSUM_OFFSET);

/// Algorithm id stored in the header. 0 means "no encryption" (plaintext DB);
/// the only supported nonzero value today is 1 = XChaCha20-Poly1305.
pub const ALGO_XCHACHA20POLY1305: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeySlot {
    pub state: u8,
    pub kdf_id: u8,
    pub argon2: Argon2Params,
    pub salt: [u8; SALT_LEN],
    pub wrap_nonce: [u8; NONCE_LEN],
    pub wrapped_dek: [u8; DEK_LEN],
    pub wrap_tag: [u8; TAG_LEN],
}

impl KeySlot {
    pub const EMPTY: KeySlot = KeySlot {
        state: 0,
        kdf_id: 0,
        argon2: Argon2Params {
            m_cost: 0,
            t_cost: 0,
            p_cost: 0,
        },
        salt: [0u8; SALT_LEN],
        wrap_nonce: [0u8; NONCE_LEN],
        wrapped_dek: [0u8; DEK_LEN],
        wrap_tag: [0u8; TAG_LEN],
    };

    /// True if this slot holds a usable wrapped DEK (state byte == `KEY_SLOT_ACTIVE`).
    pub fn is_active(&self) -> bool {
        self.state == KEY_SLOT_ACTIVE
    }

    /// The bytes an unwrap operation must authenticate as AAD: the slot's own
    /// metadata up to but excluding the wrapped_dek/tag. Binds the wrap to its
    /// salt/params/nonce so a slot can't be transplanted between DBs.
    pub fn aad(&self) -> [u8; 1 + 1 + 12 + SALT_LEN + NONCE_LEN] {
        let mut a = [0u8; 1 + 1 + 12 + SALT_LEN + NONCE_LEN];
        a[0] = self.state;
        a[1] = self.kdf_id;
        a[2..6].copy_from_slice(&self.argon2.m_cost.to_le_bytes());
        a[6..10].copy_from_slice(&self.argon2.t_cost.to_le_bytes());
        a[10..14].copy_from_slice(&self.argon2.p_cost.to_le_bytes());
        a[14..14 + SALT_LEN].copy_from_slice(&self.salt);
        a[14 + SALT_LEN..14 + SALT_LEN + NONCE_LEN].copy_from_slice(&self.wrap_nonce);
        a
    }

    fn write_into(&self, slot: &mut [u8]) {
        slot[0] = self.state;
        slot[1] = self.kdf_id;
        slot[2..6].copy_from_slice(&self.argon2.m_cost.to_le_bytes());
        slot[6..10].copy_from_slice(&self.argon2.t_cost.to_le_bytes());
        slot[10..14].copy_from_slice(&self.argon2.p_cost.to_le_bytes());
        slot[14..14 + SALT_LEN].copy_from_slice(&self.salt);
        slot[30..30 + NONCE_LEN].copy_from_slice(&self.wrap_nonce);
        slot[54..54 + DEK_LEN].copy_from_slice(&self.wrapped_dek);
        slot[86..86 + TAG_LEN].copy_from_slice(&self.wrap_tag);
    }

    fn read_from(slot: &[u8]) -> KeySlot {
        let mut k = KeySlot::EMPTY;
        k.state = slot[0];
        k.kdf_id = slot[1];
        k.argon2 = Argon2Params {
            m_cost: u32::from_le_bytes(slot[2..6].try_into().unwrap()),
            t_cost: u32::from_le_bytes(slot[6..10].try_into().unwrap()),
            p_cost: u32::from_le_bytes(slot[10..14].try_into().unwrap()),
        };
        k.salt.copy_from_slice(&slot[14..14 + SALT_LEN]);
        k.wrap_nonce.copy_from_slice(&slot[30..30 + NONCE_LEN]);
        k.wrapped_dek.copy_from_slice(&slot[54..54 + DEK_LEN]);
        k.wrap_tag.copy_from_slice(&slot[86..86 + TAG_LEN]);
        k
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CryptoHeader {
    pub algorithm: u8,
    pub stride: u32,
    pub slots: [KeySlot; KEY_SLOT_COUNT],
}

impl CryptoHeader {
    /// Write the crypto-header into the superblock's reserved region. Touches
    /// only [CRYPTO_HEADER_OFFSET, CRYPTO_HEADER_OFFSET+CRYPTO_HEADER_SIZE);
    /// the caller stamps the page checksum afterward.
    pub fn serialize_into(&self, buf: &mut [u8; PAGE_SIZE]) {
        buf[CRYPTO_HEADER_OFFSET] = self.algorithm;
        buf[CRYPTO_HEADER_OFFSET + 1..CRYPTO_HEADER_OFFSET + 5]
            .copy_from_slice(&self.stride.to_le_bytes());
        for (i, slot) in self.slots.iter().enumerate() {
            let base = SLOT_TABLE_OFFSET + i * KEY_SLOT_SIZE;
            slot.write_into(&mut buf[base..base + KEY_SLOT_SIZE]);
        }
    }

    /// Read the crypto-header. Returns None for a plaintext DB (algorithm byte
    /// 0), which is how callers distinguish "encrypted" from "plaintext".
    pub fn deserialize(buf: &[u8; PAGE_SIZE]) -> Option<CryptoHeader> {
        let algorithm = buf[CRYPTO_HEADER_OFFSET];
        if algorithm == 0 {
            return None;
        }
        let stride = u32::from_le_bytes(
            buf[CRYPTO_HEADER_OFFSET + 1..CRYPTO_HEADER_OFFSET + 5]
                .try_into()
                .unwrap(),
        );
        let mut slots = [KeySlot::EMPTY; KEY_SLOT_COUNT];
        for (i, slot) in slots.iter_mut().enumerate() {
            let base = SLOT_TABLE_OFFSET + i * KEY_SLOT_SIZE;
            *slot = KeySlot::read_from(&buf[base..base + KEY_SLOT_SIZE]);
        }
        Some(CryptoHeader {
            algorithm,
            stride,
            slots,
        })
    }

    /// Count how many slots currently hold a wrapped DEK (state == active).
    /// Used by `remove_key` (Task 5.4) to guard against removing the last
    /// credential and locking the caller out of their own database.
    pub fn active_count(&self) -> usize {
        self.slots.iter().filter(|s| s.is_active()).count()
    }

    /// Index of the first non-active slot, or `None` if all 8 are occupied.
    /// Maps to `ChiselError::NoFreeKeySlot` in the caller (Task 5.2).
    pub fn free_slot(&self) -> Option<usize> {
        self.slots.iter().position(|s| !s.is_active())
    }

    /// Try each active slot in turn: derive the KEK from `key` + the slot's
    /// KDF identity, then attempt to unwrap the DEK. Returns `(slot_index,
    /// dek)` for the first slot whose AEAD tag verifies. If no slot matches,
    /// returns `Err(InvalidEncryptionKey)`.
    ///
    /// This is byte-identical to the inline trial in `recovery.rs`
    /// (`unwrap_first_matching_slot`) — both call `slot.aad()` on the
    /// fully-populated slot before passing it to `unwrap_dek`.
    ///
    /// # Errors
    /// Returns `ChiselError::InvalidEncryptionKey` if no active slot's tag
    /// verifies under the supplied key.
    pub fn unlock(
        &self,
        key: &crate::crypto::Key,
    ) -> Result<(usize, crate::crypto::Dek), ChiselError> {
        use crate::crypto::{self, KdfId};
        for (i, slot) in self.slots.iter().enumerate() {
            if !slot.is_active() {
                continue;
            }
            // Map the on-disk kdf_id byte to the typed enum. An unrecognized
            // id means a slot from a newer format — skip it rather than
            // failing the whole open, so forward-compatible keys still work.
            let kdf = match slot.kdf_id {
                x if x == KdfId::Hkdf as u8 => KdfId::Hkdf,
                x if x == KdfId::Argon2id as u8 => KdfId::Argon2id,
                _ => continue,
            };
            let kek = match crypto::derive_kek(key, kdf, &slot.salt, &slot.argon2) {
                Ok(k) => k,
                Err(_) => continue,
            };
            if let Ok(dek) = crypto::unwrap_dek(
                &kek,
                &slot.wrapped_dek,
                &slot.wrap_tag,
                &slot.wrap_nonce,
                &slot.aad(),
            ) {
                return Ok((i, dek));
            }
        }
        Err(ChiselError::InvalidEncryptionKey)
    }

    /// Populate `slots[slot]` with a fresh random salt and nonce, wrapping
    /// `dek` under the KEK derived from `key`. The KDF follows the key
    /// variant: `Key::Raw` → HKDF (fast, key-material quality);
    /// `Key::Passphrase` → Argon2id (memory-hard). The caller is responsible
    /// for ensuring the slot index is free (use `free_slot()`) before calling.
    ///
    /// AAD is computed over the slot metadata BEFORE the wrapped bytes are
    /// written (the `aad()` method does not read `wrapped_dek`/`wrap_tag`),
    /// so the same `slot.aad()` call on the on-disk slot at open time
    /// produces the exact same bytes `unlock` needs.
    pub fn wrap_into(
        &mut self,
        slot: usize,
        key: &crate::crypto::Key,
        dek: &crate::crypto::Dek,
    ) -> Result<(), crate::crypto::CryptoError> {
        use crate::crypto::{self, KdfId};
        let (kdf_id, argon2) = match key {
            crate::crypto::Key::Raw(_) => (
                KdfId::Hkdf,
                Argon2Params {
                    m_cost: 0,
                    t_cost: 0,
                    p_cost: 0,
                },
            ),
            crate::crypto::Key::Passphrase(_) => (KdfId::Argon2id, Argon2Params::default()),
        };
        let salt: [u8; SALT_LEN] = crypto::random_array();
        let wrap_nonce: [u8; NONCE_LEN] = crypto::random_array();
        let mut s = KeySlot {
            state: KEY_SLOT_ACTIVE,
            kdf_id: kdf_id as u8,
            argon2,
            salt,
            wrap_nonce,
            wrapped_dek: [0u8; DEK_LEN],
            wrap_tag: [0u8; TAG_LEN],
        };
        // AAD is computed before the wrapped bytes are filled in: aad() reads
        // state/kdf_id/argon2/salt/wrap_nonce, none of which are
        // wrapped_dek/wrap_tag. This ordering matches unlock() and the
        // existing recovery.rs path — all three call slot.aad() on the
        // populated-but-pre-wrap slot.
        let kek = crypto::derive_kek(key, kdf_id, &s.salt, &s.argon2)?;
        let (wrapped, tag) = crypto::wrap_dek(&kek, dek, &s.wrap_nonce, &s.aad());
        s.wrapped_dek = wrapped;
        s.wrap_tag = tag;
        self.slots[slot] = s;
        Ok(())
    }
}

#[cfg(test)]
mod crypto_header_tests {
    use super::*;
    use crate::crypto::{self, Key};
    use zeroize::Zeroizing;

    fn raw(b: u8) -> Key {
        Key::Raw(Zeroizing::new(vec![b; 32]))
    }

    // A header with exactly one active slot holding `dek` under `key`.
    fn header_with_one(key: &Key, dek: &crypto::Dek) -> CryptoHeader {
        let mut h = CryptoHeader {
            algorithm: 1,
            stride: crypto::ENC_PAGE_SIZE as u32,
            slots: [KeySlot::EMPTY; KEY_SLOT_COUNT],
        };
        h.wrap_into(0, key, dek)
            .expect("wrap_into with valid key must succeed");
        h
    }

    #[test]
    fn unlock_finds_the_right_slot_and_recovers_dek() {
        let dek = crypto::random_dek();
        let k0 = raw(0xA1);
        let mut h = header_with_one(&k0, &dek);

        // Add a second credential into slot 3 wrapping the SAME dek.
        let k1 = raw(0xB2);
        h.wrap_into(3, &k1, &dek)
            .expect("wrap_into with valid key must succeed");

        let (idx0, d0) = h.unlock(&k0).expect("k0 must unlock");
        let (idx1, d1) = h.unlock(&k1).expect("k1 must unlock");
        assert_eq!(idx0, 0);
        assert_eq!(idx1, 3);
        // Both recover the identical DEK bytes.
        assert_eq!(d0.as_bytes(), dek.as_bytes());
        assert_eq!(d1.as_bytes(), dek.as_bytes());
    }

    #[test]
    fn unlock_wrong_key_returns_invalid_encryption_key() {
        let dek = crypto::random_dek();
        let h = header_with_one(&raw(0xAA), &dek);
        // Dek has no Debug, so we can't use expect_err(); use matches! instead.
        let result = h.unlock(&raw(0xBB));
        assert!(matches!(
            result,
            Err(crate::error::ChiselError::InvalidEncryptionKey)
        ));
    }

    #[test]
    fn unlock_empty_header_returns_error() {
        let h = CryptoHeader {
            algorithm: 1,
            stride: crypto::ENC_PAGE_SIZE as u32,
            slots: [KeySlot::EMPTY; KEY_SLOT_COUNT],
        };
        assert!(h.unlock(&raw(0x01)).is_err());
    }

    #[test]
    fn free_slot_and_active_count_track_occupancy() {
        let dek = crypto::random_dek();
        let mut h = header_with_one(&raw(0x01), &dek);
        assert_eq!(h.active_count(), 1);
        assert_eq!(h.free_slot(), Some(1));

        // Fill every remaining slot.
        for i in 1..KEY_SLOT_COUNT {
            h.wrap_into(i, &raw(i as u8 + 1), &dek)
                .expect("wrap_into with valid key must succeed");
        }
        assert_eq!(h.active_count(), KEY_SLOT_COUNT);
        assert_eq!(h.free_slot(), None);
    }

    #[test]
    fn wrap_into_then_unlock_round_trips_dek() {
        // Verify that a freshly wrapped slot's AAD bytes at wrap time match
        // those recomputed at unlock time (the crux of Task 5.1).
        let dek = crypto::random_dek();
        let key = raw(0x77);
        let mut h = CryptoHeader {
            algorithm: 1,
            stride: crypto::ENC_PAGE_SIZE as u32,
            slots: [KeySlot::EMPTY; KEY_SLOT_COUNT],
        };
        h.wrap_into(5, &key, &dek)
            .expect("wrap_into with valid key must succeed");
        let (idx, recovered) = h.unlock(&key).expect("wrap_into then unlock must succeed");
        assert_eq!(idx, 5);
        assert_eq!(recovered.as_bytes(), dek.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::Argon2Params;
    use crate::page::PAGE_SIZE;

    fn sample_slot(state: u8) -> KeySlot {
        KeySlot {
            state,
            kdf_id: 1,
            argon2: Argon2Params {
                m_cost: 19456,
                t_cost: 2,
                p_cost: 1,
            },
            salt: [7u8; 16],
            wrap_nonce: [9u8; 24],
            wrapped_dek: [3u8; 32],
            wrap_tag: [5u8; 16],
        }
    }

    #[test]
    fn crypto_header_round_trips_through_reserved_region() {
        let mut slots = [KeySlot::EMPTY; KEY_SLOT_COUNT];
        slots[0] = sample_slot(1); // active
        slots[3] = sample_slot(1); // active
        let header = CryptoHeader {
            algorithm: 1,
            stride: 8232,
            slots,
        };

        let mut buf = [0u8; PAGE_SIZE];
        header.serialize_into(&mut buf);

        // Bytes before the header (the existing fields + reserved gap up to 324)
        // are NOT touched by serialize_into.
        assert_eq!(buf[..CRYPTO_HEADER_OFFSET], [0u8; CRYPTO_HEADER_OFFSET][..]);

        let back = CryptoHeader::deserialize(&buf).expect("active header must deserialize");
        assert_eq!(back.algorithm, 1);
        assert_eq!(back.stride, 8232);
        assert!(back.slots[0].is_active());
        assert!(!back.slots[1].is_active());
        assert!(back.slots[3].is_active());
        assert_eq!(back.slots[0].salt, [7u8; 16]);
        assert_eq!(back.slots[0].wrap_nonce, [9u8; 24]);
        assert_eq!(back.slots[0].wrapped_dek, [3u8; 32]);
        assert_eq!(back.slots[0].wrap_tag, [5u8; 16]);
        assert_eq!(back.slots[0].argon2.m_cost, 19456);
    }

    #[test]
    fn deserialize_returns_none_for_plaintext_db() {
        // A zeroed reserved region (plaintext DB) has algorithm == 0 -> None.
        let buf = [0u8; PAGE_SIZE];
        assert!(CryptoHeader::deserialize(&buf).is_none());
    }
}
