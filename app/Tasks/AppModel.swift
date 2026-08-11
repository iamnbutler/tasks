import AppKit
import Foundation
import Observation

/// All server state the UI renders, refreshed wholesale. The lists are small
/// and the server is loopback, so any event on the SSE stream just triggers a
/// full refetch — no incremental patching to drift.
@MainActor
@Observable
final class AppModel {
    var projects: [Project] = []
    var tasks: [TaskItem] = []
    var sessions: [ScoutSession] = []
    var specs: [Spec] = []
    var builds: [BuildItem] = []
    var chat: [ChatMessage] = []
    var specQueue: [SpecQueueItem] = []
    var mode: Mode?
    /// The three Home briefing slots, in the server's display order.
    var briefings: [BriefingStatus] = []
    /// The whole event log, held client-side, oldest first. Backfilled once
    /// by paging `/events?since=1` and extended with one delta request per
    /// refresh. Held rather than refetched because `GET /events` without
    /// `since` returns the newest N — Activity and the build→task joins
    /// (`servedTaskIds`) need the log, not a page of it.
    var eventLog: [ActivityEvent] = []

    /// What the Activity feed renders: the newest 200, newest first. A slice,
    /// not the whole log — the unread badge below is counted over what the
    /// feed will actually show.
    var events: [ActivityEvent] {
        Array(eventLog.suffix(200).reversed())
    }

    /// Highest event seq the user has seen in Activity; drives the unread
    /// divider and the sidebar badge. Persisted so "while you were away"
    /// survives relaunch.
    var lastSeenSeq: Int64 = UserDefaults.standard.object(forKey: "lastSeenSeq") as? Int64 ?? 0

    var unreadCount: Int {
        events.filter { $0.seq > lastSeenSeq }.count
    }

    func markActivityRead() {
        if let newest = events.first?.seq, newest > lastSeenSeq {
            lastSeenSeq = newest
            UserDefaults.standard.set(newest, forKey: "lastSeenSeq")
        }
    }

    var connectionError: String?
    var lastRefreshed: Date?

    /// Live view of the in-flight orchestrator tick, from
    /// `/orchestrator/stream`. `liveReply` is the assistant text generated so
    /// far (reset on each tool call — pre-tool text is working narration, and
    /// the segment after the last tool call is the reply that persists);
    /// `liveActivity` is the latest tool-call label. Both are ephemeral and
    /// cleared once the durable message lands in `chat`.
    var liveReply = ""
    var liveActivity: String?

    // Non-private: session-detail views borrow it for transcript tailing.
    let client = TasksClient()
    private var started = false

    func task(_ id: String) -> TaskItem? {
        tasks.first { $0.id == id }
    }

    func spec(_ id: String) -> Spec? {
        specs.first { $0.id == id }
    }

    func session(_ id: String) -> ScoutSession? {
        sessions.first { $0.id == id }
    }

    func queueEntry(forSpec specId: String) -> SpecQueueItem? {
        specQueue.first { $0.specId == specId }
    }

    /// One verdict = one POST. Applies the returned entry optimistically;
    /// the event-driven refresh reconciles everything else (task state
    /// changes on approve/needs-revision). Throws so the review form can
    /// show the server's real error message.
    func review(specId: String, verdict: ReviewVerdict, feedback: String?) async throws {
        let updated = try await client.reviewSpec(specId, verdict: verdict, feedback: feedback)
        if let index = specQueue.firstIndex(where: { $0.specId == specId }) {
            specQueue[index] = updated
        } else {
            specQueue.append(updated)
        }
        await refresh()
    }

    /// One intent = one POST; the response is the updated task, and the
    /// event-driven refresh reconciles the rest. Errors surface on the banner.
    func queueTask(_ id: String) async {
        await perform { try await self.client.queueTask(id) }
    }

    func dequeueTask(_ id: String) async {
        await perform { try await self.client.dequeueTask(id) }
    }

    func scoutNow(_ id: String) async {
        await perform { try await self.client.scoutNow(id) }
    }

    /// The at-most-one build the serial loop is running.
    var runningBuild: BuildItem? {
        builds.first { $0.status == .running }
    }

    /// Send a chat turn to the orchestrator. Appends optimistically; the
    /// reply arrives via the event-driven refresh.
    func sendChat(_ content: String) async {
        do {
            let sent = try await client.sendOrchestratorMessage(content)
            if !chat.contains(where: { $0.seq == sent.seq }) {
                chat.append(sent)
            }
        } catch {
            connectionError = error.localizedDescription
        }
    }

    /// Resume the orchestrator's Claude Code session in Terminal. The wrapper
    /// script checks the session out (suspending headless ticks — CC sessions
    /// have no file locking), renews the checkout every minute, and releases
    /// on exit so nudges queued meanwhile get answered.
    func openOrchestratorInTerminal() async {
        do {
            let info = try await client.orchestratorSession()
            guard let sessionId = info.ccSessionId else {
                connectionError = "No orchestrator session yet — say something in Chat first."
                return
            }
            let base = client.baseURL.absoluteString
            let workdir = info.workdir ?? FileManager.default.homeDirectoryForCurrentUser.path
            let script = """
                #!/bin/zsh -li
                # Interactive checkout of the Tasks orchestrator session.
                BASE=\(shellQuoted(base))
                trap 'kill $HEARTBEAT 2>/dev/null; curl -s -X POST "$BASE/orchestrator/session/release" > /dev/null' EXIT
                curl -s -X POST "$BASE/orchestrator/session/checkout" > /dev/null
                ( while :; do sleep 60; curl -s -X POST "$BASE/orchestrator/session/checkout" > /dev/null; done ) &
                HEARTBEAT=$!
                cd \(shellQuoted(workdir))
                claude --resume \(shellQuoted(sessionId))
                """
            let dir = FileManager.default.temporaryDirectory
            let file = dir.appending(path: "tasks-orchestrator.command")
            try script.write(to: file, atomically: true, encoding: .utf8)
            try FileManager.default.setAttributes(
                [.posixPermissions: 0o755], ofItemAtPath: file.path)
            NSWorkspace.shared.open(file)
        } catch {
            connectionError = error.localizedDescription
        }
    }

    private func shellQuoted(_ value: String) -> String {
        "'" + value.replacingOccurrences(of: "'", with: "'\\''") + "'"
    }

    /// Queue a one-spec Builder run (the API takes a batch; the UI's unit is
    /// a task's approved spec). 202 — the queue view reflects progress.
    func buildNow(specId: String) async {
        do {
            _ = try await client.requestBuild(specIds: [specId])
            await refresh()
        } catch {
            connectionError = error.localizedDescription
        }
    }

    func setMode(_ mode: Mode) async {
        do {
            self.mode = try await client.setMode(mode)
        } catch {
            connectionError = error.localizedDescription
        }
    }

    private func perform(_ op: @escaping () async throws -> TaskItem) async {
        do {
            let updated = try await op()
            if let index = tasks.firstIndex(where: { $0.id == updated.id }) {
                tasks[index] = updated
            }
            await refresh()
        } catch {
            connectionError = error.localizedDescription
        }
    }

    /// Optimistic drag-reorder of the "Up next" group. Only `queued` tasks
    /// carry meaning in the ranked order now, so the reorder POST sends
    /// exactly their ids — everything else goes unranked, which is correct.
    /// The response is the full filtered projection (#770) and replaces the
    /// list directly.
    func moveQueued(from source: IndexSet, to destination: Int) {
        var upNext = tasks.filter { $0.state == .queued }
        upNext.move(fromOffsets: source, toOffset: destination)
        let order = upNext.map(\.id)
        // Optimistic: re-sort the local list to match the new ranks.
        let rank = Dictionary(uniqueKeysWithValues: order.enumerated().map { ($1, $0) })
        tasks.sort { a, b in
            switch (rank[a.id], rank[b.id]) {
            case (let x?, let y?): return x < y
            case (.some, nil): return true
            case (nil, .some): return false
            case (nil, nil): return false
            }
        }
        Task {
            do {
                tasks = try await client.reorderQueue(order)
            } catch {
                connectionError = error.localizedDescription
                await refresh()
            }
        }
    }

    /// The clients.md loop: open the SSE stream first, snapshot once it's
    /// established (`.connected`), refetch on every event, reconnect forever.
    /// Full re-snapshot per event stands in for per-entity refetch + seq
    /// tracking — always correct, and cheap at loopback list sizes.
    func start() async {
        guard !started else { return }
        started = true
        Task { await orchestratorFeedLoop() }
        while !Task.isCancelled {
            do {
                for try await _ in client.eventStream() {
                    // .connected and .event both mean the same thing here:
                    // our lists may be stale — refetch.
                    await refresh()
                }
            } catch is CancellationError {
                return
            } catch {
                connectionError = error.localizedDescription
            }
            try? await Task.sleep(for: .seconds(3))
        }
    }

    /// Tail `/orchestrator/stream` forever, mirroring the event loop's
    /// reconnect policy. A dropped connection just clears the live view —
    /// the durable reply still arrives through the ordinary refresh.
    private func orchestratorFeedLoop() async {
        while !Task.isCancelled {
            do {
                for try await frame in client.orchestratorFeed() {
                    switch frame.kind {
                    case "delta":
                        liveReply += frame.text ?? ""
                    case "tool":
                        liveActivity = frame.label
                        liveReply = ""
                    case "done":
                        // Keep the reply text visible; the refresh replaces
                        // it with the persisted message (no flash).
                        liveActivity = nil
                    default:
                        break
                    }
                }
            } catch is CancellationError {
                return
            } catch {
                // Connection errors surface via the event loop's banner.
            }
            liveReply = ""
            liveActivity = nil
            try? await Task.sleep(for: .seconds(3))
        }
    }

    /// Every fetch stands alone: one failing endpoint surfaces on the banner
    /// but must not blank the six that succeeded.
    func refresh() async {
        let c = client
        async let projects = Self.attempt { try await c.projects() }
        async let tasks = Self.attempt { try await c.tasks() }
        async let sessions = Self.attempt { try await c.sessions() }
        async let specs = Self.attempt { try await c.specs() }
        async let builds = Self.attempt { try await c.builds() }
        async let chat = Self.attempt { try await c.orchestratorMessages() }
        async let specQueue = Self.attempt { try await c.specQueue() }
        async let mode = Self.attempt { try await c.mode() }
        // Reading briefings IS the demand signal: a stale section starts
        // regenerating server-side, and its `briefing_updated` event lands
        // us back here with the fresh copy.
        async let briefings = Self.attempt { try await c.briefings() }

        var firstError: String?
        func apply<T>(_ result: Result<T, any Error>?, _ assign: (T) -> Void) {
            switch result {
            case .success(let value): assign(value)
            case .failure(let error):
                if firstError == nil { firstError = error.localizedDescription }
            case nil: break
            }
        }
        apply(await projects) { self.projects = $0 }
        apply(await tasks) { self.tasks = $0 }
        apply(await sessions) { self.sessions = $0 }
        apply(await specs) { self.specs = $0 }
        apply(await builds) { self.builds = $0 }
        apply(await chat) {
            self.chat = $0
            // The durable reply has landed — drop the live-tick preview.
            if $0.last?.role == .assistant {
                self.liveReply = ""
                self.liveActivity = nil
            }
        }
        apply(await specQueue) { self.specQueue = $0 }
        apply(await mode) { self.mode = $0 }
        apply(await briefings) { self.briefings = $0 }

        connectionError = firstError
        // Delta-extend the held log (backfilling on the first pass), then
        // name any newly on-screen shipped work.
        await extendEventLog()
        await resolveRetiredTitles()
        lastRefreshed = Date()
    }

    // MARK: Event log

    /// Backfill (first call) then extend the held log. Pages at 500; `since`
    /// is inclusive so the loop asks for `high_water + 1` and ALSO filters
    /// the page on `> high_water` — two interleaved refreshes must not
    /// append the same event twice.
    func extendEventLog() async {
        do {
            while true {
                let since = (eventLog.last?.seq ?? 0) + 1
                let page = try await client.events(since: since, limit: 500)
                // Re-read the tail AFTER the await: a second refresh can
                // interleave on the main actor, and filtering against the
                // pre-await watermark would append its events twice.
                let tail = eventLog.last?.seq ?? 0
                let fresh = page.filter { $0.seq > tail }.sorted { $0.seq < $1.seq }
                eventLog.append(contentsOf: fresh)
                if page.count < 500 {
                    return
                }
            }
        } catch is CancellationError {
        } catch {
            if connectionError == nil { connectionError = error.localizedDescription }
        }
    }

    // MARK: Home

    /// One briefing slot by its wire section name; nil until the first
    /// `/briefings` fetch lands.
    func briefing(_ section: String) -> BriefingStatus? {
        briefings.first { $0.section == section }
    }

    var runningSessions: [ScoutSession] {
        sessions.filter { $0.status == .running }.sorted { $0.startedAt < $1.startedAt }
    }

    var specsAwaitingReview: [SpecQueueItem] {
        specQueue.filter { $0.status == .pendingReview }
    }

    /// When a pending spec landed — a join through `/specs`; nil (rendered as
    /// nothing, not a fabricated age) when the join misses.
    func waitingSince(specId: String) -> Date? {
        spec(specId)?.createdAt
    }

    var failedBuilds: [BuildItem] {
        builds.filter { $0.status == .failed }
            .sorted { $0.finishedOrCreatedAt > $1.finishedOrCreatedAt }
    }

    /// The tasks a build serves: its `build_requested` event names the specs
    /// (the listing projection doesn't), and specs name their tasks.
    func servedTaskIds(for build: BuildItem) -> [String] {
        guard
            let requested = eventLog.last(where: {
                $0.kind == "build_requested" && $0.buildId == build.id
            }),
            let specIds = requested.specIds
        else { return [] }
        return specIds.compactMap { spec($0)?.taskId }
    }

    /// Titles of retired tasks, resolved on demand via `GET /tasks/{id}` and
    /// cached forever — a retired task's title doesn't change.
    private var retiredTitles: [String: String] = [:]
    private var retiredTitleFetchesInFlight: Set<String> = []

    /// `#N title` for a task id, whether it's in the working set or retired.
    /// A miss returns nil now and triggers a bounded background resolve.
    func title(forTask id: String) -> String? {
        if let live = task(id) {
            return "#\(live.ghIssueNumber) \(live.title)"
        }
        return retiredTitles[id]
    }

    /// One line naming a build's work, falling back to its branch when the
    /// joins miss (never a dead `build/<uuid>` row if we can help it).
    func label(for build: BuildItem) -> String {
        let titles = servedTaskIds(for: build).compactMap { title(forTask: $0) }
        return titles.isEmpty ? build.branch : titles.joined(separator: " · ")
    }

    /// Fill `retiredTitles` for the builds Home still names mechanically
    /// (the running build, recent failures) — a retired task is exactly what
    /// `GET /tasks` reconciles away, and without this those rows degrade to
    /// `build/<uuid>`. Bounded by what's on screen; the steady state is zero
    /// requests.
    func resolveRetiredTitles() async {
        var onScreen = Array(failedBuilds.prefix(5))
        if let running = runningBuild {
            onScreen.append(running)
        }
        var missing: Set<String> = []
        for build in onScreen {
            for taskId in servedTaskIds(for: build)
            where task(taskId) == nil
                && retiredTitles[taskId] == nil
                && !retiredTitleFetchesInFlight.contains(taskId)
            {
                missing.insert(taskId)
            }
        }
        guard !missing.isEmpty else { return }
        retiredTitleFetchesInFlight.formUnion(missing)
        for id in missing {
            do {
                let retired = try await client.task(id)
                retiredTitles[id] = "#\(retired.ghIssueNumber) \(retired.title)"
            } catch {
                // Leave unresolved; the row falls back to the branch name.
            }
            retiredTitleFetchesInFlight.remove(id)
        }
    }

    /// Nil on cancellation (mid-teardown — not an error, not a result).
    private nonisolated static func attempt<T: Sendable>(
        _ op: @Sendable () async throws -> T
    ) async -> Result<T, any Error>? {
        do {
            return .success(try await op())
        } catch is CancellationError {
            return nil
        } catch {
            return .failure(error)
        }
    }
}
