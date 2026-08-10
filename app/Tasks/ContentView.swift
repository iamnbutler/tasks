import SwiftUI

enum NavSection: Hashable {
    case tasks, queue, activity, chat
}

struct ContentView: View {
    @Environment(AppModel.self) private var model
    @State private var section: NavSection? = .queue
    @State private var selectedTask: TaskItem.ID?
    @State private var selectedQueueTask: TaskItem.ID?
    /// Events newer than this get the unread accent while Activity is open.
    /// Captured from `lastSeenSeq` when the section is entered, then the model
    /// marks everything read — so the accent survives the visit but the badge
    /// clears.
    @State private var unreadBoundary: Int64 = 0

    var body: some View {
        NavigationSplitView {
            sidebar
                .navigationSplitViewColumnWidth(min: 150, ideal: 180)
        } content: {
            listColumn
                .navigationSplitViewColumnWidth(min: 320, ideal: 420)
        } detail: {
            detail
                .frame(minWidth: 380)
        }
        .navigationTitle("Tasks")
        .navigationSubtitle(subtitle)
        .toolbar { toolbarContent }
        .onChange(of: section) { _, next in
            if next == .activity {
                unreadBoundary = model.lastSeenSeq
                model.markActivityRead()
            }
        }
    }

    // MARK: Toolbar

    @ToolbarContentBuilder
    private var toolbarContent: some ToolbarContent {
        ToolbarItemGroup {
            Button("Play", systemImage: "play.fill") {
                Task { await model.setMode(.play) }
            }
            .disabled(model.mode == .play)
            .help(model.mode == .play ? "Dispatching is on" : "Start dispatching queued tasks")

            Button("Pause", systemImage: "pause.fill") {
                Task { await model.setMode(.pause) }
            }
            .disabled(model.mode == .pause)
            .help("Stop starting new scouts (in-flight scouts finish)")
        }
        ToolbarItem {
            Button("Refresh", systemImage: "arrow.clockwise") {
                Task { await model.refresh() }
            }
            .help("Refetch everything")
        }
    }

    // MARK: Sidebar

    private var sidebar: some View {
        List(selection: $section) {
            Label("Tasks", systemImage: "tray.full")
                .tag(NavSection.tasks)
            Label("Queue", systemImage: "list.number")
                .badge(queuedWork.count)
                .tag(NavSection.queue)
            Label("Activity", systemImage: "waveform.path.ecg")
                .badge(model.unreadCount)
                .tag(NavSection.activity)
            Label("Chat", systemImage: "bubble.left.and.bubble.right")
                .tag(NavSection.chat)
        }
        .listStyle(.sidebar)
        .safeAreaInset(edge: .bottom, spacing: 0) {
            if let error = model.connectionError {
                ConnectionBanner(error: error)
            }
        }
    }

    private var queuedWork: [TaskItem] {
        model.tasks.filter { $0.state.isQueuedWork }
    }

    // MARK: List column

    @ViewBuilder
    private var listColumn: some View {
        if model.lastRefreshed == nil && model.connectionError == nil {
            ContentUnavailableView {
                ProgressView()
            } description: {
                Text("Connecting to the tasks server…")
            }
        } else {
            sectionList
        }
    }

    @ViewBuilder
    private var sectionList: some View {
        switch section {
        case .tasks:
            TasksTable(selection: $selectedTask)
        case .queue, nil:
            queueList
        case .activity:
            ActivityFeed(unreadBoundary: unreadBoundary)
        case .chat:
            ContentUnavailableView(
                "No orchestrator yet",
                systemImage: "bubble.left.and.bubble.right",
                description: Text("The orchestrator session ships next — this is where you'll talk to it."))
        }
    }

    /// The queue, grouped in attention order: verdicts you owe, work running
    /// now, work up next (reorderable), and approved specs parked for a
    /// builder.
    private var queueList: some View {
        List(selection: $selectedQueueTask) {
            let needsYou = tasksIn(.inReview)
            let running = tasksIn(.scouting)
            let building = tasksIn(.building)
            let upNext = tasksIn(.queued)
            let readyToBuild = tasksIn(.readyToBuild)

            if queuedWork.isEmpty {
                Text("Nothing queued — pick tasks up from the Tasks list")
                    .foregroundStyle(.tertiary)
                    .font(.callout)
            }
            if !needsYou.isEmpty {
                Section("Needs you") {
                    ForEach(needsYou) { task in
                        QueueRow(task: task, accessory: .complexity(latestSpec(for: task)?.complexity))
                    }
                }
            }
            if !running.isEmpty {
                Section("Running") {
                    ForEach(running) { task in
                        QueueRow(task: task, accessory: .elapsed(runningSession(for: task)?.startedAt))
                    }
                }
            }
            if !building.isEmpty {
                Section("Building") {
                    ForEach(building) { task in
                        QueueRow(task: task, accessory: .elapsed(model.runningBuild?.startedAt))
                    }
                }
            }
            if !upNext.isEmpty {
                Section("Up next") {
                    ForEach(upNext) { task in
                        QueueRow(task: task, accessory: .none)
                            .contextMenu {
                                Button("Scout Now") { Task { await model.scoutNow(task.id) } }
                                Button("Remove from Queue") { Task { await model.dequeueTask(task.id) } }
                            }
                    }
                    .onMove { source, destination in
                        model.moveQueued(from: source, to: destination)
                    }
                }
            }
            if !readyToBuild.isEmpty {
                Section("Ready to build") {
                    ForEach(readyToBuild) { task in
                        QueueRow(task: task, accessory: .complexity(latestSpec(for: task)?.complexity))
                            .contextMenu {
                                if let spec = latestSpec(for: task) {
                                    Button("Build") {
                                        Task { await model.buildNow(specId: spec.id) }
                                    }
                                }
                            }
                    }
                }
            }
        }
        .navigationTitle("Queue")
    }

    private func tasksIn(_ state: TaskState) -> [TaskItem] {
        model.tasks.filter { $0.state == state }
    }

    private func latestSpec(for task: TaskItem) -> Spec? {
        model.specs
            .filter { $0.taskId == task.id }
            .max { $0.createdAt < $1.createdAt }
    }

    private func runningSession(for task: TaskItem) -> ScoutSession? {
        model.sessions
            .filter { $0.taskId == task.id && $0.status == .running }
            .max { $0.startedAt < $1.startedAt }
    }

    // MARK: Detail

    @ViewBuilder
    private var detail: some View {
        switch section {
        case .tasks:
            if let id = selectedTask, let task = model.task(id) {
                TaskDetailView(task: task)
            } else {
                noSelection("Select a task")
            }
        case .queue, nil:
            queueDetail
        case .activity:
            noSelection("The feed is the whole story")
        case .chat:
            noSelection("")
        }
    }

    /// The detail pane follows the task's state: a spec awaiting a verdict
    /// shows the spec + review form, a running scout shows its live session,
    /// anything else shows the task itself.
    @ViewBuilder
    private var queueDetail: some View {
        if let id = selectedQueueTask, let task = model.task(id) {
            switch task.state {
            case .inReview, .readyToBuild:
                if let spec = latestSpec(for: task) {
                    SpecDetailView(
                        spec: spec,
                        entry: model.queueEntry(forSpec: spec.id),
                        task: task)
                } else {
                    TaskDetailView(task: task)
                }
            case .scouting:
                if let session = runningSession(for: task) {
                    SessionDetailView(session: session, task: task)
                } else {
                    TaskDetailView(task: task)
                }
            default:
                TaskDetailView(task: task)
            }
        } else {
            noSelection("Select queued work")
        }
    }

    private func noSelection(_ text: String) -> some View {
        ContentUnavailableView(text, systemImage: "sidebar.left")
    }

    private var subtitle: String {
        model.projects.isEmpty
            ? "no projects" : model.projects.map(\.slug).joined(separator: " · ")
    }
}

// MARK: - Tasks table

/// Linear-style table over every open issue. Backlog rows are plain — state
/// only shows once work exists. Right-click is the golden path in.
struct TasksTable: View {
    @Environment(AppModel.self) private var model
    @Binding var selection: TaskItem.ID?

    var body: some View {
        Table(model.tasks, selection: $selection) {
            TableColumn("#") { task in
                Text("\(task.ghIssueNumber)")
                    .monospacedDigit()
                    .foregroundStyle(.secondary)
            }
            .width(min: 40, ideal: 48, max: 60)

            TableColumn("Title") { task in
                Text(task.title).lineLimit(1)
            }

            TableColumn("Labels") { task in
                Text(task.labels.joined(separator: ", "))
                    .lineLimit(1)
                    .foregroundStyle(.secondary)
            }
            .width(min: 60, ideal: 120)

            TableColumn("State") { task in
                if task.state != .backlog {
                    StatusBadge(text: task.state.display, color: task.state.color)
                }
            }
            .width(min: 80, ideal: 110)

            TableColumn("Updated") { task in
                Text(task.updatedAt.formatted(.relative(presentation: .named)))
                    .foregroundStyle(.secondary)
            }
            .width(min: 80, ideal: 110)
        }
        .contextMenu(forSelectionType: TaskItem.ID.self) { ids in
            if let id = ids.first, let task = model.task(id) {
                taskActions(task)
            }
        }
        .navigationTitle("Tasks")
    }

    @ViewBuilder
    private func taskActions(_ task: TaskItem) -> some View {
        if task.state == .backlog {
            Button("Add to Queue") { Task { await model.queueTask(task.id) } }
            Button("Scout Now") { Task { await model.scoutNow(task.id) } }
        }
        if task.state == .queued {
            Button("Scout Now") { Task { await model.scoutNow(task.id) } }
            Button("Remove from Queue") { Task { await model.dequeueTask(task.id) } }
        }
        if let url = githubURL(for: task) {
            Divider()
            Link("Open on GitHub", destination: url)
        }
    }

    private func githubURL(for task: TaskItem) -> URL? {
        guard let project = model.projects.first(where: { $0.id == task.projectId }) else {
            return nil
        }
        return URL(string: "https://github.com/\(project.slug)/issues/\(task.ghIssueNumber)")
    }
}

// MARK: - Queue rows

struct QueueRow: View {
    enum Accessory {
        case none
        case complexity(Complexity?)
        case elapsed(Date?)
    }

    let task: TaskItem
    let accessory: Accessory

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            VStack(alignment: .leading, spacing: 1) {
                Text(task.title)
                    .lineLimit(2)
                Text("#\(task.ghIssueNumber)")
                    .monospaced()
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Spacer()
            switch accessory {
            case .none:
                EmptyView()
            case .complexity(let complexity):
                if let complexity {
                    Text(complexity.wire)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            case .elapsed(let start):
                if let start {
                    ElapsedTimeText(since: start)
                }
            }
        }
        .padding(.vertical, 2)
    }
}

/// Live-updating elapsed time — scouts run 20+ minutes, and a wall clock
/// beats a spinner that looks hung.
struct ElapsedTimeText: View {
    let since: Date

    var body: some View {
        TimelineView(.periodic(from: .now, by: 1)) { context in
            Text(elapsed(at: context.date))
                .monospacedDigit()
                .font(.caption)
                .foregroundStyle(.blue)
        }
    }

    private func elapsed(at now: Date) -> String {
        let seconds = max(0, Int(now.timeIntervalSince(since)))
        return String(format: "%d:%02d", seconds / 60, seconds % 60)
    }
}

// MARK: - Activity feed

struct ActivityFeed: View {
    @Environment(AppModel.self) private var model
    let unreadBoundary: Int64

    var body: some View {
        List(model.events) { event in
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                Circle()
                    .fill(event.seq > unreadBoundary ? Color.accentColor : .clear)
                    .frame(width: 6, height: 6)
                Image(systemName: icon(for: event.kind))
                    .foregroundStyle(.secondary)
                    .frame(width: 16)
                VStack(alignment: .leading, spacing: 1) {
                    Text(describe(event))
                        .lineLimit(2)
                    Text(event.timestamp.formatted(.relative(presentation: .named)))
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Spacer()
            }
            .padding(.vertical, 1)
        }
        .navigationTitle("Activity")
    }

    private func icon(for kind: String) -> String {
        switch kind {
        case "task_ingested": "tray.and.arrow.down"
        case "task_state_changed": "arrow.right.circle"
        case "task_gh_state_changed": "link.circle"
        case "session_started": "play.circle"
        case "session_completed": "checkmark.circle"
        case "spec_created": "doc.text"
        case "spec_queue_status_changed": "text.badge.checkmark"
        case "queue_reordered", "spec_queue_reordered": "arrow.up.arrow.down"
        case "build_requested": "hammer"
        case "build_started": "hammer.circle"
        case "build_completed": "checkmark.seal"
        case "pull_request_opened": "arrow.triangle.branch"
        case "mode_changed": "playpause"
        case "note": "text.bubble"
        case "project_added": "folder.badge.plus"
        default: "circle"
        }
    }

    private func describe(_ event: ActivityEvent) -> String {
        let title = event.taskId.flatMap { model.task($0)?.title }
        switch event.kind {
        case "task_ingested":
            return "Ingested \(title ?? "a task")"
        case "task_state_changed":
            let change = [event.from, event.to].compactMap(\.self)
                .map { $0.replacingOccurrences(of: "_", with: " ") }
                .joined(separator: " → ")
            return "\(title ?? "Task"): \(change)"
        case "task_gh_state_changed":
            return "\(title ?? "A task") was \(event.to ?? "changed") on GitHub"
        case "session_started":
            return "Scout started on \(title ?? "a task")"
        case "session_completed":
            return "Scout finished on \(title ?? "a task")"
        case "spec_created":
            return "Spec produced for \(title ?? "a task")"
        case "spec_queue_status_changed":
            let verdict = (event.to ?? "updated").replacingOccurrences(of: "_", with: " ")
            return "Spec \(verdict)"
        case "queue_reordered":
            return "Queue reordered"
        case "spec_queue_reordered":
            return "Spec queue reordered"
        case "build_requested":
            return "Build requested"
        case "build_started":
            return "Build started"
        case "build_completed":
            let status = event.status ?? "finished"
            return "Build \(status)"
        case "pull_request_opened":
            if let pr = event.prNumber {
                return "Pull request #\(pr) opened"
            }
            return "Pull request opened"
        case "mode_changed":
            return "Mode: \(event.from ?? "?") → \(event.to ?? "?")"
        case "note":
            return event.message ?? "Note"
        case "project_added":
            return "Project added"
        default:
            return event.kind.replacingOccurrences(of: "_", with: " ")
        }
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
        Text(text)
            .font(.caption.weight(.medium))
            .padding(.horizontal, 7)
            .padding(.vertical, 2)
            .background(color.opacity(0.15), in: Capsule())
            .foregroundStyle(color)
    }
}

extension TaskState {
    var display: String {
        wire.replacingOccurrences(of: "_", with: " ")
    }

    var color: Color {
        switch self {
        case .backlog: .gray
        case .queued: .orange
        case .scouting: .blue
        case .inReview: .purple
        case .readyToBuild: .teal
        case .building: .indigo
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
