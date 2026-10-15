// LifecycleTests.swift — close / double-close / post-close behaviour and the
// runtime config setters, none of which the suite exercised before SWIFT-7.
// The Rust side has equivalents (use_after_close_is_closed_error,
// double_close_is_closed_error); it is precisely the hand-written Swift
// wrapper that was untested.
import XCTest
import Foundation
@testable import Chisel

final class LifecycleTests: XCTestCase {
    func testCloseThenUseIsClosedError() throws {
        let db = try ChiselDatabase(path: nil)
        try db.close()
        XCTAssertThrowsError(try db.handles()) { error in
            guard let e = error as? ChiselError else { return XCTFail("expected ChiselError") }
            XCTAssertEqual(e.category, .operational)
        }
    }

    func testDoubleCloseIsClosedError() throws {
        let db = try ChiselDatabase(path: nil)
        try db.close()
        XCTAssertThrowsError(try db.close())
    }

    // SWIFT-8's substance as an executable assertion: a cleanly closed handle
    // reports isPoisoned == true, because the flag answers "can this handle
    // still do work", not "is the file corrupt". A caller reading it as the
    // latter after a clean shutdown concludes the database is damaged.
    func testIsPoisonedIsTrueAfterACleanClose() throws {
        let db = try ChiselDatabase(path: nil)
        XCTAssertFalse(db.isPoisoned)
        try db.close()
        XCTAssertTrue(db.isPoisoned, "a closed handle reports poisoned; see the doc comment")
    }

    func testRuntimeConfigSettersApplyOutsideATransaction() throws {
        let db = try ChiselDatabase(path: nil)
        try db.setCacheMaxBytes(4 * 1024 * 1024)
        try db.setSpillwayMaxBytes(0)
        try db.setDrainInsertion(.mru)
        try db.setDrainInsertion(.lruTail)
    }

    // Each setter refuses while a transaction is open — the engine's
    // TransactionInProgress, surfaced through the wrapper.
    func testRuntimeConfigSettersRefuseInsideATransaction() throws {
        let db = try ChiselDatabase(path: nil)
        try db.transaction { _ in
            XCTAssertThrowsError(try db.setCacheMaxBytes(1024 * 1024))
            XCTAssertThrowsError(try db.setSpillwayMaxBytes(0))
            XCTAssertThrowsError(try db.setDrainInsertion(.mru))
        }
    }

    func testStatsAndCountersAndFileSizeAreReachable() throws {
        let db = try ChiselDatabase(path: nil)
        XCTAssertEqual(try db.stats().handleCount, 0)
        _ = try db.counters()
        _ = try db.fileSizeBytes()
    }
}
