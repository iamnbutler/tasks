fn main() {
    // Stamps TASKS_CLIENT_VERSION / TASKS_CLIENT_COMMIT — the build the
    // connect-time preflight reports as "this client" when the embedder
    // doesn't name its own (an app passes `about::VERSION` instead).
    build_stamp::emit("TASKS_CLIENT");
}
