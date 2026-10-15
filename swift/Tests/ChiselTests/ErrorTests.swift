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

    // The five variants the engine grew after this binding was written. Before
    // the SWIFT-13 fix every one of them arrived as `.Poisoned`, so
    // InvalidArgon2Params — a rejected cost parameter, with the database
    // untouched — was reported to callers as a fatal poisoning.
    func testVariantsAddedAfterTheBindingClassifyCorrectly() {
        XCTAssertEqual(ChiselError.InvalidArgon2Params(message: "x").category, .operational)
        XCTAssertEqual(ChiselError.InvalidFreemapDepth(message: "x").category, .fatal)
        XCTAssertEqual(ChiselError.InvalidNextHandle(message: "x").category, .fatal)
        XCTAssertEqual(ChiselError.TxnCounterExhausted(message: "x").category, .fatal)
        XCTAssertEqual(ChiselError.HandleSpaceExhausted(message: "x").category, .fatal)
    }

    // `.Other` carries the engine's is_fatal() verdict rather than a constant,
    // so a future unmapped variant still reaches the caller at the right tier.
    func testOtherCarriesItsTier() {
        XCTAssertEqual(ChiselError.Other(message: "x", fatal: false).category, .operational)
        XCTAssertEqual(ChiselError.Other(message: "x", fatal: true).category, .fatal)
        XCTAssertTrue(ChiselError.Other(message: "x", fatal: true).isPoisoning)
        XCTAssertEqual(ChiselError.Other(message: "boom", fatal: true).message, "boom")
    }

    // A zero tag is a caller mistake with the database untouched — the same
    // shape as every other operational case, and catchable as ChiselError,
    // which is what swift/README.md promises for every throwing call.
    func testZeroTagIsOperationalAndCatchable() throws {
        let db = try ChiselDatabase(path: nil)
        try db.transaction { txn in
            XCTAssertThrowsError(try txn.allocateTagged(Data("x".utf8), tag: 0)) { error in
                guard let e = error as? ChiselError else { return XCTFail("expected ChiselError") }
                XCTAssertEqual(e.category, .operational)
            }
            XCTAssertThrowsError(try txn.handlesWithTag(0))
            // A nonzero tag is unaffected.
            _ = try txn.allocateTagged(Data("x".utf8), tag: 7)
        }
    }
}
