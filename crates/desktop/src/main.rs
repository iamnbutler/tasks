//! Tasks Desktop — Main entry point.
//!
//! A GPUI-based native desktop application for the Tasks platform.

use gpui::{
    App, AppContext as _, Application, Context, Entity, IntoElement, ParentElement, Render, Styled,
    Window, WindowOptions, actions, px,
};
use std::sync::Arc;
use tasks_desktop::{
    SseClient, SseClientEvent, SseConnectionState, SseFilters, colors, spacing,
    style_helpers::{StyledExt, container, heading, muted_text, status_dot},
    typography,
};
use tracing_subscriber::EnvFilter;

actions!(desktop, [Quit]);

/// Default server URL.
const DEFAULT_SERVER_URL: &str = "http://localhost:4800";

fn main() {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    Application::new().run(|cx: &mut App| {
        // Register quit action
        cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
        cx.bind_keys([gpui::KeyBinding::new("cmd-q", Quit, None)]);

        cx.open_window(
            WindowOptions {
                window_bounds: Some(gpui::WindowBounds::Windowed(gpui::Bounds {
                    origin: gpui::Point::default(),
                    size: gpui::Size {
                        width: px(800.),
                        height: px(600.),
                    },
                })),
                ..Default::default()
            },
            |_window, cx| {
                let view = cx.new(|cx| TasksApp::new(cx));
                view
            },
        )
        .unwrap();

        cx.activate(true);
    });
}

/// Main application view.
struct TasksApp {
    #[allow(dead_code)] // Kept for future disconnect functionality
    sse_client: Entity<SseClient>,
    connection_state: SseConnectionState,
    event_count: usize,
    last_event: Option<Arc<events::Event>>,
}

impl TasksApp {
    fn new(cx: &mut Context<Self>) -> Self {
        // Get server URL from environment or use default
        let server_url =
            std::env::var("TASKS_SERVER_URL").unwrap_or_else(|_| DEFAULT_SERVER_URL.to_string());

        // Create SSE client
        let filters = SseFilters::new();
        let sse_client = cx.new(|_| SseClient::new(&server_url, filters));

        // Subscribe to SSE events
        cx.subscribe(
            &sse_client,
            |this: &mut Self, _entity, event: &SseClientEvent, cx| match event {
                SseClientEvent::StateChanged(state) => {
                    this.connection_state = *state;
                    cx.notify();
                }
                SseClientEvent::EventReceived(event) => {
                    this.event_count += 1;
                    this.last_event = Some(event.clone());
                    cx.notify();
                }
                SseClientEvent::Error(err) => {
                    tracing::error!(error = %err, "SSE error");
                    cx.notify();
                }
            },
        )
        .detach();

        // Start connection
        sse_client.update(cx, |client: &mut SseClient, cx| {
            client.connect(cx);
        });

        Self {
            sse_client,
            connection_state: SseConnectionState::Connecting,
            event_count: 0,
            last_event: None,
        }
    }
}

impl Render for TasksApp {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let status_text = match self.connection_state {
            SseConnectionState::Disconnected => "Disconnected",
            SseConnectionState::Connecting => "Connecting...",
            SseConnectionState::Connected => "Connected",
            SseConnectionState::Reconnecting => "Reconnecting...",
            SseConnectionState::Failed => "Connection Failed",
        };

        let status_color = match self.connection_state {
            SseConnectionState::Connected => colors::STATE_COMPLETED,
            SseConnectionState::Connecting | SseConnectionState::Reconnecting => {
                colors::STATE_QUESTION
            }
            SseConnectionState::Disconnected | SseConnectionState::Failed => colors::STATE_FAILED,
        };

        let last_event_text = self
            .last_event
            .as_ref()
            .map(|e| format!("{}: {}", e.event_type.as_str(), e.task))
            .unwrap_or_else(|| "No events yet".to_string());

        container()
            .p(spacing::SPACE_4)
            .gap(spacing::SPACE_4)
            .child(
                gpui::div()
                    .flex()
                    .flex_col()
                    .gap(spacing::SPACE_2)
                    .child(
                        heading(typography::TEXT_XL)
                            .font_weight(typography::WEIGHT_BOLD)
                            .child("Tasks Desktop"),
                    )
                    .child(muted_text().child("GPUI-based desktop client for the Tasks platform")),
            )
            .child(
                gpui::div()
                    .flex()
                    .items_center()
                    .gap(spacing::SPACE_2)
                    .child(status_dot(status_color))
                    .child(
                        gpui::div()
                            .text_primary()
                            .child(format!("Status: {}", status_text)),
                    ),
            )
            .child(
                gpui::div()
                    .flex()
                    .flex_col()
                    .gap(spacing::SPACE_1)
                    .child(
                        gpui::div()
                            .text_primary()
                            .child(format!("Events received: {}", self.event_count)),
                    )
                    .child(muted_text().child(format!("Last event: {}", last_event_text))),
            )
    }
}
