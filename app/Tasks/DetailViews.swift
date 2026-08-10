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

                MarkdownView(text: task.body)
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

                MarkdownView(text: spec.content)

                if entry != nil {
                    Divider()

                    ReviewForm(specId: spec.id)
                }
            }
            .padding(16)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
    }
}

/// Verdict buttons + feedback field. The server is the authority on verdict
/// legality — this form only pre-disables the obviously-unhelpful case
/// (needs_revision without feedback, since feedback is the reviewer's
/// message to the next scout).
struct ReviewForm: View {
    @Environment(AppModel.self) private var model
    let specId: String

    @State private var feedback = ""
    @State private var submitting = false
    @State private var errorMessage: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("Verdict")
                .font(.headline)

            TextField(
                "Feedback — fed to the next scout on needs revision",
                text: $feedback, axis: .vertical
            )
            .lineLimit(3...8)
            .textFieldStyle(.roundedBorder)

            HStack(spacing: 8) {
                Button("Approve") {
                    submit(.approved)
                }
                .buttonStyle(.borderedProminent)
                .tint(.green)

                Button("Needs revision") {
                    submit(.needsRevision)
                }
                .tint(.orange)
                .disabled(feedback.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                .help("Write feedback first — it's the message the re-scout sees")

                Button("Reject") {
                    submit(.rejected)
                }
                .tint(.red)

                if submitting {
                    ProgressView()
                        .controlSize(.small)
                }
            }

            if let errorMessage {
                Text(errorMessage)
                    .font(.caption)
                    .foregroundStyle(.red)
            }
        }
        .disabled(submitting)
    }

    private func submit(_ verdict: ReviewVerdict) {
        submitting = true
        errorMessage = nil
        let trimmed = feedback.trimmingCharacters(in: .whitespacesAndNewlines)
        Task {
            do {
                try await model.review(
                    specId: specId,
                    verdict: verdict,
                    feedback: trimmed.isEmpty ? nil : trimmed)
                feedback = ""
            } catch {
                errorMessage = error.localizedDescription
            }
            submitting = false
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

                if let usage = session.usage {
                    UsageBadgeRow(usage: usage)
                }

                if let reason = session.exitReason, !reason.isEmpty {
                    Callout(title: "Exit reason", text: reason, color: .red)
                }

                Divider()

                TranscriptView(session: session)
            }
            .padding(16)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
    }
}

/// Compact chips for what a scout run cost, from the agent's final result
/// record. Renders whatever fields survived parsing; each is independently
/// optional.
struct UsageBadgeRow: View {
    let usage: SessionUsage

    var body: some View {
        HStack(spacing: 6) {
            if let cost = usage.totalCostUsd {
                StatusBadge(text: String(format: "$%.2f", cost), color: .green)
            }
            if let turns = usage.numTurns {
                StatusBadge(text: "\(turns) turns", color: .blue)
            }
            if let input = usage.inputTokens {
                StatusBadge(text: "\(input.formatted()) in", color: .secondary)
            }
            if let output = usage.outputTokens {
                StatusBadge(text: "\(output.formatted()) out", color: .secondary)
            }
            if let cached = usage.cacheReadInputTokens, cached > 0 {
                StatusBadge(text: "\(cached.formatted()) cached", color: .secondary)
            }
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
