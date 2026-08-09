import Foundation

/// Thin client for the tasks server's loopback HTTP API.
struct TasksClient: Sendable {
    // Honors the same TASKS_SERVER_PORT the server reads.
    var baseURL = URL(
        string: "http://127.0.0.1:\(ProcessInfo.processInfo.environment["TASKS_SERVER_PORT"] ?? "4800")"
    )!

    func projects() async throws -> [Project] { try await get("projects") }
    func tasks() async throws -> [TaskItem] { try await get("tasks") }
    func sessions() async throws -> [ScoutSession] { try await get("sessions") }
    func specs() async throws -> [Spec] { try await get("specs") }
    func specQueue() async throws -> [SpecQueueItem] { try await get("spec-queue") }
    func spec(_ id: String) async throws -> Spec { try await get("specs/\(id)") }

    func mode() async throws -> Mode {
        let response: ModeResponse = try await get("mode")
        return response.mode
    }

    /// Sets manual_rank 1..n in the given order; unlisted tasks go unranked.
    /// Returns the full re-sorted queue.
    func reorderQueue(_ taskIds: [String]) async throws -> [TaskItem] {
        try await post("queue/reorder", body: ["task_ids": taskIds])
    }

    enum SSESignal: Sendable {
        /// Response head received — the server-side subscription exists, so a
        /// snapshot taken now can't miss events (clients.md: stream first,
        /// then snapshot).
        case connected
        /// One `data:` payload.
        case event(String)
    }

    /// Signals from /events/stream. Finishes when the server closes the
    /// connection; the caller owns reconnect policy.
    func eventStream() -> AsyncThrowingStream<SSESignal, Error> {
        let url = baseURL.appending(path: "events/stream")
        return AsyncThrowingStream { continuation in
            let reader = Task {
                do {
                    let (bytes, response) = try await URLSession.shared.bytes(from: url)
                    try Self.checkOK(response)
                    continuation.yield(.connected)
                    for try await line in bytes.lines where line.hasPrefix("data: ") {
                        continuation.yield(.event(String(line.dropFirst("data: ".count))))
                    }
                    continuation.finish()
                } catch {
                    continuation.finish(throwing: error)
                }
            }
            continuation.onTermination = { _ in reader.cancel() }
        }
    }

    private func post<T: Decodable>(_ path: String, body: some Encodable & Sendable)
        async throws -> T
    {
        var request = URLRequest(url: baseURL.appending(path: path))
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.httpBody = try JSONEncoder().encode(body)
        let (data, response) = try await URLSession.shared.data(for: request)
        try Self.checkOK(response)
        return try Self.makeDecoder().decode(T.self, from: data)
    }

    private func get<T: Decodable>(_ path: String) async throws -> T {
        let (data, response) = try await URLSession.shared.data(from: baseURL.appending(path: path))
        try Self.checkOK(response)
        return try Self.makeDecoder().decode(T.self, from: data)
    }

    private static func checkOK(_ response: URLResponse) throws {
        guard let http = response as? HTTPURLResponse,
            !(200..<300).contains(http.statusCode)
        else { return }
        throw URLError(
            .badServerResponse,
            userInfo: [NSLocalizedDescriptionKey: "HTTP \(http.statusCode)"])
    }

    private static func makeDecoder() -> JSONDecoder {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        decoder.dateDecodingStrategy = .custom { decoder in
            let container = try decoder.singleValueContainer()
            let raw = try container.decode(String.self)
            guard let date = parseRFC3339(raw) else {
                throw DecodingError.dataCorruptedError(
                    in: container, debugDescription: "unparseable date: \(raw)")
            }
            return date
        }
        return decoder
    }

    /// chrono emits RFC3339 with 0–9 fractional digits depending on the value;
    /// normalize to exactly 3 so a single format style parses everything.
    private static func parseRFC3339(_ raw: String) -> Date? {
        var s = raw
        guard let dot = s.firstIndex(of: ".") else {
            return try? Date(s, strategy: .iso8601)
        }
        let start = s.index(after: dot)
        var end = start
        while end < s.endIndex, s[end].isNumber { end = s.index(after: end) }
        let frac = String(s[start..<end].prefix(3))
            .padding(toLength: 3, withPad: "0", startingAt: 0)
        s.replaceSubrange(dot..<end, with: ".\(frac)")
        return try? Date(s, strategy: Date.ISO8601FormatStyle(includingFractionalSeconds: true))
    }
}
