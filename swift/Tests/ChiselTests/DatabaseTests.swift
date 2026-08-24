import XCTest
import Foundation
@testable import Chisel

final class DatabaseTests: XCTestCase {
    func testTransactionCommits() throws {
        let db = try ChiselDatabase(path: nil)
        let h = try db.transaction { txn in
            try txn.allocate(Data("hi".utf8))
        }
        let v = try db.transaction { txn in try txn.read(h) }
        XCTAssertEqual(v, Data("hi".utf8))
    }

    func testTransactionRollsBackOnThrow() throws {
        struct Boom: Error {}
        let db = try ChiselDatabase(path: nil)
        var leaked: Handle?
        XCTAssertThrowsError(try db.transaction { txn -> Void in
            leaked = try txn.allocate(Data("x".utf8))
            throw Boom()
        })
        // The allocation was rolled back.
        try db.transaction { txn in
            XCTAssertThrowsError(try txn.read(leaked!))
        }
    }

    // stats/counters/defrag must be reachable on the in-transaction façade
    // (Python parity — these exist on Python's Transaction object too).
    func testTransactionStatsReachable() throws {
        let db = try ChiselDatabase(path: nil)
        try db.transaction { txn in
            let s = try txn.stats()
            XCTAssertEqual(s.handleCount, 0)
            _ = try txn.counters()
            _ = try txn.defrag()
        }
    }

    // `URL.path` on a non-file URL silently drops the scheme and host, so
    // `https://example.com/prod.db` created and flocked `/prod.db` on the
    // local filesystem, and `URL(string: "data.db")!` resolved against the
    // process cwd — which on iOS is not a writable container location. Both
    // succeeded silently instead of failing at the boundary.
    func testNonFileURLIsRejected() {
        XCTAssertThrowsError(
            try ChiselDatabase(path: URL(string: "https://example.com/prod.db")!)
        )
        XCTAssertThrowsError(
            try ChiselDatabase(path: URL(string: "data.db")!)
        )
    }

    func testFileURLOpensCommitsAndReopens() throws {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("chisel-swift-file-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let path = dir.appendingPathComponent("db.chisel")

        let db = try ChiselDatabase(path: path)
        try db.transaction { txn in
            let h = try txn.allocate(Data("durable".utf8))
            try txn.setRootName("root", h)
        }
        try db.close()

        let reopened = try ChiselDatabase(path: path)
        let root = try XCTUnwrap(try reopened.getRootName("root"))
        XCTAssertEqual(try reopened.read(root), Data("durable".utf8))
        try reopened.close()
    }
}
