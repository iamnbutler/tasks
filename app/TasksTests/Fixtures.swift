import Foundation

@testable import Tasks

/// Loads the golden wire fixtures the Rust API generates.
///
/// The files live at `<repo>/fixtures`, outside `app/`, and are resolved from
/// `#filePath` rather than copied into the test bundle as a resource: a copy
/// would be a second thing to keep in sync, which is exactly the failure mode
/// these fixtures exist to catch.
///
/// If the app ever gains an App Sandbox entitlement, the test bundle inherits
/// it and these reads stop working — at which point the fixtures do have to
/// become a bundled folder reference, and the copy has to happen at build time
/// so it can't go stale.
enum Fixtures {
    /// `<repo>/fixtures` — this file is at `<repo>/app/TasksTests/Fixtures.swift`.
    static let directory: URL = URL(fileURLWithPath: #filePath)
        .deletingLastPathComponent()  // TasksTests
        .deletingLastPathComponent()  // app
        .deletingLastPathComponent()  // <repo>
        .appendingPathComponent("fixtures")

    /// Every fixture's base name (no `.json`), sorted.
    static func names() throws -> [String] {
        let entries = try FileManager.default.contentsOfDirectory(
            at: directory, includingPropertiesForKeys: nil)
        let names =
            entries
            .filter { $0.pathExtension == "json" }
            .map { $0.deletingPathExtension().lastPathComponent }
            .sorted()
        guard !names.isEmpty else {
            // A silently empty enumeration would let the coverage check pass
            // while proving nothing — this has actually happened, via a symlink
            // corelibs Foundation declined to walk.
            throw FixtureError.noneFound(directory.path)
        }
        return names
    }

    static func data(_ name: String) throws -> Data {
        let url = directory.appendingPathComponent("\(name).json")
        guard let data = FileManager.default.contents(atPath: url.path) else {
            throw FixtureError.missing(url.path)
        }
        return data
    }

    /// Decode a fixture with the *production* decoder — the whole point is to
    /// exercise what the app actually ships, key strategy and date parsing
    /// included.
    static func decode<T: Decodable>(_ type: T.Type, _ name: String) throws -> T {
        try TasksClient.makeDecoder().decode(type, from: try data(name))
    }

    /// The raw JSON, for assertions about the wire that don't survive decoding
    /// (a field's presence, an enum's exact spelling).
    static func json(_ name: String) throws -> [String: Any] {
        guard
            let object = try JSONSerialization.jsonObject(with: try data(name))
                as? [String: Any]
        else {
            throw FixtureError.notAnObject(name)
        }
        return object
    }

    enum FixtureError: Error, CustomStringConvertible {
        case missing(String)
        case noneFound(String)
        case notAnObject(String)

        var description: String {
            switch self {
            case .missing(let path):
                "no fixture at \(path) — regenerate with "
                    + "`UPDATE_FIXTURES=1 cargo test -p tasks --test wire_fixtures`"
            case .noneFound(let path):
                "no .json fixtures under \(path) — the directory is missing, empty, "
                    + "or not walkable from this test bundle"
            case .notAnObject(let name):
                "fixture \(name).json is not a JSON object"
            }
        }
    }
}
