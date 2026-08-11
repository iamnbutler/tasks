import SwiftUI

/// Live transcript pane for one scout session. Subscribes to the per-session
/// SSE stream only while visible (`.task(id:)` tears it down on close or
/// session change), which is the contract clients.md asks for — transcript
/// lines never ride the main event stream.
struct TranscriptView: View {
    @Environment(AppModel.self) private var model
    let session: ScoutSession

    @State private var lines: [TranscriptLine] = []
    @State private var streamError: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            if lines.isEmpty {
                // Empty means nothing was recorded (pre-transcript sessions),
                // never a failure.
                ContentUnavailableView {
                    Label("No transcript", systemImage: "text.bubble")
                } description: {
                    Text("No output was recorded for this session.")
                }
            } else {
                LazyVStack(alignment: .leading, spacing: 6) {
                    ForEach(lines) { line in
                        TranscriptLineView(line: line)
                    }
                }
            }
            if let streamError {
                Label(streamError, systemImage: "bolt.horizontal.circle")
                    .font(.caption)
                    .foregroundStyle(.orange)
            }
        }
        .task(id: session.id) { await follow() }
    }

    private func follow() async {
        lines = []
        streamError = nil
        var since: Int64 = 0
        while !Task.isCancelled {
            do {
                for try await line in model.client.transcriptStream(
                    sessionId: session.id, since: since)
                {
                    lines.append(line)
                    since = line.seq + 1
                    streamError = nil
                }
            } catch is CancellationError {
                return
            } catch let error as APIError where error.status == 404 {
                // Pre-transcript server or vanished session — that's the
                // empty state, not a failure.
                return
            } catch {
                streamError = error.localizedDescription
            }
            // Stream closed. Keep tailing only if the session may still emit.
            let current = model.session(session.id) ?? session
            if current.completedAt != nil {
                return
            }
            try? await Task.sleep(for: .seconds(3))
        }
    }
}

/// One transcript line. stdout lines are Claude Code stream-json records —
/// parsed leniently into readable blocks; anything unparseable (and all
/// stderr) renders as raw monospaced text so nothing is ever hidden.
struct TranscriptLineView: View {
    let line: TranscriptLine

    var body: some View {
        switch TranscriptRecord(line) {
        case .assistantBlocks(let blocks):
            ForEach(Array(blocks.enumerated()), id: \.offset) { _, block in
                AssistantBlockView(block: block)
            }
        case .toolResult(let summary, let isError):
            DisclosureGroup {
                Text(summary)
                    .font(.caption.monospaced())
                    .textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
            } label: {
                Label(
                    isError ? "tool error" : "tool result",
                    systemImage: isError ? "xmark.circle" : "arrow.turn.down.right")
                .font(.caption)
                .foregroundStyle(isError ? .red : .secondary)
            }
        case .systemInit(let model):
            Label(model.map { "session started · \($0)" } ?? "session started",
                  systemImage: "power")
                .font(.caption)
                .foregroundStyle(.secondary)
        case .systemNote(let subtype):
            // Every non-`init` system record used to claim the session had
            // just started. One quiet line instead.
            Label(subtype, systemImage: "gearshape")
                .font(.caption2)
                .foregroundStyle(.tertiary)
        case .result(let summary):
            Label(summary, systemImage: "flag.checkered")
                .font(.caption.weight(.medium))
                .foregroundStyle(.secondary)
                .padding(.top, 4)
        case .raw(let truncatedByServer):
            RawLineView(line: line, truncatedByServer: truncatedByServer)
        }
    }
}

/// An unparsed line. Short ones (a dropped-lines marker, a stderr note) render
/// inline exactly as before; anything longer — typically a server-cut
/// stream-json record, which is a single physical line of escaped JSON —
/// collapses behind a one-line preview so it can't swallow the pane.
private struct RawLineView: View {
    let line: TranscriptLine
    let truncatedByServer: Bool

    /// Below this a disclosure arrow costs more than it saves.
    private static let inlineLimit = 200
    /// Longest first-line preview shown on the disclosure label.
    private static let previewLimit = 120

    var body: some View {
        if line.line.count <= Self.inlineLimit {
            text(line.line)
        } else {
            DisclosureGroup {
                // The content is built whether or not it's expanded, so the
                // cap has to be on the string, not on the disclosure state.
                text(TranscriptRecord.capped(line.line))
            } label: {
                Label(summary, systemImage: symbol)
                    .font(.caption)
                    .foregroundStyle(line.stream == .stderr ? .orange : .secondary)
                    .lineLimit(1)
            }
        }
    }

    private func text(_ content: String) -> some View {
        Text(content)
            .font(.caption.monospaced())
            .foregroundStyle(line.stream == .stderr ? .orange : .secondary)
            .textSelection(.enabled)
            .frame(maxWidth: .infinity, alignment: .leading)
    }

    /// Plain `String`, not an interpolated literal — that would go through
    /// the `LocalizedStringKey` overload of `Label`.
    private var summary: String { "\(kind) · \(preview)" }

    private var kind: String {
        if truncatedByServer { return "truncated record" }
        return line.stream == .stderr ? "stderr" : "unparsed record"
    }

    private var symbol: String { truncatedByServer ? "scissors" : "curlybraces" }

    private var preview: String {
        // Escaped-JSON walls are one physical line; stderr backtraces are not.
        let first = line.line.prefix(while: { !$0.isNewline })
        return first.count > Self.previewLimit
            ? String(first.prefix(Self.previewLimit)) + "…"
            : String(first)
    }
}

struct AssistantBlockView: View {
    let block: AssistantBlock

    var body: some View {
        switch block {
        case .text(let text):
            Text(text)
                .textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .leading)
        case .thinking(let text):
            DisclosureGroup {
                Text(text)
                    .font(.callout.italic())
                    .foregroundStyle(.secondary)
                    .textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
            } label: {
                Label("Thinking", systemImage: "brain")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        case .toolUse(let name, let input):
            DisclosureGroup {
                Text(input)
                    .font(.caption.monospaced())
                    .textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
            } label: {
                Label(name, systemImage: "wrench.and.screwdriver")
                    .font(.caption.weight(.medium))
                    .foregroundStyle(.blue)
            }
        }
    }
}

enum AssistantBlock {
    case text(String)
    case thinking(String)
    case toolUse(name: String, input: String)
}

/// Lenient projection of one stream-json record. The schema belongs to
/// Claude Code; anything unrecognized falls through to `.raw`.
enum TranscriptRecord {
    case assistantBlocks([AssistantBlock])
    case toolResult(summary: String, isError: Bool)
    case systemInit(model: String?)
    case systemNote(subtype: String)
    case result(summary: String)
    case raw(truncated: Bool)

    /// Prefix the server appends to any line it cuts (`truncate_line` in
    /// `crates/tasks/src/scout.rs`). Cutting a stream-json record leaves it
    /// unparseable, so this is how we tell "the server cut it" apart from
    /// "the agent printed junk". Cross-language contract: the wording after
    /// the prefix may change, the prefix may not.
    private static let truncationMarker = "[tasks: truncated "

    /// Longest tool payload we hand to a `Text`. Roughly 60 lines of code —
    /// enough to see what a tool did, small enough to render instantly.
    static let bodyLimit = 4_000

    /// Cut `text` to ``bodyLimit`` characters, marking what went missing.
    static func capped(_ text: String) -> String {
        guard text.count > bodyLimit else { return text }
        let dropped = text.count - bodyLimit
        return String(text.prefix(bodyLimit)) + "\n…[truncated \(dropped) more characters]"
    }

    init(_ line: TranscriptLine) {
        // Lazy: scanning for the marker is only worth it once a line has
        // already failed to parse.
        func unparsed() -> TranscriptRecord {
            .raw(truncated: line.line.contains(TranscriptRecord.truncationMarker))
        }

        guard line.stream == .stdout,
            let data = line.line.data(using: .utf8),
            let json = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
            let type = json["type"] as? String
        else {
            self = unparsed()
            return
        }

        switch type {
        case "assistant":
            let content = (json["message"] as? [String: Any])?["content"] as? [[String: Any]] ?? []
            let blocks: [AssistantBlock] = content.compactMap { block in
                switch block["type"] as? String {
                case "text":
                    return (block["text"] as? String).map(AssistantBlock.text)
                case "thinking":
                    return (block["thinking"] as? String).map(AssistantBlock.thinking)
                case "tool_use":
                    let name = block["name"] as? String ?? "tool"
                    let input = Self.capped(Self.compactJSON(block["input"]) ?? "")
                    return .toolUse(name: name, input: input)
                default:
                    return nil
                }
            }
            self = blocks.isEmpty ? unparsed() : .assistantBlocks(blocks)
        case "user":
            let content = (json["message"] as? [String: Any])?["content"] as? [[String: Any]] ?? []
            guard let result = content.first(where: { $0["type"] as? String == "tool_result" })
            else {
                self = unparsed()
                return
            }
            let isError = result["is_error"] as? Bool ?? false
            let summary = Self.capped(
                Self.flattenToolResult(result["content"]) ?? "(empty result)")
            self = .toolResult(summary: summary, isError: isError)
        case "system":
            // `case "init"` matches through the optional, so it has to come
            // first — otherwise `case let subtype?` swallows it.
            switch json["subtype"] as? String {
            case "init": self = .systemInit(model: json["model"] as? String)
            case let subtype?: self = .systemNote(subtype: subtype)
            case nil: self = .systemNote(subtype: "system")
            }
        case "result":
            var parts: [String] = [json["subtype"] as? String ?? "result"]
            if let turns = json["num_turns"] as? Int { parts.append("\(turns) turns") }
            if let cost = json["total_cost_usd"] as? Double {
                parts.append(String(format: "$%.2f", cost))
            }
            if let ms = json["duration_ms"] as? Int {
                parts.append(Duration.milliseconds(ms).formatted(
                    .units(allowed: [.hours, .minutes, .seconds], width: .abbreviated)))
            }
            self = .result(summary: parts.joined(separator: " · "))
        default:
            self = unparsed()
        }
    }

    private static func flattenToolResult(_ content: Any?) -> String? {
        if let text = content as? String { return text }
        if let blocks = content as? [[String: Any]] {
            let texts = blocks.compactMap { $0["text"] as? String }
            if !texts.isEmpty { return texts.joined(separator: "\n") }
        }
        return compactJSON(content)
    }

    private static func compactJSON(_ value: Any?) -> String? {
        guard let value, JSONSerialization.isValidJSONObject(value),
            let data = try? JSONSerialization.data(withJSONObject: value, options: [.sortedKeys])
        else { return value.map { "\($0)" } }
        return String(data: data, encoding: .utf8)
    }
}
