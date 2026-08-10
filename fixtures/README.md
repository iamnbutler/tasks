# Wire fixtures

One committed JSON file per shape the Tasks HTTP API returns. They exist so a
change to the API contract fails a test instead of a user's client.

They live at the repo root rather than inside `crates/tasks/` because they are a
cross-language artifact: the Rust crate generates them, the SwiftUI app asserts
against them, and a third client (TUI, CLI) should too.

## Who reads these

| | |
| --- | --- |
| `crates/tasks/tests/wire_fixtures.rs` | Serializes a deterministic instance of every shape and asserts it matches the committed bytes. Also drives the real `ApiError::into_response()` for `error.json`. |
| `app/TasksTests/WireFixtureTests.swift` | Decodes every file through the app's production decoder (`TasksClient.makeDecoder()`), so the Swift models can't drift from `models.rs` unnoticed. |

A rename on the Rust side fails `cargo test` first, then keeps failing the app's
suite until the Swift models catch up. Neither half is worth much alone.

## Regenerating

```sh
UPDATE_FIXTURES=1 cargo test -p tasks --test wire_fixtures
```

Then read the diff. That's the point of the loop — a fixture change is a wire
contract change, and it should be as visible in review as any other one. If a
field was renamed or a variant added, update `app/TasksTests/WireFixtureTests.swift`
(and any other client) in the same commit.

Regenerating never *deletes* files. `fixtures_dir_has_no_orphans` fails on a
`.json` nothing generates, so a renamed shape can't leave a stale file behind
for a client to keep reading.

## What's in here

- **Entities** — `project`, `task`, `session`, `spec`, `spec_queue_item`,
  `build`, `transcript_line`.
- **Both halves of the optional fields** — `task_minimal`, `session_running`,
  `spec_queue_item_pending`, `build_queued`. Nulls and empty collections are
  pinned, not just the populated path, so a field going optional on the server
  is caught by a test rather than by whoever hits the empty case first.
- **Envelopes** — `task_list`, `build_detail` (`GET /builds/{id}`),
  `mode_response`, `error` (the `{"error": "..."}` body every non-2xx carries).
- **Events** — one file per `EventPayload` variant, inside the `Event`
  envelope, plus `event_spec_queue_status_changed_initial` for the `from: null`
  shape a spec has on its first appearance in the queue.
- **`enums.json`** — every snake_case enum value the API can emit, by enum.
  Generated from exhaustive matches, so a new variant in `models.rs` has to
  pass through here.
- **`timestamps.json`** — all four fractional-second widths chrono emits (0, 3,
  6, 9 digits; the width is value-dependent, not configurable). A client's date
  parser has to handle every one.

## What these do *not* do

They don't make runtime parsing strict. Clients still parse enums leniently —
an unknown wire value renders as its raw string rather than failing the decode
(see `docs/clients.md`). The fixtures assert that the values the server can emit
*today* are all handled today; they don't make a client brittle against a server
that's ahead of it.

## Adding a shape

Anything new the API returns should arrive with a fixture in the same PR. Add it
to `all_fixtures()` in `crates/tasks/tests/wire_fixtures.rs`, regenerate, then
add a test and a `covered` entry in `WireFixtureTests.swift` — the Swift
coverage check compares the on-disk set against the tested set in both
directions, so a new file can't sit here untested.
