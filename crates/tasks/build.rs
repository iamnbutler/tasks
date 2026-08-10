// sqlx::migrate! embeds ./migrations at compile time; without this, adding a
// migration file alone doesn't dirty the crate and a cached binary ships
// without it (which happened with 0006).
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
