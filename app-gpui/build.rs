//! Stamps the version identity shown in About Tasks.
//!
//! `TASKS_GPUI_VERSION` is `0.1.<commit count>` and `TASKS_GPUI_COMMIT` the
//! short SHA (`-dirty` when the tree had uncommitted changes) — the same two
//! values the Swift app carried in MARKETING_VERSION / CURRENT_PROJECT_VERSION.
//! Both can be set in the environment to override the git probe, which is how
//! `make app` guarantees an installed bundle is stamped exactly.
//!
//! The scheme itself lives in `build-stamp`, shared with the server and
//! `tasks-client`: the connect-time build check compares these numbers across
//! processes, which only means anything if one implementation computes them.

fn main() {
    build_stamp::emit("TASKS_GPUI");
}
