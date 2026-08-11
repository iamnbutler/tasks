import SwiftUI

/// The launch surface: "what is going on, and does anything need me?" —
/// three LLM-written briefings (state of the project / changes / issues)
/// with the mechanical rows that must not wait on prose (needs-you, in
/// flight) between them. Briefings are a cache with a visible date: the "as
/// of" caption is part of the content, not chrome. Every mechanical row
/// points into Queue; no review/build/queue actions live here.
struct HomeView: View {
    @Environment(AppModel.self) private var model
    /// Rows are pointers, not surfaces: this flips the sidebar to Queue.
    let openQueue: () -> Void

    var body: some View {
        ScrollView {
            // Bounded content: prose plus a handful of rows — plain VStack,
            // not lazy (see Markdown.swift on lazy-container churn).
            VStack(alignment: .leading, spacing: 28) {
                briefing("state_of_project", title: "State of the project")
                needsYou
                inFlight
                briefing("changes", title: "Changes")
                briefing("issues", title: "Issues")
            }
            .padding(24)
            .frame(maxWidth: 760, alignment: .leading)
            .frame(maxWidth: .infinity, alignment: .center)
        }
        .navigationTitle("Home")
    }

    // MARK: Briefings

    @ViewBuilder
    private func briefing(_ section: String, title: String) -> some View {
        let status = model.briefing(section)
        HomeSection(title, caption: status.map(caption)) {
            if let content = status?.content {
                MarkdownView(text: content)
                    .textSelection(.enabled)
            } else if status?.regenerating == true {
                EmptyLine("Writing the first briefing…")
            } else {
                // Failure with no stored copy included: the caption carries
                // "couldn't refresh", and there is nothing honest to show.
                EmptyLine("No briefing yet")
            }
        }
    }

    /// The slot's provenance in one line: age, refresh-in-flight, failure.
    /// Never blank once content exists — prose without a date would read as
    /// current, and briefings are exactly the surface that must not lie
    /// about freshness.
    private func caption(for status: BriefingStatus) -> String {
        var parts: [String] = []
        if let generatedAt = status.generatedAt {
            parts.append("as of \(elapsed(since: generatedAt))")
        }
        if status.regenerating {
            parts.append("refreshing…")
        } else if status.error != nil {
            parts.append("couldn't refresh")
        }
        return parts.joined(separator: " · ")
    }

    // MARK: Needs you

    @ViewBuilder
    private var needsYou: some View {
        HomeSection("Needs you") {
            let pending = model.specsAwaitingReview
            let failed = model.failedBuilds.prefix(5)
            if pending.isEmpty && failed.isEmpty {
                EmptyLine("Nothing needs you")
            }
            ForEach(pending) { entry in
                HomeRow(action: openQueue) {
                    HomeRowBody(
                        icon: "doc.text.magnifyingglass",
                        title: model.title(forTask: entry.taskId) ?? "Spec awaiting review",
                        subtitle: "Spec awaiting your verdict",
                        trailing: model.waitingSince(specId: entry.specId)
                            .map { "waiting \(elapsed(since: $0))" })
                }
            }
            ForEach(failed) { build in
                HomeRow(action: openQueue) {
                    HomeRowBody(
                        icon: "exclamationmark.triangle",
                        title: "Build failed — \(model.label(for: build))",
                        subtitle: build.exitReason ?? build.branch,
                        trailing: elapsed(since: build.finishedOrCreatedAt))
                }
            }
        }
    }

    // MARK: In flight

    @ViewBuilder
    private var inFlight: some View {
        HomeSection("In flight") {
            let scouts = model.runningSessions
            let build = model.runningBuild
            if scouts.isEmpty && build == nil {
                EmptyLine("Nothing running")
            }
            ForEach(scouts) { session in
                HomeRow(action: openQueue) {
                    HomeRowBody(
                        icon: "binoculars",
                        title: model.title(forTask: session.taskId) ?? "Scout",
                        subtitle: session.branch,
                        trailing: elapsed(since: session.startedAt))
                }
            }
            if let build {
                HomeRow(action: openQueue) {
                    HomeRowBody(
                        icon: "hammer",
                        title: model.label(for: build),
                        subtitle: build.branch,
                        trailing: build.startedAt.map { elapsed(since: $0) })
                }
            }
        }
    }

    private func elapsed(since date: Date) -> String {
        date.formatted(.relative(presentation: .named))
    }
}

// MARK: - Building blocks

/// A titled block with an optional right-aligned caption.
private struct HomeSection<Content: View>: View {
    let title: String
    let caption: String?
    @ViewBuilder let content: Content

    init(_ title: String, caption: String? = nil, @ViewBuilder content: () -> Content) {
        self.title = title
        self.caption = caption
        self.content = content()
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(alignment: .firstTextBaseline) {
                Text(title)
                    .font(.headline)
                Spacer()
                if let caption, !caption.isEmpty {
                    Text(caption)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }
            content
        }
    }
}

/// A row that navigates (into Queue). The whole row is the hit target.
private struct HomeRow<Content: View>: View {
    let action: () -> Void
    @ViewBuilder let content: Content

    var body: some View {
        Button(action: action) {
            content
        }
        .buttonStyle(.plain)
    }
}

/// Icon + title/subtitle + trailing detail, the shape every Home row shares.
private struct HomeRowBody: View {
    let icon: String
    let title: String
    let subtitle: String?
    let trailing: String?

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            Image(systemName: icon)
                .frame(width: 18)
                .foregroundStyle(.secondary)
            VStack(alignment: .leading, spacing: 1) {
                Text(title)
                    .lineLimit(1)
                if let subtitle {
                    Text(subtitle)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
            }
            Spacer()
            if let trailing {
                Text(trailing)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .monospacedDigit()
            }
        }
        .padding(.vertical, 5)
        .padding(.horizontal, 8)
        .background(.quaternary.opacity(0.4), in: RoundedRectangle(cornerRadius: 8))
        .contentShape(Rectangle())
    }
}

/// A quiet placeholder line for an empty block.
private struct EmptyLine: View {
    let text: String

    init(_ text: String) { self.text = text }

    var body: some View {
        Text(text)
            .font(.callout)
            .foregroundStyle(.tertiary)
    }
}
