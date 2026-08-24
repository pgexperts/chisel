// Rekey.swift — the one operation that is not a method on an open handle.
//
// `rekey` rewrites the whole database and publishes it with an atomic rename,
// so it takes a path: a handle opened beforehand would afterwards refer to the
// original, now-unlinked inode. This mirrors the Python binding, where `rekey`
// is a module-level function rather than a `Chisel` method for the same reason.
import Foundation

/// Re-encrypt an entire database under a freshly generated data key.
///
/// Close any open handle to `path` first, call this, then reopen — the file is
/// replaced by rename, so an existing handle would keep pointing at the old
/// inode.
///
/// Use this only when the DATA key itself is believed compromised. For a leaked
/// credential, `ChiselDatabase.rotateKey(old:new:)` is the right tool: it is
/// O(1) and touches only the superblock.
///
/// Every credential other than `key` is revoked, because each key slot's
/// wrapping key derives from its own credential and only `key` was supplied.
/// Re-add the others with `addKey(existing:new:)` afterwards.
///
/// - Parameter argon2Params: `nil` inherits the cost parameters of the slot
///   `key` unlocks, so a hardened database is not silently downgraded.
/// - Throws: `.DatabaseFileNotFound` if `path` names nothing,
///   `.EncryptionNotSupported` if the database is plaintext,
///   `.InvalidEncryptionKey` if `key` unlocks no slot, `.InvalidArgon2Params`
///   if `argon2Params` carries unusable cost values.
public func rekey(at path: URL, key: ChiselKey, argon2Params: Argon2Params? = nil) throws {
    try Chisel.rekey(path: chiselFilePath(path), key: key, argon2Params: argon2Params)
}
