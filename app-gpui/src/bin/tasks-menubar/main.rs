//! Tasks in the menu bar: every machine running the pool, at a glance.
//!
//! A separate binary from the Tasks app on purpose — the daemon is the
//! product and this is one more client of its HTTP API, small enough to run
//! on a machine that only serves. It puts one item in the status bar; a
//! click drops a gpui popup with a section per machine (name, mode,
//! serving/uptime, work in flight, holds), a mode chip that toggles
//! play ↔ pause, and a footer (Open Tasks / Refresh / Quit).
//!
//! vm-pool is single-machine today, so the list is usually just this Mac;
//! more machines — each just another tasks server — are named in
//! `TASKS_MENUBAR_MACHINES` (`Name=http://host:4800`, comma-separated). See
//! `machines.rs` for why the shape is a list anyway.
//!
//! On non-mac hosts there is no status bar to live in, so `main` opens the
//! same view as a plain window — which is also what lets the Linux agent VMs
//! that develop this crate compile and exercise it.

mod machines;
mod popup;
#[cfg(target_os = "macos")]
mod status_item;

/// The app's Server-menu model, compiled into this binary too: it owns
/// finding the `tasks` binary (TASKS_BIN → pidfile exe → bundle seed → PATH),
/// the op commands (`reload`/`stop`), and the run/outcome bookkeeping — logic
/// that must not fork between the two front ends. `#[path]` rather than a lib
/// target because the package's own structure should not reorganize around
/// its smallest binary. Dead-code allowed: this bin uses the ops and not the
/// preflight/version half.
#[path = "../../server.rs"]
#[allow(dead_code)]
mod server;

use gpui::{App, Application};

fn main() {
    let app = Application::with_platform(gpui_platform::current_platform(false))
        .with_assets(gpuikit::assets());

    app.run(|cx: &mut App| {
        gpuikit::theme::init(cx);
        popup::bind_keys(cx);
        machines::init(cx);
        // The SERVER section's model: ops, run tracking, and its own /status
        // probe (the popup re-probes it while open). One warm read now so the
        // first open's Start/Stop label is right rather than defaulted.
        server::init(cx);
        server::ServerControl::global(cx).update(cx, |control, cx| control.refresh(cx));

        #[cfg(target_os = "macos")]
        {
            // `to_async` because the click arrives from AppKit's run loop,
            // outside any gpui update; the handle re-enters the app there.
            let async_cx = cx.to_async();
            status_item::install(Box::new(move |anchor| {
                async_cx.update(|cx| popup::toggle(cx, anchor));
            }));

            // Dev affordance: open the dropdown immediately, so the popup
            // can be iterated on (and exercised headlessly) without a hand
            // on the status item.
            if std::env::var_os("TASKS_MENUBAR_OPEN_AT_LAUNCH").is_some() {
                popup::toggle(
                    cx,
                    popup::Anchor {
                        x: 400.0,
                        bottom: 30.0,
                    },
                );
            }
        }

        #[cfg(not(target_os = "macos"))]
        popup::open_detached(cx);
    });
}
