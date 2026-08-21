# Changelog

Releases of Tasks, newest first. A release is a human choosing a commit on
`main` and publishing the two downloadables built from it — the app
(`Tasks-<version>.dmg`) and the standalone server
(`tasks-server-<version>-macos-arm64.zip`) — under one number.

**The number is `0.1.<commit count>`**, the same identity `build-stamp` already
puts in every binary, so a tag, a `GET /version`, a DMG filename and a heading
here all say the same thing. `[workspace.package] version` in `Cargo.toml` is
**not** that number and is inert; nothing in this workspace publishes to
crates.io. Tags are annotated, cut only by `make publish`, and never moved or
deleted — a release names a commit, so `v0.1.<n>` has to keep meaning the one
it named.

**Between releases, `git log` is the record.** This file is a release-time
digest, not a per-merge obligation: the pipeline merges its own pull requests
many times a day and nothing about that cadence changes. Sections are generated
by `scripts/changelog.sh` from the commits themselves — every landing on `main`,
plus every pull-request merge that landed on another build's branch first,
which this pipeline does routinely. There are no Added/Fixed/Changed
categories: the subjects here are already sentences, and a category imposed at
publish time is the step that gets skipped and then rots the format.
