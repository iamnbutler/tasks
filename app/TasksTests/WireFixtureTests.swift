import Foundation
import Testing

@testable import Tasks

/// Decodes every golden fixture the Rust API generates
/// (`crates/tasks/tests/wire_fixtures.rs`) through the app's production
/// decoder.
///
/// This is the client half of the contract. A field rename or a new enum
/// variant on the server fails `cargo test` first — the fixture no longer
/// matches what the server serializes — and then fails here until the Swift
/// models catch up. Neither half is much use without the other.
///
/// Note what this deliberately is *not*: runtime parsing stays lenient. Unknown
/// enum values still decode to `.unknown` in the app. These tests assert the
/// values we know about today are all known today; they don't make the client
/// brittle against a server that's ahead of it.
@Suite("Wire fixtures")
struct WireFixtureTests {

    /// Every fixture this suite decodes. `everyFixtureOnDiskIsCovered` holds
    /// this to the directory in both directions, so a fixture added on the Rust
    /// side can't sit here silently untested.
    static let covered: Set<String> = Set([
        "project",
        "task", "task_minimal", "task_list",
        "session", "session_running",
        "transcript_line",
        "spec",
        "spec_queue_item", "spec_queue_item_pending",
        "build", "build_queued", "build_detail",
        "mode_response",
        "error",
        "enums",
        "timestamps",
    ]).union(eventFixtures)

    /// One per `EventPayload` variant, plus the `from: null` shape.
    static let eventFixtures: Set<String> = [
        "event_project_added",
        "event_task_ingested",
        "event_task_state_changed",
        "event_task_gh_state_changed",
        "event_session_started",
        "event_session_completed",
        "event_spec_created",
        "event_spec_queue_status_changed",
        "event_spec_queue_status_changed_initial",
        "event_queue_reordered",
        "event_spec_queue_reordered",
        "event_build_requested",
        "event_build_started",
        "event_build_completed",
        "event_pull_request_opened",
        "event_mode_changed",
        "event_note",
    ]

    // --- entities ---

    @Test func project() throws {
        let project = try Fixtures.decode(Tasks.Project.self, "project")
        #expect(project.id == "proj_0f4b1c2d3e4f5a6b7c8d9e0f1a2b3c4d")
        #expect(project.repoOwner == "iamnbutler")
        #expect(project.repoName == "tasks")
        #expect(project.slug == "iamnbutler/tasks")
        try expect(project.addedAt, is: "2026-08-01T09:15:00Z")
    }

    @Test func task() throws {
        let task = try Fixtures.decode(TaskItem.self, "task")
        #expect(task.id == "task_1a2b3c4d5e6f708192a3b4c5d6e7f809")
        #expect(task.projectId == "proj_0f4b1c2d3e4f5a6b7c8d9e0f1a2b3c4d")
        #expect(task.ghIssueNumber == 763)
        #expect(task.labels == ["enhancement", "testing"])
        #expect(task.ghState == .open)
        #expect(task.state == .inReview)
        #expect(task.priority == 3)
        #expect(task.manualRank == 1)
        #expect(task.dispatchAttempts == 2)
        try expect(task.updatedAt, is: "2026-08-09T18:30:00.123Z")
    }

    /// The null/empty half of the shape. Without this, a field going optional
    /// on the server would only be noticed by whoever hit the empty case first.
    @Test func taskMinimal() throws {
        let task = try Fixtures.decode(TaskItem.self, "task_minimal")
        #expect(task.body == "")
        #expect(task.labels.isEmpty)
        #expect(task.manualRank == nil)
        #expect(task.dispatchAttempts == 0)
        #expect(task.ghState == .closed)
        #expect(task.state == .backlog)
    }

    /// `dispatch_attempts` is typed `Int?` "absent on older servers" — the
    /// fixture shows the server always sends it. Left optional deliberately;
    /// this pins the fact so tightening it stays a decision, not a discovery.
    @Test func dispatchAttemptsIsAlwaysSent() throws {
        for name in ["task", "task_minimal"] {
            #expect(try Fixtures.json(name)["dispatch_attempts"] != nil)
        }
    }

    @Test func taskList() throws {
        let tasks = try Fixtures.decode([TaskItem].self, "task_list")
        #expect(tasks.count == 2)
        #expect(tasks.map(\.state) == [.inReview, .backlog])
    }

    @Test func session() throws {
        let session = try Fixtures.decode(ScoutSession.self, "session")
        #expect(session.id == "sess_3c4d5e6f708192a3b4c5d6e7f8091a2b")
        #expect(session.vmId == "vm_7c1f9a4e")
        #expect(session.status == .scoutSucceeded)
        #expect(session.exitReason == "spec reported")
        #expect(session.usage?.inputTokens == 184_320)
        #expect(session.usage?.numTurns == 87)
        #expect(session.usage?.totalCostUsd == 4.2175)
        try expect(#require(session.completedAt), is: "2026-08-09T17:23:41.456Z")
    }

    @Test func sessionRunning() throws {
        let session = try Fixtures.decode(ScoutSession.self, "session_running")
        #expect(session.status == .running)
        #expect(session.vmId == nil)
        #expect(session.completedAt == nil)
        #expect(session.exitReason == nil)
        #expect(session.usage == nil)
    }

    @Test func transcriptLine() throws {
        let line = try Fixtures.decode(Tasks.TranscriptLine.self, "transcript_line")
        #expect(line.sessionId == "sess_3c4d5e6f708192a3b4c5d6e7f8091a2b")
        #expect(line.seq == 42)
        #expect(line.id == 42)
        #expect(line.stream == .stdout)
        #expect(line.line.hasPrefix(#"{"type":"assistant""#))
    }

    /// The server serves `content`; the Swift model also accepts
    /// `spec_markdown`, which docs/clients.md used to claim. The fixture is what
    /// settles it — `content`, and no `agent_exit_code` at all.
    @Test func spec() throws {
        let spec = try Fixtures.decode(Tasks.Spec.self, "spec")
        #expect(spec.id == "spec_4d5e6f708192a3b4c5d6e7f8091a2b3c")
        #expect(spec.sessionId == "sess_3c4d5e6f708192a3b4c5d6e7f8091a2b")
        #expect(spec.complexity == .medium)
        #expect(spec.content.hasPrefix("## Spec: Golden JSON wire fixtures"))
        #expect(spec.filesTouched.count == 2)
        #expect(spec.agentExitCode == nil)

        let raw = try Fixtures.json("spec")
        #expect(raw["content"] != nil)
        #expect(raw["spec_markdown"] == nil)
        #expect(raw["agent_exit_code"] == nil)
    }

    /// `SpecQueueItem` is `#[serde(flatten)]` on the Rust side, and flatten
    /// keeps declaration order — so the entry's fields sit at the top level with
    /// `task_id` last, not inside a nested `entry` object.
    @Test func specQueueItemIsFlat() throws {
        let item = try Fixtures.decode(Tasks.SpecQueueItem.self, "spec_queue_item")
        #expect(item.specId == "spec_4d5e6f708192a3b4c5d6e7f8091a2b3c")
        #expect(item.id == item.specId)
        #expect(item.status == .approved)
        #expect(item.rank == 1)
        #expect(item.feedback?.hasPrefix("Good.") == true)
        #expect(item.blockingDependencies == ["task_2b3c4d5e6f708192a3b4c5d6e7f8091a"])
        #expect(item.taskId == "task_1a2b3c4d5e6f708192a3b4c5d6e7f809")
        try expect(#require(item.approvedAt), is: "2026-08-09T18:00:00Z")

        let raw = try Fixtures.json("spec_queue_item")
        #expect(raw["entry"] == nil, "the queue item is flat, not nested under `entry`")
        #expect(raw["spec_id"] != nil)
    }

    @Test func specQueueItemPending() throws {
        let item = try Fixtures.decode(Tasks.SpecQueueItem.self, "spec_queue_item_pending")
        #expect(item.status == .pendingReview)
        #expect(item.rank == nil)
        #expect(item.approvedAt == nil)
        #expect(item.feedback == nil)
        #expect(item.blockingDependencies.isEmpty)
    }

    @Test func build() throws {
        let build = try Fixtures.decode(Tasks.Build.self, "build")
        #expect(build.id == "build_6f708192a3b4c5d6e7f8091a2b3c4d5e")
        #expect(build.branch == "build/build_6f708192")
        #expect(build.baseBranch == "main")
        #expect(build.baseSha == "a3b1d0c9e8f7a6b5c4d3e2f1a0b9c8d7e6f5a4b3")
        #expect(build.headSha == "b4c2e1d0f9a8b7c6d5e4f3a2b1c0d9e8f7a6b5c4")
        #expect(build.prNumber == 781)
        #expect(build.status == .succeeded)
        #expect(build.summary?.isEmpty == false)
        #expect(build.exitReason == nil)
        try expect(#require(build.completedAt), is: "2026-08-09T19:41:12.789Z")
    }

    @Test func buildQueued() throws {
        let build = try Fixtures.decode(Tasks.Build.self, "build_queued")
        #expect(build.status == .queued)
        #expect(build.vmId == nil)
        #expect(build.baseSha == nil)
        #expect(build.headSha == nil)
        #expect(build.prNumber == nil)
        #expect(build.summary == nil)
        #expect(build.filesTouched.isEmpty)
        #expect(build.startedAt == nil)
        #expect(build.completedAt == nil)
    }

    /// `GET /builds/{id}` flattens the batch alongside the build.
    @Test func buildDetail() throws {
        let detail = try Fixtures.decode(Tasks.BuildDetail.self, "build_detail")
        #expect(detail.id == detail.build.id)
        #expect(detail.build.status == .succeeded)
        #expect(
            detail.specIds == [
                "spec_4d5e6f708192a3b4c5d6e7f8091a2b3c",
                "spec_5e6f708192a3b4c5d6e7f8091a2b3c4d",
            ])
        #expect(try Fixtures.json("build_detail")["build"] == nil)
    }

    @Test func modeResponse() throws {
        #expect(try Fixtures.decode(Tasks.ModeResponse.self, "mode_response").mode == .play)
    }

    /// The body every non-2xx carries. `TasksClient.checkOK` reads exactly this
    /// to build `APIError.message`, so a rename would silently degrade every
    /// error in the UI to "HTTP 404".
    @Test func errorBody() throws {
        let error = try Fixtures.decode(TasksClient.ServerError.self, "error")
        #expect(error.error == "task task_1a2b3c4d5e6f708192a3b4c5d6e7f809")
    }

    // --- enums ---

    /// Every snake_case value the server can emit maps to a real case, not
    /// `.unknown`. `enums.json` is generated from exhaustive matches on the Rust
    /// side, so this is the check that a new variant reaches the client.
    @Test func everyServerEnumValueIsKnown() throws {
        let known: [String: (String) -> Bool] = [
            "gh_state": { if case .unknown = GhState(wire: $0) { false } else { true } },
            "task_state": { if case .unknown = TaskState(wire: $0) { false } else { true } },
            "session_status": { if case .unknown = SessionStatus(wire: $0) { false } else { true } },
            "transcript_stream": {
                if case .unknown = TranscriptStream(wire: $0) { false } else { true }
            },
            "spec_queue_status": {
                if case .unknown = SpecQueueStatus(wire: $0) { false } else { true }
            },
            "build_status": { if case .unknown = BuildStatus(wire: $0) { false } else { true } },
            "complexity": { if case .unknown = Complexity(wire: $0) { false } else { true } },
            "mode": { if case .unknown = Mode(wire: $0) { false } else { true } },
        ]

        let inventory = try Fixtures.json("enums")
        #expect(
            Set(inventory.keys) == Set(known.keys),
            """
            enums.json lists \(Set(inventory.keys).sorted()) but this test knows \
            \(Set(known.keys).sorted()) — a wire enum was added or removed on the server.
            """)

        for (name, values) in inventory {
            let values = try #require(values as? [String], "\(name) is not an array of strings")
            let check = try #require(known[name])
            for value in values {
                #expect(check(value), "\(name) value \"\(value)\" decodes as .unknown")
            }
        }
    }

    // --- timestamps ---

    /// chrono's fractional-second width is value-dependent — 0, 3, 6 or 9
    /// digits, nothing between — so the client's parser has to take all four.
    ///
    /// It normalizes by *truncating* to 3: `.123456789Z` decodes as `.123`.
    /// Sub-millisecond precision is dropped, not rounded. Pinned here so that if
    /// it ever matters, this is where it surfaces.
    @Test func allFourFractionalWidthsParse() throws {
        let timestamps = try Fixtures.decode([String: Date].self, "timestamps")
        let expected: [String: String] = [
            "whole_seconds": "2026-08-09T12:00:00Z",
            "milliseconds": "2026-08-09T12:00:00.123Z",
            "microseconds": "2026-08-09T12:00:00.123Z",  // truncated from .123456
            "nanoseconds": "2026-08-09T12:00:00.123Z",  // truncated from .123456789
        ]
        #expect(Set(timestamps.keys) == Set(expected.keys))
        for (key, iso) in expected {
            try expect(#require(timestamps[key], "missing \(key)"), is: iso)
        }
    }

    // --- events ---

    /// Every event fixture decodes to a typed payload — none falls through to
    /// `.unknown`, and the `kind` the model reports round-trips to the wire.
    @Test func everyEventVariantDecodes() throws {
        var seen: Set<String> = []
        for name in Self.eventFixtures.sorted() {
            let event = try Fixtures.decode(Tasks.Event.self, name)
            #expect(event.seq > 0, "\(name): seq should be present")

            if case .unknown(let kind) = event.payload {
                Issue.record("\(name) decoded as .unknown(kind: \(kind))")
                continue
            }
            let raw = try #require(try Fixtures.json(name)["payload"] as? [String: Any])
            #expect(event.payload.kind == raw["kind"] as? String)
            seen.insert(event.payload.kind)
        }
        // 17 files, 16 kinds: spec_queue_status_changed has two shapes.
        #expect(seen.count == 16, "expected all 16 payload kinds, saw \(seen.sorted())")
    }

    /// The payloads carrying detail, spot-checked past the `kind` tag.
    @Test func eventPayloadsCarryTheirFields() throws {
        #expect(
            try Fixtures.decode(Tasks.Event.self, "event_task_state_changed").payload
                == .taskStateChanged(
                    taskId: "task_1a2b3c4d5e6f708192a3b4c5d6e7f809",
                    from: .queued, to: .scouting))

        #expect(
            try Fixtures.decode(Tasks.Event.self, "event_build_completed").payload
                == .buildCompleted(
                    buildId: "build_6f708192a3b4c5d6e7f8091a2b3c4d5e", status: .succeeded))

        #expect(
            try Fixtures.decode(Tasks.Event.self, "event_pull_request_opened").payload
                == .pullRequestOpened(
                    buildId: "build_6f708192a3b4c5d6e7f8091a2b3c4d5e", prNumber: 781))

        #expect(
            try Fixtures.decode(Tasks.Event.self, "event_mode_changed").payload
                == .modeChanged(from: .pause, to: .play))

        #expect(
            try Fixtures.decode(Tasks.Event.self, "event_queue_reordered").payload
                == .queueReordered(taskIds: [
                    "task_1a2b3c4d5e6f708192a3b4c5d6e7f809",
                    "task_2b3c4d5e6f708192a3b4c5d6e7f8091a",
                ]))

        #expect(
            try Fixtures.decode(Tasks.Event.self, "event_note").payload
                == .note(
                    source: "scout-dispatcher",
                    message: "task task_1a2b3c4d rejected after 3 failed dispatches"))
    }

    /// A spec's first appearance in the queue has no prior status. Both shapes
    /// of the same `kind` have to decode.
    @Test func specQueueStatusChangedHandlesTheInitialTransition() throws {
        #expect(
            try Fixtures.decode(Tasks.Event.self, "event_spec_queue_status_changed").payload
                == .specQueueStatusChanged(
                    specId: "spec_4d5e6f708192a3b4c5d6e7f8091a2b3c",
                    from: .pendingReview, to: .approved))

        #expect(
            try Fixtures.decode(Tasks.Event.self, "event_spec_queue_status_changed_initial").payload
                == .specQueueStatusChanged(
                    specId: "spec_4d5e6f708192a3b4c5d6e7f8091a2b3c",
                    from: nil, to: .pendingReview))
    }

    /// An unrecognized kind must not fail the decode — the Activity feed has to
    /// keep rendering against a server that's ahead of this build.
    @Test func unknownEventKindsSurviveDecoding() throws {
        let json = Data(
            #"{"seq":1,"timestamp":"2026-08-09T17:00:00Z","payload":{"kind":"warp_drive_engaged"}}"#
                .utf8)
        let event = try TasksClient.makeDecoder().decode(Tasks.Event.self, from: json)
        #expect(event.payload == .unknown(kind: "warp_drive_engaged"))
        #expect(event.payload.kind == "warp_drive_engaged")
    }

    /// The Activity feed reads events as a loose bag of strings. The fixtures
    /// prove the keys it reaches for are the ones actually on the wire.
    @Test func activityFeedReadsTheFieldsItExpects() throws {
        let stateChange = try Fixtures.decode(ActivityEvent.self, "event_task_state_changed")
        #expect(stateChange.kind == "task_state_changed")
        #expect(stateChange.taskId == "task_1a2b3c4d5e6f708192a3b4c5d6e7f809")
        #expect(stateChange.from == "queued")
        #expect(stateChange.to == "scouting")

        let note = try Fixtures.decode(ActivityEvent.self, "event_note")
        #expect(note.source == "scout-dispatcher")
        #expect(note.message?.isEmpty == false)
    }

    // --- coverage ---

    /// Both directions: a fixture the Rust side added but nothing here reads is
    /// a shape shipping untested, and a name here with no file is a rename this
    /// suite slept through.
    @Test func everyFixtureOnDiskIsCovered() throws {
        let onDisk = Set(try Fixtures.names())
        #expect(!onDisk.isEmpty, "no fixtures were enumerated at \(Fixtures.directory.path)")

        #expect(
            onDisk.subtracting(Self.covered).isEmpty,
            """
            new fixtures with no Swift coverage: \(onDisk.subtracting(Self.covered).sorted()).
            Add a test above and list the name in `covered`.
            """)
        #expect(
            Self.covered.subtracting(onDisk).isEmpty,
            """
            fixtures this suite expects but that are not on disk: \
            \(Self.covered.subtracting(onDisk).sorted()).
            Regenerate with `UPDATE_FIXTURES=1 cargo test -p tasks --test wire_fixtures`.
            """)
    }

    // --- helpers ---

    /// `Date` is a `Double`, so it can't represent a nanosecond offset at this
    /// epoch anyway; half a millisecond is well under the precision the client
    /// keeps and well over the float noise.
    private func expect(
        _ actual: Date, is iso: String,
        sourceLocation: SourceLocation = #_sourceLocation
    ) throws {
        let expected =
            iso.contains(".")
            ? try Date(iso, strategy: Date.ISO8601FormatStyle(includingFractionalSeconds: true))
            : try Date(iso, strategy: .iso8601)
        #expect(
            abs(actual.timeIntervalSince(expected)) < 0.0005,
            "expected \(iso), got \(actual)",
            sourceLocation: sourceLocation)
    }
}
