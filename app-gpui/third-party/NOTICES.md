# Third-party notices — Tasks.app

`make dist` produces a redistributable `Tasks.app` whose linked code is not
all MIT. Apache-2.0 §4(a) asks that a copy of the License travel with a
binary distribution, so this directory is copied into
`Tasks.app/Contents/Resources/third-party/` by `make app-install`. The bundle
plist's `NSHumanReadableCopyright` points here rather than claiming MIT for
the whole artifact.

Tasks itself is MIT — see the `LICENSE` at the repository root.

## Components

| Component | Version / rev | License | Text |
| --- | --- | --- | --- |
| `gpui-unofficial` | `=1.14.2` (crates.io) | Apache-2.0 | `LICENSE-APACHE-2.0` |
| `gpui-platform-gpui-unofficial` | `=1.14.2` (crates.io) | Apache-2.0 | `LICENSE-APACHE-2.0` |
| `gpuikit` | rev `b28732f290f8bd8ca7ffcc56a40d00a852ac978a` | MIT OR Apache-2.0 — **taken under MIT** | `gpuikit-LICENSE-MIT` |

`LICENSE-APACHE-2.0` is the file `gpui-unofficial` 1.14.2 itself ships, kept
verbatim including its `Copyright 2022 - 2025 Zed Industries, Inc.` header
line; `gpui-platform-gpui-unofficial` 1.14.2 ships a byte-identical copy
(md5 `776e07ed20b75b675553b3a113323c42`), so one file serves both.
`gpuikit-LICENSE-MIT` is that repository's own `LICENSE-MIT` at the pinned
rev. Neither Apache-2.0 component ships a `NOTICE` file, so §4(d) — which
applies only "If the Work includes a NOTICE text file" — imposes nothing
further; the obligation discharged here is §4(a).

## What this does not claim to be

**A transitive audit.** These are the crates `app-gpui/Cargo.toml` declares
directly, whose licenses were read from the artifacts the build consumes.
The full dependency graph has not been walked, and doing that properly wants
`cargo about` / `cargo-deny` and its own issue rather than a hand-maintained
list that goes stale on the next `cargo update`. This file is the honest
floor, not the finished answer.
