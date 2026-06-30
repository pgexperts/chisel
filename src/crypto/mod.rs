// src/crypto/mod.rs — Crypto core (layer 1, no engine coupling).
//
// Standalone at-rest encryption primitives for Chisel: the XChaCha20-Poly1305
// PageCipher (whole-page + variable-length body seal/open), the envelope KDF
// (HKDF-SHA256 for raw keys, Argon2id for passphrases), DEK wrap/unwrap, and
// the zeroizing key types. Nothing here touches page_io, the cache, or the
// superblock — those layers consume this module in later phases. See
// docs/specs/2026-06-29-on-disk-encryption-design.md §3.
//
// All randomness is OS-sourced (getrandom). Rolling our own crypto is
// forbidden; only the vetted RustCrypto primitives are used.

// Smoke check that the chacha20poly1305 dep is linked and its key length is
// the 32 bytes the envelope assumes. Replaced by real tests in later tasks.
#[cfg(test)]
mod tests {
    #[test]
    fn deps_link() {
        use chacha20poly1305::KeySizeUser;
        use chacha20poly1305::XChaCha20Poly1305;
        assert_eq!(
            <XChaCha20Poly1305 as KeySizeUser>::key_size(),
            32,
            "XChaCha20-Poly1305 key must be 32 bytes"
        );
    }
}
