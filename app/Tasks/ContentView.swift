import SwiftUI

enum NavSection: Hashable {
    case review, queue, sessions
}

struct ContentView: View {
    @Environment(AppModel.self) private var model
    @State private var section: NavSection? = .queue
    @State private var selectedTask: TaskItem.ID?
    @State private var selectedSpec: Spec.ID?
    @State private var selectedSession: ScoutSession.ID?

    var body: some View {
        NavigationSplitView {
            sidebar
                .navigationSplitViewColumnWidth(min: 150, ideal: 180)
        } content: {
            listColumn
                .navigationSplitViewColumnWidth(min: 300, ideal: 380)
        } detail: {
            detail
                .frame(minWidth: 380)
        }
        .navigationTitle("Tasks")
        .navigationSubtitle(subtitle)
        .toolbar {
            ToolbarItem {
                ModeBadge(mode: model.mode)
            }
            ToolbarItem {
                Button("Refresh", systemImage: "arrow.clockwise") {
                    Task { await model.refresh() }
                }
            }
        }
    }

    // MARK: Sidebar

    private var sidebar: some View {
        List(selection: $section) {
            Section {
                Label("Review", systemImage: "text.badge.checkmark")
                    .badge(pendingReviewCount)
                    .tag(NavSection.review)
                Label("Queue", systemImage: "list.number")
                    .badge(model.tasks.count)
                    .tag(NavSection.queue)
            }
            Section {
                Label("Sessions", systemImage: "terminal")
                    .tag(NavSection.sessions)
            }
        }
        .listStyle(.sidebar)
        .safeAreaInset(edge: .bottom, spacing: 0) {
            if let error = model.connectionError {
                ConnectionBanner(error: error)
            }
        }
    }

    private var pendingReviewCount: Int {
        model.specQueue.filter { $0.status == .pendingReview }.count
    }

    // MARK: List column

    @ViewBuilder
    private var listColumn: some View {
        switch section {
        case .review, nil:
            List(selection: $selectedSpec) {
                if model.specs.isEmpty {
                    emptyRow("No specs produced yet")
                }
                ForEach(reviewOrdered) { spec in
                    SpecRow(
                        spec: spec,
                        entry: model.queueEntry(forSpec: spec.id),
                        task: model.task(spec.taskId))
                }
            }
            .navigationTitle("Review")
        case .queue:
            List(selection: $selectedTask) {
                if model.tasks.isEmpty {
                    emptyRow("No tasks ingested yet")
                }
                ForEach(model.tasks) { task in
                    TaskRow(task: task)
                }
                .onMove { source, destination in
                    model.moveTasks(from: source, to: destination)
                }
            }
            .navigationTitle("Queue")
        case .sessions:
            List(selection: $selectedSession) {
                if model.sessions.isEmpty {
                    emptyRow("No scouts have run")
                }
                ForEach(model.sessions.sorted { $0.startedAt > $1.startedAt }) { session in
                    SessionRow(session: session, task: model.task(session.taskId))
                }
            }
            .navigationTitle("Sessions")
        }
    }

    /// Pending review floats to the top; everything else newest-first.
    private var reviewOrdered: [Spec] {
        model.specs.sorted { a, b in
            let aPending = model.queueEntry(forSpec: a.id)?.status == .pendingReview
            let bPending = model.queueEntry(forSpec: b.id)?.status == .pendingReview
            if aPending != bPending { return aPending }
            return a.createdAt > b.createdAt
        }
    }

    // MARK: Detail

    @ViewBuilder
    private var detail: some View {
        switch section {
        case .review, nil:
            if let id = selectedSpec, let spec = model.spec(id) {
                SpecDetailView(
                    spec: spec,
                    entry: model.queueEntry(forSpec: spec.id),
                    task: model.task(spec.taskId))
            } else {
                noSelection("Select a spec to review")
            }
        case .queue:
            if let id = selectedTask, let task = model.task(id) {
                TaskDetailView(task: task)
            } else {
                noSelection("Select a task")
            }
        case .sessions:
            if let id = selectedSession, let session = model.session(id) {
                SessionDetailView(session: session, task: model.task(session.taskId))
            } else {
                noSelection("Select a session")
            }
        }
    }

    private func noSelection(_ text: String) -> some View {
        ContentUnavailableView(text, systemImage: "sidebar.left")
    }

    private var subtitle: String {
        model.projects.isEmpty
            ? "no projects" : model.projects.map(\.slug).joined(separator: " · ")
    }

    private func emptyRow(_ text: String) -> some View {
        Text(text)
            .foregroundStyle(.tertiary)
            .font(.callout)
    }
}

// MARK: - Rows

struct TaskRow: View {
    let task: TaskItem

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            VStack(alignment: .leading, spacing: 1) {
                Text(task.title)
                    .lineLimit(2)
                HStack(spacing: 4) {
                    Text("#\(task.ghIssueNumber)")
                        .monospaced()
                    if !task.labels.isEmpty {
                        Text("· " + task.labels.joined(separator: " · "))
                            .lineLimit(1)
                    }
                }
                .font(.caption)
                .foregroundStyle(.secondary)
            }
            Spacer()
            // A closed issue on a non-terminal task usually means the work
            // shipped outside the pipeline — make that legible.
            if task.ghState == .closed {
                StatusBadge(text: "closed", color: .purple)
                    .help("GitHub issue is closed (as of the last poll)")
            }
            StatusBadge(text: task.state.wire, color: task.state.color)
        }
        .padding(.vertical, 2)
    }
}

struct SpecRow: View {
    let spec: Spec
    let entry: SpecQueueItem?
    let task: TaskItem?

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            VStack(alignment: .leading, spacing: 1) {
                Text(task?.title ?? spec.taskId)
                    .lineLimit(2)
                Text("\(spec.complexity.wire) · \(spec.createdAt.formatted(.relative(presentation: .named)))")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Spacer()
            if let entry {
                StatusBadge(text: entry.status.wire, color: entry.status.color)
            }
        }
        .padding(.vertical, 2)
    }
}

struct SessionRow: View {
    let session: ScoutSession
    let task: TaskItem?

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            VStack(alignment: .leading, spacing: 1) {
                Text(task?.title ?? session.taskId)
                    .lineLimit(2)
                Text(session.startedAt.formatted(.relative(presentation: .named)))
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Spacer()
            StatusBadge(text: session.status.wire, color: session.status.color)
        }
        .padding(.vertical, 2)
    }
}

// MARK: - Shared components

struct ConnectionBanner: View {
    let error: String

    var body: some View {
        Label {
            VStack(alignment: .leading, spacing: 2) {
                Text("Server unreachable")
                    .font(.callout.weight(.semibold))
                Text(error)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        } icon: {
            Image(systemName: "bolt.horizontal.circle")
                .foregroundStyle(.orange)
        }
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(.quaternary.opacity(0.5))
    }
}

struct StatusBadge: View {
    let text: String
    let color: Color

    var body: some View {
        Text(text.replacingOccurrences(of: "_", with: " "))
            .font(.caption.weight(.medium))
            .padding(.horizontal, 7)
            .padding(.vertical, 2)
            .background(color.opacity(0.15), in: Capsule())
            .foregroundStyle(color)
    }
}

struct ModeBadge: View {
    let mode: Mode?

    var body: some View {
        Label(mode?.wire ?? "unknown", systemImage: icon)
            .foregroundStyle(color)
            .labelStyle(.titleAndIcon)
    }

    private var icon: String {
        switch mode {
        case .play: "play.circle.fill"
        case .pause: "pause.circle.fill"
        case .stop: "stop.circle.fill"
        case .unknown, nil: "questionmark.circle"
        }
    }

    private var color: Color {
        switch mode {
        case .play: .green
        case .pause: .yellow
        case .stop: .red
        case .unknown, nil: .secondary
        }
    }
}

extension TaskState {
    var color: Color {
        switch self {
        case .new: .gray
        case .scouting: .blue
        case .specReady: .purple
        case .queued: .orange
        case .done: .green
        case .rejected: .red
        case .unknown: .secondary
        }
    }
}

extension SpecQueueStatus {
    var color: Color {
        switch self {
        case .pendingReview: .blue
        case .approved: .green
        case .needsRevision: .orange
        case .blocked: .gray
        case .rejected: .red
        case .unknown: .secondary
        }
    }
}

extension SessionStatus {
    var color: Color {
        switch self {
        case .running: .blue
        case .scoutSucceeded: .green
        case .scoutFailed: .red
        case .cancelled: .gray
        case .unknown: .secondary
        }
    }
}
