import XCTest
import Foundation
@testable import Chisel

final class StoreTests: XCTestCase {
    func testAsyncRoundTrip() async throws {
        let store = try await ChiselStore(path: nil)
        let h = try await store.transaction { txn in
            try txn.allocate(Data("async".utf8))
        }
        let v = try await store.read(h)
        XCTAssertEqual(v, Data("async".utf8))
    }

    func testStoreCloseThenUseThrows() async throws {
        let store = try await ChiselStore(path: nil)
        try await store.close()
        do {
            _ = try await store.handles()
            XCTFail("expected a throw after close")
        } catch let e as ChiselError {
            XCTAssertEqual(e.category, .operational)
        }
    }

    func testStoreIsPoisonedAfterClose() async throws {
        let store = try await ChiselStore(path: nil)
        let before = await store.isPoisoned()
        XCTAssertFalse(before)
        try await store.close()
        let after = await store.isPoisoned()
        XCTAssertTrue(after)
    }

    func testStoreRollsBackOnThrow() async throws {
        struct Boom: Error {}
        let store = try await ChiselStore(path: nil)
        do {
            _ = try await store.transaction { txn -> Handle in
                _ = try txn.allocate(Data("x".utf8))
                throw Boom()
            }
            XCTFail("expected Boom")
        } catch is Boom {
            // expected
        }
        let all = try await store.handles()
        XCTAssertTrue(all.isEmpty, "the throwing transaction must have rolled back")
    }
}
