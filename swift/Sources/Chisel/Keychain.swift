// Keychain.swift — hand-written ergonomics layer (see Extensions.swift
// header) adding a Keychain-backed source for `ChiselKey`. Not part of the
// UniFFI-generated surface: this is pure Swift/Security-framework glue, so
// key bytes never need to be marshalled through Rust to get discovered.
//
// Bytes returned by SecItemCopyMatching cross the FFI boundary on the very
// next call site (wherever the resulting `.raw(bytes:)` is handed to
// ChiselDatabase.open), where Rust takes ownership and zeroizes them. This
// file does not zero the intermediate `Data` itself — Swift `Data` has no
// portable secure-wipe primitive, and the buffer is Keychain-service-owned
// autoreleased memory outside our control anyway.
import Foundation
import Security

public enum KeychainError: Error, Sendable {
    case itemNotFound
    case unexpectedData
    case unhandled(OSStatus)
}

extension ChiselKey {
    /// Load raw key bytes from the Keychain (a generic-password item) and
    /// wrap them as `.raw`. `service`/`account` are the standard Keychain
    /// generic-password lookup pair — same two fields `SecItemAdd` was
    /// called with when the key was provisioned.
    public static func keychain(service: String, account: String) throws -> ChiselKey {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
        ]
        var item: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &item)
        switch status {
        case errSecSuccess:
            guard let data = item as? Data else { throw KeychainError.unexpectedData }
            return .raw(bytes: data)
        case errSecItemNotFound:
            throw KeychainError.itemNotFound
        default:
            throw KeychainError.unhandled(status)
        }
    }
}
