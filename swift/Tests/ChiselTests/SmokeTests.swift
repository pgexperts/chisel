import XCTest
@testable import Chisel

final class SmokeTests: XCTestCase {
    func testLibraryVersion() {
        XCTAssertFalse(libraryVersion().isEmpty)
    }
}
