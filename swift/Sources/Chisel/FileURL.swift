// FileURL.swift — the one place a caller-supplied URL becomes a filesystem
// path. Every entry point that accepts a URL routes through it.
//
// `URL.path` is not a safe conversion: on a non-file URL it silently returns
// the path component with the scheme and host discarded, so a URL that names a
// remote resource turns into a local path that the engine will happily create
// and flock.
import Foundation

/// Convert a caller-supplied file URL to the path the engine expects.
///
/// - Throws: `.DatabaseFileNotFound` if `url` is not a file URL. Reusing that
///   case rather than adding one keeps the FFI error surface aligned with the
///   engine's; the message names the actual problem.
func chiselFilePath(_ url: URL) throws -> String {
    guard url.isFileURL else {
        throw ChiselError.DatabaseFileNotFound(
            message: """
                database path must be a file URL, got scheme \
                \(url.scheme ?? "none") — build it with URL(fileURLWithPath:)
                """
        )
    }
    return url.path
}
