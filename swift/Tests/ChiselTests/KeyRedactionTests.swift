// KeyRedactionTests.swift — covers SWIFT-6's Swift-side half (issue #155).
//
// The Rust FFI layer redacts `ChiselKey`'s Debug output (chisel-ffi/src/types.rs),
// but that only protects `{:?}` on the Rust side of the boundary. Swift's
// generated `ChiselKey` enum has no CustomStringConvertible /
// CustomDebugStringConvertible of its own, so before Extensions.swift added
// one, default enum reflection printed the associated value verbatim through
// every ordinary rendering path a caller is likely to hit first: string
// interpolation, `print`, `os_log`, and XCTest failure messages.
import XCTest
@testable import Chisel

final class KeyRedactionTests: XCTestCase {
    // SWIFT-6's Swift half. Default enum reflection would render the payload,
    // so a passphrase reached os_log and crash reporters through ordinary
    // string interpolation. Asserts the secret is absent from every rendering
    // path a caller is likely to use.
    func testKeyMaterialNeverRendersInAnyDescription() {
        let secret = "correct horse battery staple"
        let key = ChiselKey.passphrase(phrase: secret)
        for rendered in ["\(key)", String(describing: key), String(reflecting: key), key.debugDescription] {
            XCTAssertFalse(rendered.contains(secret), "passphrase leaked: \(rendered)")
            XCTAssertTrue(rendered.contains("redacted"), "expected a redaction marker: \(rendered)")
        }

        let bytes = Data([0xDE, 0xAD, 0xBE, 0xEF])
        let raw = ChiselKey.raw(bytes: bytes)
        for rendered in ["\(raw)", String(describing: raw), String(reflecting: raw)] {
            XCTAssertFalse(rendered.contains("222"), "raw byte leaked: \(rendered)")
            XCTAssertFalse(rendered.contains("DEAD"), "raw byte leaked: \(rendered)")
            XCTAssertTrue(rendered.contains("redacted"))
        }
    }

    // `Options` has no `CustomStringConvertible` of its own, but it embeds
    // `ChiselKey` as `encryptionKey`. Swift's default mirror-based
    // description for a struct without its own conformance renders each
    // stored property via that property's own `String(describing:)`, which
    // dispatches to `ChiselKey`'s conformance above — so an `Options` value
    // should redact too, with no `Options`-specific code required. This is
    // the Swift-side counterpart to the Rust side's equivalent coverage
    // (chisel-ffi/src/types.rs).
    func testOptionsContainingKeyRedactsThroughDefaultReflection() {
        let secret = "correct horse battery staple"
        let options = Options(drainInsertion: .lruTail, encryptionKey: .passphrase(phrase: secret))
        for rendered in ["\(options)", String(describing: options), String(reflecting: options)] {
            XCTAssertFalse(rendered.contains(secret), "passphrase leaked via Options: \(rendered)")
            XCTAssertTrue(rendered.contains("redacted"), "expected a redaction marker: \(rendered)")
        }
    }
}
