fn main() {
    // Stamps SCOUT_SUPERVISOR_VERSION / SCOUT_SUPERVISOR_COMMIT, which this
    // binary answers `--version` with and reports on `ScoutEvent::Started`.
    // The same crate the server, tasks-client and app-gpui use: comparing an
    // image's number against the server's is only meaningful because one
    // implementation computes both.
    build_stamp::emit("SCOUT_SUPERVISOR");
}
