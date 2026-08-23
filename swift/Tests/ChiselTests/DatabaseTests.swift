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
}
