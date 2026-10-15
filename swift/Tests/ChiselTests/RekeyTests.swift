import XCTest
import Foundation
@testable import Chisel

final class RekeyTests: XCTestCase {
    func testRekeyPreservesContentUnderTheSameCredential() throws {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("chisel-swift-rekey-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let path = dir.appendingPathComponent("rk.db")
        let key = ChiselKey.passphrase(phrase: "correct horse")

        var opts = Options()
        opts.encryptionKey = key
        let db = try ChiselDatabase(path: path, options: opts)
        let h = try db.transaction { txn -> Handle in
            let h = try txn.allocate(Data("secret".utf8))
            try txn.setRootName("r", h)
            return h
        }
        _ = h
        try db.close()

        try rekey(at: path, key: key)

        var reopen = Options()
        reopen.encryptionKey = key
        let again = try ChiselDatabase(path: path, options: reopen)
        let root = try XCTUnwrap(try again.getRootName("r"))
        XCTAssertEqual(try again.read(root), Data("secret".utf8))
        try again.close()
    }

    // A non-file URL must be refused at the boundary rather than silently
    // rekeying the path component on the local filesystem.
    func testRekeyRejectsNonFileURL() {
        let url = URL(string: "https://example.com/prod.db")!
        XCTAssertThrowsError(try rekey(at: url, key: .passphrase(phrase: "x"))) { error in
            guard let e = error as? ChiselError else { return XCTFail("expected ChiselError") }
            XCTAssertEqual(e.category, .operational)
        }
    }
}
