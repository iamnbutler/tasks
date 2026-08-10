# Golden JSON wire fixtures shared by the Rust API and Swift client tests

Closes #763.

The SwiftUI client's models are hand-mirrored from `crates/tasks/src/models.rs`,
and nothing catches them drifting apart. This adds a repo-root `fixtures/`
directory holding one committed JSON file per wire shape the HTTP API returns,
and puts both sides of the API under test against it.

`crates/tasks/tests/wire_fixtures.rs` serializes a deterministic instance of
every shape and asserts a byte-for-byte match, with a per-file line diff on
failure and an `UPDATE_FIXTURES=1` regeneration path so changing the contract is
a conscious, reviewable act. `app/TasksTests/WireFixtureTests.swift` decodes
every one of those files through the app's *production* decoder
(`TasksClient.makeDecoder()`), so a rename or a new enum variant on the Rust
side fails `cargo test` first and then keeps failing the app's suite until the
Swift models catch up. The fixture set is 34 files: every entity, both the
populated and the null/empty half of each optional-heavy shape
(`task_minimal`, `session_running`, `spec_queue_item_pending`, `build_queued`),
the `{"error": …}` body, `build_detail` and `mode_response`, one file per
`EventPayload` variant inside the `Event` envelope plus the `from: null` shape,
an `enums.json` inventory of every snake_case value the API can emit, and a
`timestamps.json` covering all four fractional-second widths chrono produces.

The forcing functions are the point. Every enum inventory and `kind_of()` is an
exhaustive match, so adding a variant in `models.rs` or `events.rs` stops the
test file compiling until the variant is accounted for (verified: planting a
`TaskState::Deploying` and an `EventPayload::BuilderResourceAdded` breaks it at
exactly those two matches). `fixtures_dir_has_no_orphans` fails on a `.json`
nothing generates, so a renamed shape can't leave a stale file the Swift side
still reads. `error_fixture_matches_api_error` drives the real
`ApiError::into_response()` rather than trusting a hand-written body. On the
Swift side, `everyFixtureOnDiskIsCovered` compares the on-disk set against the
tested set in both directions.

**Client changes fixed by the fixtures.** Writing them immediately caught three
pieces of drift: `TaskState` was missing `building`, `SpecQueueStatus` was
missing `built`, and the app had no `Build`/`BuildStatus`/`BuildDetail` types at
all. Those are added, along with a typed `Event`/`EventPayload` with an
`.unknown(kind:)` fallback — the app previously treated every SSE line as
"refetch everything", and the typed payload gives a future `AppModel` the option
of refetching one entity instead. The loose `ActivityEvent` the Activity feed
renders is kept and is now pinned by the same fixtures. `makeDecoder()` and the
nested `ServerError` went from `private` to internal so `@testable import`
reaches them; a decoder built inside a test would prove nothing about
production. `ModeResponse` and `BuildDetail` became `pub` in `server.rs` for the
same reason. `docs/clients.md` gains a "Contract drift" section, and its
`GET /specs` bullet is corrected — it documented `spec_markdown` and
`agent_exit_code`, and the server serves `content` and neither of the others.
That is exactly the drift the issue describes, found by writing the fixture.

Fixtures deliberately do **not** make runtime parsing strict: clients keep
parsing enums leniently, and the tests assert only that everything the server
can emit *today* is handled today.

## Verification

`cargo test --workspace` is green (all 26 test binaries), `cargo clippy
--workspace --all-targets` reports nothing new, `cargo fmt` clean. Both Rust
failure modes were exercised by hand — a corrupted `task.json` and a planted
`stale_shape.json` — and the output names the offending line pair and the exact
regeneration command; keep that if the harness is ever refactored.

The Swift suite was run on **Linux**, not in Xcode: a Swift 6.1.2 toolchain and
a scratch SwiftPM package assembled from the *real* `app/Tasks/Models.swift` and
`app/Tasks/TasksClient.swift` plus the two new test files. **All 24 tests pass
against the actual committed fixtures**, and both failure directions were
confirmed by planting an extra `task_state` value, an extra enum family, and an
uncovered fixture file. The one Linux-only shim needed (`FoundationNetworking`
and a stub for `URLSession.bytes(from:)`, which swift-corelibs-foundation lacks)
lives only in the scratch copy and touches only the SSE methods, which the
fixtures never exercise.

**What that run cannot cover, and what a reviewer on a Mac should do:** the
Xcode plumbing. `app/Tasks.xcodeproj/project.pbxproj` gains a hand-written
`TasksTests` unit-test target (objectVersion 77, so it uses a
`PBXFileSystemSynchronizedRootGroup` like the app target rather than per-file
build entries), `TEST_HOST`/`BUNDLE_LOADER` pointed at `Tasks.app`, a dependency
on `Tasks`, and a `<Testables>` entry in the shared scheme. It was validated
structurally — balanced braces, every object id both defined and referenced, all
sections closed, scheme XML parses, both blueprint ids resolve to real targets —
but "parses" is not "builds". Please run once:

```sh
xcodebuild test -project app/Tasks.xcodeproj -scheme Tasks -destination 'platform=macOS'
```

Two other things worth knowing. The Swift tests read `<repo>/fixtures` off disk
via `#filePath` rather than bundling the JSON as a test resource — the files
live outside `app/`, and a copy would be a second thing to keep in sync, which
is the exact failure mode being fixed. That works because the app has no App
Sandbox entitlement and the test bundle inherits the host's; if the app is ever
sandboxed, the fixtures have to become a copied bundle resource, which is why
the test target already carries an empty `PBXResourcesBuildPhase`. And this repo
has no CI, so the fixtures only bite for whoever runs the tests — a workflow
running `cargo test --workspace` (plus `xcodebuild test` on a macOS runner) is
what would turn this from a convention into a guarantee. Out of scope here, but
the value of the fixtures is roughly proportional to something running them.

New wire shapes on the roadmap (Builder resources, progress visibility) should
each arrive with a fixture; `fixtures/README.md` and the clients.md section both
say so.
