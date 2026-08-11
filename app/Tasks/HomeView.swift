import SwiftUI

/// The launch surface: "what is this project doing right now?" in four
/// read-only blocks — in flight, needs you, velocity, recent PRs. Every row
/// points into Queue (or out to GitHub); no review/build/queue actions live
/// here, so Home and Queue can't become two answers to the same question.
struct HomeView: View {
    @Environment(AppModel.self) private var model
    /// Rows are pointers, not surfaces: this flips the sidebar to Queue.
    let openQueue: () -> Void

    // `static` on purpose: a `private let` instance property would make the
    // memberwise `HomeView(openQueue:)` init private too.
    private static let windowDays = 7

    var body: some View {
        ScrollView {
            // Bounded content: a handful of rows per block — plain VStack,
            // not lazy (see Markdown.swift on lazy-container churn).
            VStack(alignment: .leading, spacing: 28) {
                inFlight
                needsYou
                velocity
                recentPullRequests
            }
            .padding(24)
            .frame(maxWidth: 760, alignment: .leading)
            .frame(maxWidth: .infinity, alignment: .center)
        }
        .navigationTitle("Home")
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

    // MARK: Velocity

    @ViewBuilder
    private var velocity: some View {
        HomeSection("Velocity", caption: "Last \(Self.windowDays) days") {
            if let v = model.velocity(days: Self.windowDays) {
                // A KPI row, not a chart: five headline numbers in ordinary
                // ink. No per-counter color (none is good or bad on its own)
                // and no delta (the wire carries no comparison period, and
                // inventing one would be the dashboard's first lie).
                HStack(spacing: 12) {
                    StatTile(value: v.ingested, label: "Tasks ingested")
                    StatTile(value: v.specsProduced, label: "Specs produced")
                    StatTile(value: v.specsApproved, label: "Specs approved")
                    StatTile(
                        value: v.buildsFinished, label: "Builds finished",
                        help: "Trips through the Builder, failures included")
                    StatTile(
                        value: v.prsOpened, label: "PRs opened",
                        help: "Opened, not merged — merge state lives on GitHub")
                }
            } else {
                // Backfill in progress: no numbers beats wrong zeros.
                EmptyLine("Counting…")
            }
        }
    }

    // MARK: Recent pull requests

    @ViewBuilder
    private var recentPullRequests: some View {
        HomeSection("Recent pull requests") {
            let recent = model.recentPullRequests.prefix(8)
            if recent.isEmpty {
                EmptyLine("No pull requests yet")
            }
            ForEach(recent) { build in
                if let pr = build.prNumber, let url = model.pullRequestURL(for: build) {
                    Link(destination: url) {
                        HomeRowBody(
                            icon: "arrow.triangle.pull",
                            title: model.label(for: build),
                            subtitle: "PR #\(pr) · \(build.branch)",
                            trailing: elapsed(since: build.finishedOrCreatedAt))
                    }
                    .buttonStyle(.plain)
                } else {
                    // Can't navigate (project vanished): plain text beats a
                    // dead control.
                    HomeRowBody(
                        icon: "arrow.triangle.pull",
                        title: model.label(for: build),
                        subtitle: build.branch,
                        trailing: elapsed(since: build.finishedOrCreatedAt))
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
                if let caption {
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

/// One large number over its label, in ordinary text ink.
private struct StatTile: View {
    let value: Int
    let label: String
    var help: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text("\(value)")
                .font(.title2.weight(.semibold))
                .monospacedDigit()
            Text(label)
                .font(.caption)
                .foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(10)
        .background(.quaternary.opacity(0.4), in: RoundedRectangle(cornerRadius: 8))
        .help(help ?? label)
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
