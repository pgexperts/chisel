// ConcurrencyTests.swift — proves ChiselStore's actor + serial-queue design
// actually serializes engine access under concurrent load. This is the test
// meant to be run with `--sanitize=thread`: TSan instruments the Swift side
// only (the linked Rust static lib is opaque to it), so a clean TSan run here
// is evidence the race being ruled out — concurrent Swift Tasks touching the
// engine without synchronization — cannot happen, not that the Rust engine
// itself is data-race-free (that's the engine's own test suite's job).
import XCTest
import Foundation
@testable import Chisel

final class ConcurrencyTests: XCTestCase {
    func testManyTasksSerialize() async throws {
        let store = try await ChiselStore(path: nil)
        // 100 concurrent allocations, each in its own transaction. If the
        // actor/queue ever let two of these run the engine concurrently, the
        // !Sync engine's internal state would corrupt and this would surface
        // as lost/duplicate handles, a crash, or (under TSan) a race report.
        try await withThrowingTaskGroup(of: Handle.self) { group in
            for i in 0..<100 {
                group.addTask {
                    try await store.transaction { txn in
                        try txn.allocate(Data("v\(i)".utf8))
                    }
                }
            }
            var seen = Set<Handle>()
            for try await h in group { seen.insert(h) }
            XCTAssertEqual(seen.count, 100)  // all distinct, none lost
        }
        let all = try await store.transaction { txn in try txn.handles() }
        XCTAssertEqual(all.count, 100)
    }
}
