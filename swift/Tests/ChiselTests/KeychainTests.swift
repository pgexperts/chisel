// KeychainTests.swift — covers the read-miss path only. We deliberately do
// NOT write a Keychain item in setup: writing a generic-password item in a
// CI/sandboxed macOS test runner routinely fails with errSecMissingEntitlement
// (no keychain-access-groups entitlement granted to `swift test`'s xctest
// process), which would make the test flaky by environment rather than by
// code. A lookup miss needs no write access and is deterministic everywhere
// SecItemCopyMatching itself works.
import XCTest
@testable import Chisel

final class KeychainTests: XCTestCase {
    func testMissingItemThrows() {
        XCTAssertThrowsError(
            try ChiselKey.keychain(
                service: "org.chisel.tests",
                account: "does-not-exist-\(UUID())"
            )
        )
    }
}
