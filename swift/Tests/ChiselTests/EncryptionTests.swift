// EncryptionTests.swift — the encrypted-open and key-management surface,
// which had no Swift coverage at all: addKey / rotateKey / removeKey were
// reachable only through Rust tests, and no Swift test ever set
// Options.encryptionKey.
//
// File-backed throughout, deliberately: credentials round-trip through the
// on-disk superblock's wrapped-key slots, and an in-memory database has no
// analogue to reopen with a different key.
import XCTest
import Foundation
@testable import Chisel

final class EncryptionTests: XCTestCase {
    private func tempDir() throws -> URL {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("chisel-swift-enc-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        addTeardownBlock { try? FileManager.default.removeItem(at: dir) }
        return dir
    }

    private func opened(at path: URL, key: ChiselKey) throws -> ChiselDatabase {
        var opts = Options()
        opts.encryptionKey = key
        return try ChiselDatabase(path: path, options: opts)
    }

    func testEncryptedRoundTripAndWrongKeyRejected() throws {
        let path = try tempDir().appendingPathComponent("enc.db")
        let good = ChiselKey.passphrase(phrase: "correct horse battery staple")

        let db = try opened(at: path, key: good)
        try db.transaction { txn in
            let h = try txn.allocate(Data("classified".utf8))
            try txn.setRootName("root", h)
        }
        try db.close()

        let reopened = try opened(at: path, key: good)
        let root = try XCTUnwrap(try reopened.getRootName("root"))
        XCTAssertEqual(try reopened.read(root), Data("classified".utf8))
        try reopened.close()

        XCTAssertThrowsError(
            try opened(at: path, key: .passphrase(phrase: "wrong"))
        ) { error in
            guard let e = error as? ChiselError else { return XCTFail("expected ChiselError") }
            // A wrong credential is operational: supply the right key and retry.
            XCTAssertEqual(e.category, .operational)
        }
    }

    func testAddRotateAndRemoveKey() throws {
        let path = try tempDir().appendingPathComponent("keys.db")
        let first = ChiselKey.passphrase(phrase: "first credential")
        let second = ChiselKey.passphrase(phrase: "second credential")
        let third = ChiselKey.passphrase(phrase: "third credential")

        let db = try opened(at: path, key: first)
        try db.transaction { txn in
            let h = try txn.allocate(Data("payload".utf8))
            try txn.setRootName("root", h)
        }
        try db.addKey(existing: first, new: second)
        try db.close()

        // Both credentials now open it.
        let viaSecond = try opened(at: path, key: second)
        XCTAssertNotNil(try viaSecond.getRootName("root"))
        // Rotate the second into a third, then retire the first.
        try viaSecond.rotateKey(old: second, new: third)
        try viaSecond.removeKey(first)
        try viaSecond.close()

        let viaThird = try opened(at: path, key: third)
        XCTAssertNotNil(try viaThird.getRootName("root"))
        try viaThird.close()

        // The retired credentials no longer open it.
        XCTAssertThrowsError(try opened(at: path, key: first))
        XCTAssertThrowsError(try opened(at: path, key: second))
    }

    // Removing the only remaining credential would make the database
    // permanently unopenable, so the engine refuses.
    func testRemovingTheLastKeyIsRefused() throws {
        let path = try tempDir().appendingPathComponent("last.db")
        let only = ChiselKey.passphrase(phrase: "the only one")
        let db = try opened(at: path, key: only)
        XCTAssertThrowsError(try db.removeKey(only)) { error in
            guard let e = error as? ChiselError else { return XCTFail("expected ChiselError") }
            XCTAssertEqual(e.category, .operational)
        }
        try db.close()
    }

    // A raw 32-byte key takes the HKDF path rather than Argon2; both must work.
    func testRawKeyRoundTrip() throws {
        let path = try tempDir().appendingPathComponent("raw.db")
        let raw = ChiselKey.raw(bytes: Data(repeating: 0xAB, count: 32))
        let db = try opened(at: path, key: raw)
        try db.transaction { txn in
            let h = try txn.allocate(Data("bytes".utf8))
            try txn.setRootName("root", h)
        }
        try db.close()

        let again = try opened(at: path, key: .raw(bytes: Data(repeating: 0xAB, count: 32)))
        XCTAssertNotNil(try again.getRootName("root"))
        try again.close()
    }
}
