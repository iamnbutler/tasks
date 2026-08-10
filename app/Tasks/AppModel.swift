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

    /// Optimistic drag-reorder; the server's answer (or a refresh on failure)
    /// is authoritative. Since #770 the reorder response is the same filtered
    /// projection as GET /tasks, so applying it directly is safe.
    func moveTasks(from source: IndexSet, to destination: Int) {
        var reordered = tasks
        reordered.move(fromOffsets: source, toOffset: destination)
        tasks = reordered
        Task {
            do {
                tasks = try await client.reorderQueue(reordered.map(\.id))
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
            self.projects = try await projects
            self.tasks = try await tasks
            self.sessions = try await sessions
            self.specs = try await specs
            self.specQueue = try await specQueue
            self.mode = try await mode
            connectionError = nil
            lastRefreshed = Date()
        } catch is CancellationError {
            return
        } catch {
            connectionError = error.localizedDescription
        }
    }
}
