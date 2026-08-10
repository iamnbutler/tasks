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
    case pendingReview, approved, needsRevision, blocked, rejected
    case unknown(String)

    init(wire: String) {
        switch wire {
        case "pending_review": self = .pendingReview
        case "approved": self = .approved
        case "needs_revision": self = .needsRevision
        case "blocked": self = .blocked
        case "rejected": self = .rejected
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

/// One event-log entry, decoded leniently for the Activity feed: `kind` plus
/// whatever identifiers the payload carries. Unknown kinds render as-is.
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
