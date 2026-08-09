import SwiftUI

struct TaskDetailView: View {
    let task: TaskItem

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 12) {
                HStack(alignment: .firstTextBaseline, spacing: 8) {
                    Text(task.title)
                        .font(.title2.weight(.semibold))
                        .textSelection(.enabled)
                    Spacer()
                    StatusBadge(text: task.state.wire, color: task.state.color)
                }

                HStack(spacing: 8) {
                    Text("#\(task.ghIssueNumber)")
                        .font(.callout.monospaced())
                        .foregroundStyle(.secondary)
                    StatusBadge(
                        text: task.ghState.wire,
                        color: task.ghState == .open ? .green : .purple)
                        .help("GitHub state as of the last poll — not live")
                    ForEach(task.labels, id: \.self) { label in
                        StatusBadge(text: label, color: .secondary)
                    }
                }

                DetailFields {
                    LabeledContent("Priority", value: "\(task.priority)")
                    LabeledContent("Manual rank", value: task.manualRank.map(String.init) ?? "—")
                    if let attempts = task.dispatchAttempts, attempts > 0 {
                        LabeledContent("Dispatch attempts", value: "\(attempts)")
                    }
                    LabeledContent("Ingested", value: task.ingestedAt.formatted())
                    LabeledContent("Updated", value: task.updatedAt.formatted())
                }

                Divider()

                MarkdownBody(text: task.body)
            }
            .padding(16)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
    }
}

struct SpecDetailView: View {
    let spec: Spec
    let entry: SpecQueueItem?
    let task: TaskItem?

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 12) {
                HStack(alignment: .firstTextBaseline, spacing: 8) {
                    Text(task?.title ?? "Spec")
                        .font(.title2.weight(.semibold))
                        .textSelection(.enabled)
                    Spacer()
                    if let entry {
                        StatusBadge(text: entry.status.wire, color: entry.status.color)
                    }
                }

                DetailFields {
                    LabeledContent("Complexity", value: spec.complexity.wire)
                    LabeledContent("Created", value: spec.createdAt.formatted())
                    if let rank = entry?.rank {
                        LabeledContent("Rank", value: "\(rank)")
                    }
                    if let approved = entry?.approvedAt {
                        LabeledContent("Approved", value: approved.formatted())
                    }
                    if let code = spec.agentExitCode {
                        LabeledContent("Agent exit", value: "\(code)")
                    }
                }

                if let feedback = entry?.feedback, !feedback.isEmpty {
                    Callout(title: "Review feedback", text: feedback, color: .orange)
                }

                if !spec.filesTouched.isEmpty {
                    DisclosureGroup("Files touched (\(spec.filesTouched.count))") {
                        VStack(alignment: .leading, spacing: 2) {
                            ForEach(spec.filesTouched, id: \.self) { file in
                                Text(file)
                                    .font(.caption.monospaced())
                                    .textSelection(.enabled)
                            }
                        }
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(.top, 4)
                    }
                }

                Divider()

                MarkdownBody(text: spec.content)
            }
            .padding(16)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
    }
}

struct SessionDetailView: View {
    let session: ScoutSession
    let task: TaskItem?

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 12) {
                HStack(alignment: .firstTextBaseline, spacing: 8) {
                    Text(task?.title ?? "Scout session")
                        .font(.title2.weight(.semibold))
                        .textSelection(.enabled)
                    Spacer()
                    StatusBadge(text: session.status.wire, color: session.status.color)
                }

                DetailFields {
                    LabeledContent("Branch") {
                        Text(session.branch)
                            .font(.callout.monospaced())
                            .textSelection(.enabled)
                    }
                    LabeledContent("VM", value: session.vmId ?? "—")
                    LabeledContent("Started", value: session.startedAt.formatted())
                    if session.completedAt == nil {
                        // Scouts run for tens of minutes; a live clock reads
                        // as "working", a static timestamp reads as "hung".
                        LabeledContent("Elapsed") {
                            Text(session.startedAt, style: .timer)
                                .monospacedDigit()
                        }
                    }
                    if let completed = session.completedAt {
                        LabeledContent("Completed", value: completed.formatted())
                        LabeledContent(
                            "Duration",
                            value: Duration.seconds(
                                completed.timeIntervalSince(session.startedAt)
                            ).formatted(.units(allowed: [.hours, .minutes, .seconds], width: .abbreviated)))
                    }
                }

                if let reason = session.exitReason, !reason.isEmpty {
                    Callout(title: "Exit reason", text: reason, color: .red)
                }

                Divider()

                // Transcript pane lands here once the server persists scout
                // output — docs/plans/2026-08-09-session-transcripts.md.
                ContentUnavailableView {
                    Label("No transcript", systemImage: "text.bubble")
                } description: {
                    Text("The server doesn't capture scout output yet.")
                }
            }
            .padding(16)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
    }
}

// MARK: - Pieces

/// Two-column key/value block used across the detail views.
struct DetailFields<Content: View>: View {
    @ViewBuilder let content: Content

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            content
        }
        .labeledContentStyle(.detailField)
    }
}

struct DetailFieldStyle: LabeledContentStyle {
    func makeBody(configuration: Configuration) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            configuration.label
                .foregroundStyle(.secondary)
                .frame(width: 100, alignment: .trailing)
            configuration.content
                .textSelection(.enabled)
        }
        .font(.callout)
    }
}

extension LabeledContentStyle where Self == DetailFieldStyle {
    static var detailField: DetailFieldStyle { DetailFieldStyle() }
}

struct Callout: View {
    let title: String
    let text: String
    let color: Color

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(title)
                .font(.caption.weight(.semibold))
                .foregroundStyle(color)
            Text(text)
                .font(.callout)
                .textSelection(.enabled)
        }
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(color.opacity(0.08), in: RoundedRectangle(cornerRadius: 6))
    }
}

/// Renders GitHub-flavored markdown-ish text. Inline styles only, newlines
/// preserved — good enough until a real markdown view earns its place.
struct MarkdownBody: View {
    let text: String

    var body: some View {
        if let attributed = try? AttributedString(
            markdown: text,
            options: .init(interpretedSyntax: .inlineOnlyPreservingWhitespace))
        {
            Text(attributed)
                .textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .leading)
        } else {
            Text(text)
                .textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
    }
}
