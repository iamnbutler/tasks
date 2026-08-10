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
        case .result(let summary):
            Label(summary, systemImage: "flag.checkered")
                .font(.caption.weight(.medium))
                .foregroundStyle(.secondary)
                .padding(.top, 4)
        case .raw:
            Text(line.line)
                .font(.caption.monospaced())
                .foregroundStyle(line.stream == .stderr ? .orange : .secondary)
                .textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
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
    case result(summary: String)
    case raw

    init(_ line: TranscriptLine) {
        guard line.stream == .stdout,
            let data = line.line.data(using: .utf8),
            let json = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
            let type = json["type"] as? String
        else {
            self = .raw
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
                    let input = Self.compactJSON(block["input"]) ?? ""
                    return .toolUse(name: name, input: input)
                default:
                    return nil
                }
            }
            self = blocks.isEmpty ? .raw : .assistantBlocks(blocks)
        case "user":
            let content = (json["message"] as? [String: Any])?["content"] as? [[String: Any]] ?? []
            guard let result = content.first(where: { $0["type"] as? String == "tool_result" })
            else {
                self = .raw
                return
            }
            let isError = result["is_error"] as? Bool ?? false
            let summary = Self.flattenToolResult(result["content"]) ?? "(empty result)"
            self = .toolResult(summary: summary, isError: isError)
        case "system":
            self = .systemInit(model: json["model"] as? String)
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
            self = .raw
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
