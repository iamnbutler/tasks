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

enum TaskState: WireEnum {
    case new, scouting, specReady, queued, done, rejected
    case unknown(String)

    init(wire: String) {
        switch wire {
        case "new": self = .new
        case "scouting": self = .scouting
        case "spec_ready": self = .specReady
        case "queued": self = .queued
        case "done": self = .done
        case "rejected": self = .rejected
        default: self = .unknown(wire)
        }
    }

    var wire: String {
        switch self {
        case .new: "new"
        case .scouting: "scouting"
        case .specReady: "spec_ready"
        case .queued: "queued"
        case .done: "done"
        case .rejected: "rejected"
        case .unknown(let raw): raw
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
