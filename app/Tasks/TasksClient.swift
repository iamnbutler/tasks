import Foundation

/// A non-2xx response. The server always ships `{"error": "..."}` JSON;
/// `message` carries it so UI surfaces the real reason, not just a code.
struct APIError: LocalizedError {
    let status: Int
    let message: String

    var errorDescription: String? { message }
}

/// The three verdicts a reviewer may deliver (clients.md); `pending_review`
/// and `blocked` are server-assigned.
enum ReviewVerdict: String, Sendable {
    case approved
    case needsRevision = "needs_revision"
    case rejected
}

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

    func setMode(_ mode: Mode) async throws -> Mode {
        let response: ModeResponse = try await post("mode", body: ["mode": mode.wire])
        return response.mode
    }

    /// Recent events, newest last. `since` is inclusive.
    func events(since: Int64? = nil, limit: Int = 200) async throws -> [ActivityEvent] {
        var query = [URLQueryItem(name: "limit", value: String(limit))]
        if let since {
            query.append(URLQueryItem(name: "since", value: String(since)))
        }
        return try await get("events", query: query)
    }

    /// backlog -> queued, appended at the end of the ranked order.
    func queueTask(_ taskId: String) async throws -> TaskItem {
        try await postEmpty("tasks/\(taskId)/queue")
    }

    /// queued -> backlog, rank cleared.
    func dequeueTask(_ taskId: String) async throws -> TaskItem {
        try await postEmpty("tasks/\(taskId)/dequeue")
    }

    /// Queue at the front; next dispatch tick picks it up (cap still applies).
    func scoutNow(_ taskId: String) async throws -> TaskItem {
        try await postEmpty("tasks/\(taskId)/scout")
    }

    /// Sets manual_rank 1..n in the given order; unlisted tasks go unranked.
    /// Returns the full re-sorted queue.
    func reorderQueue(_ taskIds: [String]) async throws -> [TaskItem] {
        try await post("queue/reorder", body: ["task_ids": taskIds])
    }

    /// Deliver a review verdict. Returns the updated queue entry.
    func reviewSpec(_ specId: String, verdict: ReviewVerdict, feedback: String?)
        async throws -> SpecQueueItem
    {
        var body = ["status": verdict.rawValue]
        if let feedback {
            body["feedback"] = feedback
        }
        return try await post("spec-queue/\(specId)/review", body: body)
    }

    /// Catch-up read of a session transcript. `since` is inclusive; a tailing
    /// client passes `last_seq + 1`. Server default limit 500, cap 2000.
    func transcript(sessionId: String, since: Int64 = 0, limit: Int = 500)
        async throws -> [TranscriptLine]
    {
        try await get(
            "sessions/\(sessionId)/transcript",
            query: [
                URLQueryItem(name: "since", value: "\(since)"),
                URLQueryItem(name: "limit", value: "\(limit)"),
            ])
    }

    /// SSE tail of a session transcript: the server replays everything from
    /// `since` (paging internally, no holes), then streams live lines.
    /// Subscribe only while a session-detail view is open.
    func transcriptStream(sessionId: String, since: Int64 = 0)
        -> AsyncThrowingStream<TranscriptLine, Error>
    {
        let url = baseURL
            .appending(path: "sessions/\(sessionId)/transcript/stream")
            .appending(queryItems: [URLQueryItem(name: "since", value: "\(since)")])
        return AsyncThrowingStream { continuation in
            let reader = Task {
                do {
                    let (bytes, response) = try await URLSession.shared.bytes(from: url)
                    try Self.checkOK(response)
                    let decoder = Self.makeDecoder()
                    for try await raw in bytes.lines where raw.hasPrefix("data: ") {
                        let payload = Data(raw.dropFirst("data: ".count).utf8)
                        continuation.yield(try decoder.decode(TranscriptLine.self, from: payload))
                    }
                    continuation.finish()
                } catch {
                    continuation.finish(throwing: error)
                }
            }
            continuation.onTermination = { _ in reader.cancel() }
        }
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

    func builds() async throws -> [BuildItem] {
        try await get("builds")
    }

    /// Queue a Builder run over approved specs. 202: the response is the
    /// queued build, not a finished one — the serial loop picks it up.
    func requestBuild(specIds: [String]) async throws -> BuildItem {
        struct Body: Encodable {
            let specIds: [String]
            enum CodingKeys: String, CodingKey { case specIds = "spec_ids" }
        }
        return try await post("builds", body: Body(specIds: specIds))
    }

    private func postEmpty<T: Decodable>(_ path: String) async throws -> T {
        var request = URLRequest(url: baseURL.appending(path: path))
        request.httpMethod = "POST"
        let (data, response) = try await URLSession.shared.data(for: request)
        try Self.checkOK(response, body: data)
        return try Self.makeDecoder().decode(T.self, from: data)
    }

    private func post<T: Decodable>(_ path: String, body: some Encodable & Sendable)
        async throws -> T
    {
        var request = URLRequest(url: baseURL.appending(path: path))
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.httpBody = try JSONEncoder().encode(body)
        let (data, response) = try await URLSession.shared.data(for: request)
        try Self.checkOK(response, body: data)
        return try Self.makeDecoder().decode(T.self, from: data)
    }

    private func get<T: Decodable>(_ path: String, query: [URLQueryItem] = []) async throws -> T {
        var url = baseURL.appending(path: path)
        if !query.isEmpty {
            url = url.appending(queryItems: query)
        }
        let (data, response) = try await URLSession.shared.data(from: url)
        try Self.checkOK(response, body: data)
        return try Self.makeDecoder().decode(T.self, from: data)
    }

    private struct ServerError: Decodable {
        let error: String
    }

    private static func checkOK(_ response: URLResponse, body: Data? = nil) throws {
        guard let http = response as? HTTPURLResponse,
            !(200..<300).contains(http.statusCode)
        else { return }
        let message = body.flatMap { try? JSONDecoder().decode(ServerError.self, from: $0).error }
        throw APIError(
            status: http.statusCode,
            message: message ?? "HTTP \(http.statusCode)")
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
