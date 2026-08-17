fn main() {
    // Stamps BUILDER_SUPERVISOR_VERSION / BUILDER_SUPERVISOR_COMMIT. See
    // crates/scout-supervisor/build.rs.
    build_stamp::emit("BUILDER_SUPERVISOR");
}
