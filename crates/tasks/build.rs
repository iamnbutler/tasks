fn main() {
    // sqlx::migrate! embeds ./migrations at compile time; without this, adding
    // a migration file alone doesn't dirty the crate and a cached binary ships
    // without it (which happened with 0006). Emitting *any* rerun-if-changed
    // replaces cargo's default package-wide watch, so this line and the ones
    // build_stamp emits (src, build.rs, .git/*) have to coexist — dropping
    // either set silently breaks the other's freshness.
    println!("cargo:rerun-if-changed=migrations");
    // Stamps TASKS_SERVER_VERSION / TASKS_SERVER_COMMIT for `GET /version`.
    build_stamp::emit("TASKS_SERVER");
}
