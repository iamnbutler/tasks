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
    var specQueue: [SpecQueueItem] = []
    var mode: Mode?
    /// Recent event log, newest first, for the Activity feed.
    var events: [ActivityEvent] = []
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

    func refresh() async {
        do {
            async let projects = client.projects()
            async let tasks = client.tasks()
            async let sessions = client.sessions()
            async let specs = client.specs()
            async let specQueue = client.specQueue()
            async let mode = client.mode()
            async let events = client.events()
            self.projects = try await projects
            self.tasks = try await tasks
            self.sessions = try await sessions
            self.specs = try await specs
            self.specQueue = try await specQueue
            self.mode = try await mode
            self.events = try await events.sorted { $0.seq > $1.seq }
            connectionError = nil
            lastRefreshed = Date()
        } catch is CancellationError {
            return
        } catch {
            connectionError = error.localizedDescription
        }
    }
}
