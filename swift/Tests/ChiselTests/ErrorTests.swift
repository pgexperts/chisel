import XCTest
@testable import Chisel

final class ErrorTests: XCTestCase {
    func testCategory() {
        XCTAssertEqual(ChiselError.InvalidHandle(message: "x").category, .operational)
        XCTAssertEqual(ChiselError.CorruptSuperblock(message: "x").category, .fatal)
        XCTAssertTrue(ChiselError.CorruptSuperblock(message: "x").isPoisoning)
        XCTAssertFalse(ChiselError.CacheFull(message: "x").isPoisoning)
    }

    // `Poisoned` is `.fatal`, matching the caller-facing Python tiering
    // (python/src/errors.rs: PoisonedError : FatalError). It is a
    // drop-and-reopen condition for the caller, deliberately unlike the
    // engine's manager-internal `is_fatal()` which treats an already-poisoned
    // manager as a non-event. See Extensions.swift's header comment.
    func testPoisonedIsFatalPerPythonTiering() {
        XCTAssertEqual(ChiselError.Poisoned(message: "x").category, .fatal)
        XCTAssertTrue(ChiselError.Poisoned(message: "x").isPoisoning)
    }

    func testAssociatedValueCasesConstructAndClassify() {
        XCTAssertEqual(ChiselError.SavepointNotFound(message: "x").category, .operational)
        XCTAssertEqual(ChiselError.DuplicateSavepoint(message: "x").category, .operational)
        XCTAssertEqual(ChiselError.Io(kind: "Other", errno: nil, message: "x").category, .fatal)
    }

    func testFfiWrapperCasesAreOperational() {
        XCTAssertEqual(ChiselError.Closed(message: "x").category, .operational)
        XCTAssertEqual(ChiselError.AlreadyFinished(message: "x").category, .operational)
    }

    // Proves the `message` accessor actually surfaces the payload the
    // engine attached, for both the single-field shape and `.Io`'s
    // three-field shape.
    func testMessageAccessorReturnsPayload() {
        XCTAssertEqual(ChiselError.InvalidHandle(message: "bad handle").message, "bad handle")
        XCTAssertEqual(ChiselError.Io(kind: "NotFound", errno: nil, message: "file missing").message, "file missing")
    }
}
