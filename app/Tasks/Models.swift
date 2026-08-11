import Foundation

// Mirrors crates/tasks/src/models.rs, per docs/clients.md. Keys arrive
// snake_case and are converted by the decoder's key strategy.
//
// Enums parse leniently (clients.md): an unknown wire value becomes
// .unknown(raw) and renders as its raw string instead of failing the decode —
// new states will appear as the pipeline grows.

/// String-backed wire enum with a lossless fallback for unknown values.
protocol WireEnum: Decodable, Hashable, Sendable {
    init(wire: String)
    var wire: String { get }
}

extension WireEnum {
    init(from decoder: any Decoder) throws {
        self.init(wire: try decoder.singleValueContainer().decode(String.self))
    }
}

struct Project: Decodable, Identifiable, Hashable {
    let id: String
    let repoOwner: String
    let repoName: String
    let addedAt: Date

    var slug: String { "\(repoOwner)/\(repoName)" }
}

// Named TaskItem rather than Task to stay out of Swift Concurrency's way.
struct TaskItem: Decodable, Identifiable, Hashable {
    let id: String
    let projectId: String
    let ghIssueNumber: UInt64
    let title: String
    let body: String
    let labels: [String]
    let ghState: GhState
    let state: TaskState
    let priority: Int
    let manualRank: Int?
    /// Landing in PR #757; absent on older servers.
    let dispatchAttempts: Int?
    let ingestedAt: Date
    let updatedAt: Date
}

enum GhState: WireEnum {
    case open, closed
    case unknown(String)

    init(wire: String) {
        switch wire {
        case "open": self = .open
        case "closed": self = .closed
        default: self = .unknown(wire)
        }
    }

    var wire: String {
        switch self {
        case .open: "open"
        case .closed: "closed"
        case .unknown(let raw): raw
        }
    }
}

/// One state per stage (docs/clients.md): backlog is the inert issue mirror;
/// everything from queued onward is explicitly picked-up work.
enum TaskState: WireEnum {
    case backlog, queued, scouting, inReview, readyToBuild, building, done, rejected
    case unknown(String)

    init(wire: String) {
        switch wire {
        case "backlog": self = .backlog
        case "queued": self = .queued
        case "scouting": self = .scouting
        case "in_review": self = .inReview
        case "ready_to_build": self = .readyToBuild
        case "building": self = .building
        case "done": self = .done
        case "rejected": self = .rejected
        default: self = .unknown(wire)
        }
    }

    var wire: String {
        switch self {
        case .backlog: "backlog"
        case .queued: "queued"
        case .scouting: "scouting"
        case .inReview: "in_review"
        case .readyToBuild: "ready_to_build"
        case .building: "building"
        case .done: "done"
        case .rejected: "rejected"
        case .unknown(let raw): raw
        }
    }

    /// Whether the task is picked-up work (anything past backlog and not dead).
    var isQueuedWork: Bool {
        switch self {
        case .queued, .scouting, .inReview, .readyToBuild, .building: true
        default: false
        }
    }
}

struct ScoutSession: Decodable, Identifiable, Hashable {
    let id: String
    let taskId: String
    let vmId: String?
    let branch: String
    let status: SessionStatus
    let startedAt: Date
    let completedAt: Date?
    let exitReason: String?
    /// Parsed from the agent's final stream-json `result` record. Nil for
    /// sessions predating transcript capture or that never reached a result.
    let usage: SessionUsage?
}

/// What one agent run cost. Everything optional — the shape belongs to
/// Claude Code, and a renamed upstream key costs a null, not a crash.
struct SessionUsage: Decodable, Hashable {
    let inputTokens: UInt64?
    let outputTokens: UInt64?
    let cacheReadInputTokens: UInt64?
    let cacheCreationInputTokens: UInt64?
    let totalCostUsd: Double?
    let durationMs: UInt64?
    let numTurns: UInt64?
}

/// One line of agent output. `seq` is dense per session, assigned by the
/// server at persist time; tailing clients resume with `since = last + 1`.
struct TranscriptLine: Decodable, Identifiable, Hashable {
    let sessionId: String
    let seq: Int64
    let timestamp: Date
    let stream: TranscriptStream
    let line: String

    var id: Int64 { seq }
}

enum TranscriptStream: WireEnum {
    case stdout, stderr
    case unknown(String)

    init(wire: String) {
        switch wire {
        case "stdout": self = .stdout
        case "stderr": self = .stderr
        default: self = .unknown(wire)
        }
    }

    var wire: String {
        switch self {
        case .stdout: "stdout"
        case .stderr: "stderr"
        case .unknown(let raw): raw
        }
    }
}

enum SessionStatus: WireEnum {
    case running, scoutSucceeded, scoutFailed, cancelled
    case unknown(String)

    init(wire: String) {
        switch wire {
        case "running": self = .running
        case "scout_succeeded": self = .scoutSucceeded
        case "scout_failed": self = .scoutFailed
        case "cancelled": self = .cancelled
        default: self = .unknown(wire)
        }
    }

    var wire: String {
        switch self {
        case .running: "running"
        case .scoutSucceeded: "scout_succeeded"
        case .scoutFailed: "scout_failed"
        case .cancelled: "cancelled"
        case .unknown(let raw): raw
        }
    }
}

struct Spec: Decodable, Identifiable, Hashable {
    let id: String
    let sessionId: String
    let taskId: String
    let content: String
    let complexity: Complexity
    let filesTouched: [String]
    /// Present after the PR #757 shape; absent on older servers.
    let agentExitCode: Int?
    let createdAt: Date

    // clients.md calls the deliverable `spec_markdown`; v2 HEAD serves
    // `content`. Accept both until the name settles.
    enum CodingKeys: String, CodingKey {
        case id, sessionId, taskId, content, specMarkdown
        case complexity, filesTouched, agentExitCode, createdAt
    }

    init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(String.self, forKey: .id)
        sessionId = try c.decode(String.self, forKey: .sessionId)
        taskId = try c.decode(String.self, forKey: .taskId)
        content =
            try c.decodeIfPresent(String.self, forKey: .content)
            ?? c.decode(String.self, forKey: .specMarkdown)
        complexity = try c.decode(Complexity.self, forKey: .complexity)
        filesTouched = try c.decodeIfPresent([String].self, forKey: .filesTouched) ?? []
        agentExitCode = try c.decodeIfPresent(Int.self, forKey: .agentExitCode)
        createdAt = try c.decode(Date.self, forKey: .createdAt)
    }
}

enum Complexity: WireEnum {
    case simple, medium, complex
    case unknown(String)

    init(wire: String) {
        switch wire {
        case "simple": self = .simple
        case "medium": self = .medium
        case "complex": self = .complex
        default: self = .unknown(wire)
        }
    }

    var wire: String {
        switch self {
        case .simple: "simple"
        case .medium: "medium"
        case .complex: "complex"
        case .unknown(let raw): raw
        }
    }
}

/// The server flattens SpecQueueEntry and joins in the spec's task_id.
struct SpecQueueItem: Decodable, Identifiable, Hashable {
    let specId: String
    let status: SpecQueueStatus
    let rank: Int?
    let approvedAt: Date?
    let feedback: String?
    let blockingDependencies: [String]
    let taskId: String

    var id: String { specId }
}

enum SpecQueueStatus: WireEnum {
    case pendingReview, approved, needsRevision, blocked, rejected, built
    case unknown(String)

    init(wire: String) {
        switch wire {
        case "pending_review": self = .pendingReview
        case "approved": self = .approved
        case "needs_revision": self = .needsRevision
        case "blocked": self = .blocked
        case "rejected": self = .rejected
        case "built": self = .built
        default: self = .unknown(wire)
        }
    }

    var wire: String {
        switch self {
        case .pendingReview: "pending_review"
        case .approved: "approved"
        case .needsRevision: "needs_revision"
        case .blocked: "blocked"
        case .rejected: "rejected"
        case .built: "built"
        case .unknown(let raw): raw
        }
    }
}

/// One serial Builder run over a set of approved specs, producing one branch
/// and one PR.
///
/// `prNumber` is an identifier, never a state: mergeability, checks and
/// open/closed are GitHub's and are never stored here — link out instead
/// (`https://github.com/{owner}/{repo}/pull/N`).
struct Build: Decodable, Identifiable, Hashable {
    let id: String
    let projectId: String
    let vmId: String?
    let branch: String
    let baseBranch: String
    let baseSha: String?
    let headSha: String?
    let prNumber: UInt64?
    let status: BuildStatus
    /// SUMMARY.md from the agent — the PR body — if it wrote one.
    let summary: String?
    let filesTouched: [String]
    let exitReason: String?
    let createdAt: Date
    let startedAt: Date?
    let completedAt: Date?
}

/// `GET /builds/{id}` and `POST /builds`: the build with its batch, flattened,
/// so `specIds` sits alongside the build's own fields rather than nested.
struct BuildDetail: Decodable, Identifiable, Hashable {
    let build: Build
    let specIds: [String]

    var id: String { build.id }

    private enum CodingKeys: String, CodingKey {
        case specIds
    }

    init(from decoder: any Decoder) throws {
        build = try Build(from: decoder)
        specIds = try decoder.container(keyedBy: CodingKeys.self)
            .decodeIfPresent([String].self, forKey: .specIds) ?? []
    }
}

enum BuildStatus: WireEnum {
    case queued, running, succeeded, failed
    case unknown(String)

    init(wire: String) {
        switch wire {
        case "queued": self = .queued
        case "running": self = .running
        case "succeeded": self = .succeeded
        case "failed": self = .failed
        default: self = .unknown(wire)
        }
    }

    var wire: String {
        switch self {
        case .queued: "queued"
        case .running: "running"
        case .succeeded: "succeeded"
        case .failed: "failed"
        case .unknown(let raw): raw
        }
    }
}

enum Mode: WireEnum {
    case play, pause, stop
    case unknown(String)

    init(wire: String) {
        switch wire {
        case "play": self = .play
        case "pause": self = .pause
        case "stop": self = .stop
        default: self = .unknown(wire)
        }
    }

    var wire: String {
        switch self {
        case .play: "play"
        case .pause: "pause"
        case .stop: "stop"
        case .unknown(let raw): raw
        }
    }
}

struct ModeResponse: Decodable {
    let mode: Mode
}

/// One event-log entry, fully typed.
///
/// Events are invalidation signals, not data (docs/clients.md): the payloads
/// are identifier-only by design and the server's row is the truth. Decoding
/// them precisely is still worth it — it's what lets a client refetch the one
/// entity that changed instead of the whole world, and it's what the wire
/// fixtures pin.
struct Event: Decodable, Identifiable, Hashable {
    let seq: Int64
    let timestamp: Date
    let payload: EventPayload

    var id: Int64 { seq }
}

/// The `kind`-tagged payload union. Unknown kinds decode to `.unknown` rather
/// than throwing, matching the lenient-enum policy everywhere else here — new
/// events will appear as the pipeline grows.
enum EventPayload: Decodable, Hashable {
    case projectAdded(projectId: String)
    case taskIngested(taskId: String, projectId: String)
    case taskStateChanged(taskId: String, from: TaskState, to: TaskState)
    case taskGhStateChanged(taskId: String, ghState: GhState)
    case sessionStarted(sessionId: String, taskId: String)
    case sessionCompleted(sessionId: String, taskId: String, status: SessionStatus)
    case specCreated(specId: String, taskId: String, sessionId: String)
    /// `from` is nil the first time a spec enters the queue.
    case specQueueStatusChanged(specId: String, from: SpecQueueStatus?, to: SpecQueueStatus)
    case queueReordered(taskIds: [String])
    case specQueueReordered(specIds: [String])
    case buildRequested(buildId: String, specIds: [String])
    case buildStarted(buildId: String)
    case buildCompleted(buildId: String, status: BuildStatus)
    case pullRequestOpened(buildId: String, prNumber: UInt64)
    case modeChanged(from: Mode, to: Mode)
    case note(source: String, message: String)
    case unknown(kind: String)

    /// The wire `kind`, including for payloads this build doesn't understand.
    var kind: String {
        switch self {
        case .projectAdded: "project_added"
        case .taskIngested: "task_ingested"
        case .taskStateChanged: "task_state_changed"
        case .taskGhStateChanged: "task_gh_state_changed"
        case .sessionStarted: "session_started"
        case .sessionCompleted: "session_completed"
        case .specCreated: "spec_created"
        case .specQueueStatusChanged: "spec_queue_status_changed"
        case .queueReordered: "queue_reordered"
        case .specQueueReordered: "spec_queue_reordered"
        case .buildRequested: "build_requested"
        case .buildStarted: "build_started"
        case .buildCompleted: "build_completed"
        case .pullRequestOpened: "pull_request_opened"
        case .modeChanged: "mode_changed"
        case .note: "note"
        case .unknown(let kind): kind
        }
    }

    /// The task this event is about, when it names one — the common case for
    /// "refetch just this row".
    var taskId: String? {
        switch self {
        case .taskIngested(let taskId, _),
            .taskStateChanged(let taskId, _, _),
            .taskGhStateChanged(let taskId, _),
            .sessionStarted(_, let taskId),
            .sessionCompleted(_, let taskId, _),
            .specCreated(_, let taskId, _):
            taskId
        default:
            nil
        }
    }

    private enum CodingKeys: String, CodingKey {
        case kind, projectId, taskId, sessionId, specId, buildId
        case from, to, ghState, status, taskIds, specIds, prNumber
        case source, message
    }

    init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let kind = try c.decode(String.self, forKey: .kind)
        switch kind {
        case "project_added":
            self = .projectAdded(projectId: try c.decode(String.self, forKey: .projectId))
        case "task_ingested":
            self = .taskIngested(
                taskId: try c.decode(String.self, forKey: .taskId),
                projectId: try c.decode(String.self, forKey: .projectId))
        case "task_state_changed":
            self = .taskStateChanged(
                taskId: try c.decode(String.self, forKey: .taskId),
                from: try c.decode(TaskState.self, forKey: .from),
                to: try c.decode(TaskState.self, forKey: .to))
        case "task_gh_state_changed":
            self = .taskGhStateChanged(
                taskId: try c.decode(String.self, forKey: .taskId),
                ghState: try c.decode(GhState.self, forKey: .ghState))
        case "session_started":
            self = .sessionStarted(
                sessionId: try c.decode(String.self, forKey: .sessionId),
                taskId: try c.decode(String.self, forKey: .taskId))
        case "session_completed":
            self = .sessionCompleted(
                sessionId: try c.decode(String.self, forKey: .sessionId),
                taskId: try c.decode(String.self, forKey: .taskId),
                status: try c.decode(SessionStatus.self, forKey: .status))
        case "spec_created":
            self = .specCreated(
                specId: try c.decode(String.self, forKey: .specId),
                taskId: try c.decode(String.self, forKey: .taskId),
                sessionId: try c.decode(String.self, forKey: .sessionId))
        case "spec_queue_status_changed":
            self = .specQueueStatusChanged(
                specId: try c.decode(String.self, forKey: .specId),
                from: try c.decodeIfPresent(SpecQueueStatus.self, forKey: .from),
                to: try c.decode(SpecQueueStatus.self, forKey: .to))
        case "queue_reordered":
            self = .queueReordered(taskIds: try c.decode([String].self, forKey: .taskIds))
        case "spec_queue_reordered":
            self = .specQueueReordered(specIds: try c.decode([String].self, forKey: .specIds))
        case "build_requested":
            self = .buildRequested(
                buildId: try c.decode(String.self, forKey: .buildId),
                specIds: try c.decode([String].self, forKey: .specIds))
        case "build_started":
            self = .buildStarted(buildId: try c.decode(String.self, forKey: .buildId))
        case "build_completed":
            self = .buildCompleted(
                buildId: try c.decode(String.self, forKey: .buildId),
                status: try c.decode(BuildStatus.self, forKey: .status))
        case "pull_request_opened":
            self = .pullRequestOpened(
                buildId: try c.decode(String.self, forKey: .buildId),
                prNumber: try c.decode(UInt64.self, forKey: .prNumber))
        case "mode_changed":
            self = .modeChanged(
                from: try c.decode(Mode.self, forKey: .from),
                to: try c.decode(Mode.self, forKey: .to))
        case "note":
            self = .note(
                source: try c.decode(String.self, forKey: .source),
                message: try c.decode(String.self, forKey: .message))
        default:
            self = .unknown(kind: kind)
        }
    }
}

/// The Activity feed's view of an event: `kind` plus whatever identifiers and
/// display strings the payload happens to carry, all optional.
///
/// Kept alongside the typed ``Event`` on purpose. The feed renders every kind
/// including ones this build predates, so it wants the loose bag; a client that
/// wants to act on an event wants ``EventPayload``. Both are pinned by the same
/// wire fixtures.
struct ActivityEvent: Decodable, Identifiable, Hashable {
    let seq: Int64
    let timestamp: Date
    let kind: String
    let taskId: String?
    let sessionId: String?
    let specId: String?
    let from: String?
    let to: String?
    let source: String?
    let message: String?
    let buildId: String?
    let status: String?
    let prNumber: Int64?

    var id: Int64 { seq }

    enum CodingKeys: String, CodingKey {
        case seq, timestamp, payload
    }
    enum PayloadKeys: String, CodingKey {
        case kind, taskId, sessionId, specId, from, to, source, message
        case buildId, status, prNumber
    }

    init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        seq = try c.decode(Int64.self, forKey: .seq)
        timestamp = try c.decode(Date.self, forKey: .timestamp)
        let p = try c.nestedContainer(keyedBy: PayloadKeys.self, forKey: .payload)
        kind = try p.decodeIfPresent(String.self, forKey: .kind) ?? "unknown"
        taskId = try? p.decodeIfPresent(String.self, forKey: .taskId)
        sessionId = try? p.decodeIfPresent(String.self, forKey: .sessionId)
        specId = try? p.decodeIfPresent(String.self, forKey: .specId)
        from = try? p.decodeIfPresent(String.self, forKey: .from)
        to = try? p.decodeIfPresent(String.self, forKey: .to)
        source = try? p.decodeIfPresent(String.self, forKey: .source)
        message = try? p.decodeIfPresent(String.self, forKey: .message)
        buildId = try? p.decodeIfPresent(String.self, forKey: .buildId)
        status = try? p.decodeIfPresent(String.self, forKey: .status)
        prNumber = try? p.decodeIfPresent(Int64.self, forKey: .prNumber)
    }
}

/// One serial Builder run over a batch of approved specs. `prNumber` is an
/// identifier — the PR's live state is GitHub's; link out for it.
struct BuildItem: Decodable, Identifiable, Hashable {
    let id: String
    let projectId: String
    let branch: String
    let baseBranch: String
    let baseSha: String?
    let headSha: String?
    let prNumber: UInt64?
    let status: BuildStatus
    let summary: String?
    let filesTouched: [String]
    let exitReason: String?
    let createdAt: Date
    let startedAt: Date?
    let completedAt: Date?
}

enum BuildStatus: WireEnum {
    case queued, running, succeeded, failed
    case unknown(String)

    init(wire: String) {
        switch wire {
        case "queued": self = .queued
        case "running": self = .running
        case "succeeded": self = .succeeded
        case "failed": self = .failed
        default: self = .unknown(wire)
        }
    }

    var wire: String {
        switch self {
        case .queued: "queued"
        case .running: "running"
        case .succeeded: "succeeded"
        case .failed: "failed"
        case .unknown(let raw): raw
        }
    }
}

/// One turn in the orchestrator conversation (the Chat pane).
struct ChatMessage: Decodable, Identifiable, Hashable {
    let seq: Int64
    let role: ChatRole
    let content: String
    let createdAt: Date

    var id: Int64 { seq }
}

/// One frame of `/orchestrator/stream` — the live view of an in-flight tick.
/// Loose by design: unknown kinds are skipped, and nothing here is durable
/// (the finished message arrives via `/orchestrator/messages`).
struct OrchestratorFeedFrame: Decodable, Sendable {
    let kind: String
    /// Present when kind == "delta": a chunk of assistant text.
    let text: String?
    /// Present when kind == "tool": a one-line tool-call label.
    let label: String?
}

enum ChatRole: WireEnum {
    case user, assistant
    case unknown(String)

    init(wire: String) {
        switch wire {
        case "user": self = .user
        case "assistant": self = .assistant
        default: self = .unknown(wire)
        }
    }

    var wire: String {
        switch self {
        case .user: "user"
        case .assistant: "assistant"
        case .unknown(let raw): raw
        }
    }
}
