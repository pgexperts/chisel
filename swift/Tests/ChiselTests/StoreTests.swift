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
}
