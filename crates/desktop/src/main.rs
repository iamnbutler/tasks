//! Tasks Desktop — GPUI + gpuikit native client.

use std::collections::BTreeMap;

use gpui::{
    actions, div, point, prelude::FluentBuilder, px, size, App, AppContext as _, Application,
    Bounds, Context, Entity, FocusHandle, Focusable, FontWeight, InteractiveElement, IntoElement,
    KeyBinding, ParentElement, Render, SharedString, Styled, Window, WindowBounds,
    WindowControlArea, WindowOptions,
};
use gpuikit::elements::icon_button::icon_button;
use gpuikit::elements::input::input;
use gpuikit::elements::scroll_area::scroll_area;
use gpuikit::icons::Icons;
use gpuikit::input::InputState;
use gpuikit::layout::{h_stack, v_stack};
use gpuikit::theme::{ActiveTheme, Themeable};
use models::project::Project;
use tasks_desktop::state::{AppState, create_app_state};

actions!(tasks_desktop, [Quit]);

const TRAFFIC_LIGHT_PADDING: f32 = 71.;
const TOOLBAR_HEIGHT: f32 = 46.;
const SIDEBAR_WIDTH: f32 = 220.;
const LIST_WIDTH: f32 = 280.;

fn main() {
    // Install tokio reactor so reqwest works inside GPUI's smol executor.
    let _tokio_guard = tasks_desktop::install_tokio();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    Application::with_platform(gpui_platform::current_platform(false))
        .with_assets(gpuikit::assets())
        .run(|cx: &mut App| {
            gpuikit::init(cx);

            cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
            cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);

            let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(gpui::TitlebarOptions {
                        title: Some(SharedString::from("Tasks")),
                        appears_transparent: true,
                        traffic_light_position: Some(point(px(9.), px(9.))),
                    }),
                    ..Default::default()
                },
                |window, cx| {
                    let view = cx.new(|cx| TasksApp::new(window, cx));
                    let focus_handle = view.read(cx).focus_handle.clone();
                    window.focus(&focus_handle, cx);
                    view
                },
            )
            .unwrap();

            cx.activate(true);
        });
}

/// Group projects by owner (the part before `/` in the repo string).
fn group_projects_by_owner(projects: &[Project]) -> BTreeMap<String, Vec<&Project>> {
    let mut groups: BTreeMap<String, Vec<&Project>> = BTreeMap::new();
    for project in projects {
        let owner = project
            .repo
            .split('/')
            .next()
            .unwrap_or("Unknown")
            .to_string();
        groups.entry(owner).or_default().push(project);
    }
    groups
}

struct TasksApp {
    focus_handle: FocusHandle,
    app_state: Entity<AppState>,
    search_input: Entity<InputState>,
}

impl TasksApp {
    fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let app_state = cx.new(|cx| create_app_state(cx));
        let search_input = cx.new(|cx| InputState::new_singleline(cx));

        // Re-render when app state changes
        cx.subscribe(&app_state, |_this, _state, _event, cx| {
            cx.notify();
        })
        .detach();

        Self {
            focus_handle: cx.focus_handle(),
            app_state,
            search_input,
        }
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let state = self.app_state.read(cx);
        let projects = state.projects();
        let selected_id = state.selected_project().map(|s| s.to_string());
        let grouped = group_projects_by_owner(projects);

        v_stack()
            .id("sidebar")
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .flex_shrink_0()
            .bg(theme.surface())
            .border_r_1()
            .border_color(theme.border())
            // Toolbar: traffic lights left, + button right
            .child(
                h_stack()
                    .id("sidebar-toolbar")
                    .window_control_area(WindowControlArea::Drag)
                    .h(px(TOOLBAR_HEIGHT))
                    .w_full()
                    .flex_shrink_0()
                    .pl(px(TRAFFIC_LIGHT_PADDING))
                    .pr_2()
                    .items_center()
                    .justify_end()
                    .child(
                        icon_button("add-project", Icons::plus()).on_click(
                            cx.listener(|_this, _event, _window, _cx| {
                                // TODO: open add project dialog
                            }),
                        ),
                    ),
            )
            // Search input
            .child(
                div()
                    .px_2()
                    .pb_1()
                    .child(input(&self.search_input, cx).placeholder("Search...")),
            )
            // Scrollable project list grouped by owner
            .child(
                scroll_area("project-list")
                    .vertical()
                    .full_width(true)
                    .full_height(true)
                    .child(v_stack().w_full().pb_2().children(
                        grouped.into_iter().map({
                            let theme = theme.clone();
                            let selected_id = selected_id.clone();
                            move |(owner, projects)| {
                                let theme = theme.clone();
                                let selected_id = selected_id.clone();
                                v_stack()
                                    .w_full()
                                    .child(
                                        div()
                                            .px_3()
                                            .pt_3()
                                            .pb_1()
                                            .text_xs()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(theme.fg_muted())
                                            .child(owner),
                                    )
                                    .children(projects.into_iter().map({
                                        let theme = theme.clone();
                                        let selected_id = selected_id.clone();
                                        move |project| {
                                            let repo_name = project
                                                .repo
                                                .split('/')
                                                .nth(1)
                                                .unwrap_or(&project.repo);
                                            let is_selected =
                                                selected_id.as_deref() == Some(&project.id);

                                            div()
                                                .id(SharedString::from(format!(
                                                    "project-{}",
                                                    project.id
                                                )))
                                                .mx_2()
                                                .px_2()
                                                .py_1()
                                                .rounded_md()
                                                .text_sm()
                                                .cursor_pointer()
                                                .when(is_selected, |el| {
                                                    el.bg(theme.selection())
                                                        .text_color(theme.fg())
                                                })
                                                .when(!is_selected, |el| {
                                                    el.text_color(theme.fg()).hover(|style| {
                                                        style.bg(theme.border_subtle())
                                                    })
                                                })
                                                .child(repo_name.to_string())
                                        }
                                    }))
                            }
                        }),
                    )),
            )
    }

    fn render_list(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();

        v_stack()
            .id("list")
            .w(px(LIST_WIDTH))
            .h_full()
            .flex_shrink_0()
            .bg(theme.bg())
            .border_r_1()
            .border_color(theme.border())
            .child(
                h_stack()
                    .id("list-toolbar")
                    .window_control_area(WindowControlArea::Drag)
                    .h(px(TOOLBAR_HEIGHT))
                    .w_full()
                    .flex_shrink_0()
                    .px_3()
                    .items_center()
                    .border_b_1()
                    .border_color(theme.border()),
            )
            .child(
                v_stack().flex_1().p_2().child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.fg_muted())
                        .child("TASKS"),
                ),
            )
    }

    fn render_detail(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();

        v_stack()
            .id("detail")
            .flex_1()
            .h_full()
            .bg(theme.bg())
            .child(
                h_stack()
                    .id("detail-toolbar")
                    .window_control_area(WindowControlArea::Drag)
                    .h(px(TOOLBAR_HEIGHT))
                    .w_full()
                    .flex_shrink_0()
                    .px_3()
                    .items_center()
                    .border_b_1()
                    .border_color(theme.border()),
            )
            .child(
                v_stack()
                    .flex_1()
                    .p_4()
                    .justify_center()
                    .items_center()
                    .child(
                        div()
                            .text_color(theme.fg_muted())
                            .child("Select a task"),
                    ),
            )
    }
}

impl Focusable for TasksApp {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TasksApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();

        h_stack()
            .id("tasks-app")
            .key_context("TasksApp")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(theme.bg())
            .text_color(theme.fg())
            .child(self.render_sidebar(cx))
            .child(self.render_list(cx))
            .child(self.render_detail(cx))
    }
}
