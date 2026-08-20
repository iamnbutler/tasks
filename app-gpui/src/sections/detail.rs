//! The selected task's Overview and Brief tabs — what the right-sidebar
//! inspector became in the v3 frame swap. The server is the authority on
//! which transitions are legal — buttons are offered by state, and a rejected
//! action surfaces the server's own error message in the banner.
//!
//! The split: **Overview** is the landing tab — identity, state, the
//! orchestrator-context block, actions, the build-now form, the bundle
//! block, and the issue body. **Brief** is the spec and its verdict — the
//! review form lives beside the text it rules on, with the revision feedback
//! that shaped a re-scout above it.

use gpui::prelude::*;
use gpui::{div, px, AnyElement, ClickEvent, ClipboardItem, Context, Hsla};
use gpuikit::elements::input::text_area;
use gpuikit::theme::{ActiveTheme, Themeable};
use tasks_client::api::http::RejectedBundle;
use tasks_client::api::models::{
    BuildId, Complexity, GhState, SpecId, SpecQueueStatus, TaskId, TaskState,
};

use crate::components::{byte_size, status_badge, task_state_color, title_case};
use crate::time;
use crate::workspace::Workspace;

/// Amber: this is not an error and not a success. The build failed, but the
/// work survived — the block exists to be noticed, not to alarm.
const BUNDLE_ACCENT: Hsla = Hsla {
    h: 35. / 360.,
    s: 0.80,
    l: 0.55,
    a: 1.,
};

/// Reading-width cap for tab content. The middle column is far wider than
/// the 460px sidebar this UI grew up in, and prose lines that span it stop
/// being readable.
const CONTENT_MAX_WIDTH: gpui::Pixels = px(760.);

/// Owned projection of the selected task — extracted up front so no borrow
/// of the app state entity is held while listeners are created.
struct TaskView {
    id: TaskId,
    title: String,
    number: u64,
    state: TaskState,
    gh_state: GhState,
    labels: Vec<String>,
    updated_at: chrono::DateTime<chrono::Utc>,
    body: String,
    github_url: Option<String>,
    /// The task's newest spec, whatever the queue has decided about it —
    /// which is why it is not gated on `in_review`. A spec is the best
    /// description of the work there is; hiding it the moment a verdict lands
    /// left the Brief showing nothing for everything downstream of review.
    spec: Option<SpecView>,
    /// The spec id when that spec is still awaiting a verdict. What the review
    /// actions key off — approving something already approved is not an
    /// action, it is a second click that means nothing.
    pending_spec: Option<SpecId>,
    /// The feedback attached to the newest queue entry for this task's spec,
    /// when there is any — the words that sent a spec back, kept in front of
    /// the re-review they shaped.
    review_feedback: Option<String>,
    /// An implementation of this task whose branch could not be pushed, and
    /// which therefore exists only as a file on the server host. Cloned rather
    /// than borrowed for the same reason as everything else here: no borrow of
    /// the state entity may be held while listeners are built.
    bundle: Option<RejectedBundle>,
}

/// The spec as the Brief renders it.
struct SpecView {
    id: SpecId,
    complexity: Complexity,
    content: String,
    /// No Scout ran: a human wrote this spec through Build Now, and there is
    /// no session, no transcript and no second opinion behind it. Rendered
    /// rather than inferred from a missing scout link.
    human_authored: bool,
}

impl Workspace {
    /// The selected task, projected and owned. `None` when nothing is
    /// selected or the task has left the working set — the caller renders
    /// the empty sentence.
    fn task_view(&self, cx: &Context<Self>) -> Option<TaskView> {
        let state = self.app_state.read(cx);
        self.selected_task
            .as_ref()
            .and_then(|id| state.task(id))
            .map(|task| {
                let spec = state.latest_spec(&task.id);
                let entry = state.latest_queue_entry(&task.id);
                let pending = spec.filter(|spec| {
                    state.spec_queue.iter().any(|item| {
                        item.entry.spec_id == spec.id
                            && item.entry.status == SpecQueueStatus::PendingReview
                    })
                });
                TaskView {
                    id: task.id.clone(),
                    title: task.title.clone(),
                    number: task.gh_issue_number,
                    state: task.state,
                    gh_state: task.gh_state,
                    labels: task.labels.clone(),
                    updated_at: task.updated_at,
                    body: task.body.clone(),
                    github_url: state.github_url(task),
                    spec: spec.map(|spec| SpecView {
                        id: spec.id.clone(),
                        complexity: spec.complexity,
                        content: spec.content.clone(),
                        human_authored: spec.session_id.is_none(),
                    }),
                    pending_spec: pending.map(|spec| spec.id.clone()),
                    review_feedback: entry
                        .and_then(|item| item.entry.feedback.clone())
                        .filter(|feedback| !feedback.trim().is_empty()),
                    bundle: state.bundle_for_task(&task.id).cloned(),
                }
            })
    }

    /// The scrolling column every tab body lives in: padded, reading-width
    /// capped, keyed so two tabs' scroll positions stay separate.
    pub(crate) fn tab_pane(id: &'static str) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .flex()
            .flex_col()
            .size_full()
            .overflow_y_scroll()
            .p(px(16.))
    }

    /// The Overview tab — the landing surface for a selected task.
    pub(crate) fn render_overview(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let Some(task) = self.task_view(cx) else {
            return self.render_missing_task(cx);
        };

        let mut pane = div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .max_w(CONTENT_MAX_WIDTH);

        pane = pane.child(
            div()
                .flex()
                .flex_row()
                .items_start()
                .gap(px(8.))
                .child(
                    // A flex item's automatic minimum size is a MIN_CONTENT
                    // measure of its content, and gpui's text element answers
                    // that probe with its whole unwrapped line — so without
                    // this the row is floored at the entire title on one line
                    // and `flex_1`'s 0% basis is clamped *up* to that floor.
                    // `min_w(px(0.))` (CSS `min-width: 0`) drops the floor to
                    // zero, taffy's final pass then hands the text element a
                    // definite width, and the same code path wraps it.
                    //
                    // `.overflow_hidden()` reaches the same taffy branch and
                    // is *not* an equivalent fix here: it installs a content
                    // mask that would clip the second line this exists to
                    // reveal. That pairing is why every `.truncate()` site in
                    // this app carries it — it was never about the ellipsis.
                    //
                    // The rule, for the next row: in a `flex_row`, a text
                    // child needs either `min_w(px(0.))` to wrap or
                    // `overflow_hidden() + truncate()` to ellipsize. The
                    // default — neither — is the one combination that
                    // misbehaves, and it misbehaves silently, since nothing
                    // in the test suite can see it. It is a rule about text
                    // that can *grow*, not about the shape alone: `rail.rs`'s
                    // "Tasks" heading is the same shape with a static string
                    // child and is deliberately left as it is.
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .text_lg()
                        .text_color(theme.fg())
                        .child(task.title.clone()),
                )
                .child(
                    div()
                        .id("close-task")
                        .flex_none()
                        .cursor_pointer()
                        .text_xs()
                        .text_color(theme.fg_muted())
                        .hover(|el| el.opacity(0.7))
                        .on_click(cx.listener(|this, _event, window, cx| {
                            this.clear_selection(window, cx);
                        }))
                        .child("✕"),
                ),
        );

        pane = pane.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.))
                .child(status_badge(
                    title_case(task.state.as_str()),
                    task_state_color(task.state),
                ))
                .child(status_badge(
                    task.gh_state.as_str().to_string(),
                    match task.gh_state {
                        GhState::Open => gpui::hsla(135. / 360., 0.55, 0.52, 1.),
                        GhState::Closed => gpui::hsla(280. / 360., 0.70, 0.68, 1.),
                    },
                ))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.fg_muted())
                        .child(format!("#{}", task.number)),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.fg_muted())
                        .child(format!("updated {}", time::relative(task.updated_at))),
                ),
        );

        if !task.labels.is_empty() {
            pane = pane.child(
                div()
                    .text_xs()
                    .text_color(theme.fg_muted())
                    .child(task.labels.join(", ")),
            );
        }

        // The orchestrator-context block, per the v3 design: a one-off
        // generation seeded from the orchestrator's ambient context. The
        // endpoint does not exist yet (docs/plans/2026-08-17-v3-ui.md §8.1),
        // so the block renders its honest empty state — the layout is real,
        // the follow-up is purely server-side.
        pane = pane.child(
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .pt(px(4.))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.fg_muted())
                        .child("ORCHESTRATOR CONTEXT"),
                )
                .child(
                    div()
                        .italic()
                        .text_sm()
                        .text_color(theme.fg_muted())
                        .opacity(0.7)
                        .child("Not yet generated."),
                ),
        );

        // Above the actions, deliberately: every button below is about what to
        // do *next* with this task, and all of them are the wrong move while
        // a finished implementation of it is sitting unrecovered on a disk.
        if let Some(bundle) = task.bundle.clone() {
            pane = pane.child(self.render_bundle(bundle, cx));
        }

        // Actions by state; the server enforces legality.
        let mut actions = div().flex().flex_row().flex_wrap().gap(px(6.));
        let mut any_action = false;
        match task.state {
            TaskState::Backlog => {
                any_action = true;
                actions = actions
                    .child(self.action_button(
                        "queue-task",
                        "Add to Queue",
                        None,
                        cx.listener({
                            let id = task.id.clone();
                            move |this, _: &ClickEvent, _window, cx| {
                                let id = id.clone();
                                this.app_state
                                    .update(cx, |state, cx| state.queue_task(id, cx));
                            }
                        }),
                        cx,
                    ))
                    .child(self.action_button(
                        "scout-task",
                        "Scout Now",
                        None,
                        cx.listener({
                            let id = task.id.clone();
                            move |this, _: &ClickEvent, _window, cx| {
                                let id = id.clone();
                                this.app_state
                                    .update(cx, |state, cx| state.scout_task_now(id, cx));
                            }
                        }),
                        cx,
                    ));
            }
            TaskState::Queued => {
                any_action = true;
                actions = actions
                    .child(self.action_button(
                        "scout-task",
                        "Scout Now",
                        None,
                        cx.listener({
                            let id = task.id.clone();
                            move |this, _: &ClickEvent, _window, cx| {
                                let id = id.clone();
                                this.app_state
                                    .update(cx, |state, cx| state.scout_task_now(id, cx));
                            }
                        }),
                        cx,
                    ))
                    .child(self.action_button(
                        "dequeue-task",
                        "Remove from Queue",
                        None,
                        cx.listener({
                            let id = task.id.clone();
                            move |this, _: &ClickEvent, _window, cx| {
                                let id = id.clone();
                                this.app_state
                                    .update(cx, |state, cx| state.dequeue_task(id, cx));
                            }
                        }),
                        cx,
                    ));
            }
            _ => {}
        }
        // A pending spec's quick approve stays here too — the row menu and
        // ⇧⌘A path — but the *considered* verdicts live on the Brief, beside
        // the text they rule on.
        if let Some(spec_id) = &task.pending_spec {
            any_action = true;
            actions = actions.child(self.action_button(
                "approve-spec",
                "Approve",
                Some(gpui::hsla(135. / 360., 0.55, 0.45, 1.)),
                cx.listener({
                    let id = spec_id.clone();
                    move |this, _: &ClickEvent, _window, cx| {
                        // Carries the review draft if there is one: an
                        // approval's feedback reaches the Builder as a
                        // required section of its prompt, so this is how a
                        // human approves *with* changes rather than sending a
                        // whole spec back for one of them. Not gated on
                        // `has_text` — approve is the one exit that does not
                        // need text.
                        let feedback = this.take_review_draft(cx);
                        let id = id.clone();
                        this.app_state.update(cx, |state, cx| {
                            state.review_spec(id, SpecQueueStatus::Approved, feedback, cx)
                        });
                    }
                }),
                cx,
            ));
            actions = actions.child(self.action_button(
                "review-spec",
                "Review Brief…",
                None,
                cx.listener(|this, _: &ClickEvent, _window, cx| {
                    this.task_tab = crate::nav::TaskTab::Brief;
                    cx.notify();
                }),
                cx,
            ));
        }
        if let Some(url) = task.github_url.clone() {
            any_action = true;
            actions = actions.child(self.action_button(
                "open-github",
                "Open on GitHub",
                None,
                move |_: &ClickEvent, _window, cx| cx.open_url(&url),
                cx,
            ));
        }
        if any_action {
            pane = pane.child(actions);
        }

        // Build now: skip the Scout for a task whose issue body already is the
        // spec. Only before any work has started — past `queued` a Scout has
        // run or is running, and the server refuses it anyway.
        //
        // The draft is the *rationale*, and the button is inert without it.
        // Nothing else in this path carries a second opinion — the human
        // writing the spec is the review — so the reason it needed none is the
        // only thing a later reader has. The server does not demand it; this
        // form demands it of itself, because a one-click path to an unreviewed
        // build is not worth the seconds it saves.
        if matches!(task.state, TaskState::Backlog | TaskState::Queued) {
            let has_text = !self.build_input.read(cx).content().trim().is_empty();
            pane = pane.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.fg_muted())
                            .child("BUILD NOW — the issue body becomes the spec, unreviewed"),
                    )
                    .child(
                        div()
                            .h(px(52.))
                            .p(px(4.))
                            .rounded(px(6.))
                            .border_1()
                            .border_color(theme.border_secondary())
                            .bg(theme.bg())
                            .text_sm()
                            .child(text_area(&self.build_input, cx).size_full()),
                    )
                    .child(div().flex().flex_row().items_center().gap(px(6.)).child(
                        self.form_button(
                            "build-now",
                            "Build Now",
                            gpui::hsla(200. / 360., 0.70, 0.58, 1.),
                            has_text,
                            cx.listener(|this, _: &ClickEvent, _window, cx| {
                                this.build_selected_task_now(cx);
                            }),
                            cx,
                        ),
                    )),
            );
        }

        // The issue body — the task as GitHub knows it. The spec's home is
        // the Brief; showing it here too would be the same text twice one
        // tab apart.
        if !task.body.is_empty() {
            let entity = self
                .markdown_cache()
                .entity(format!("task:{}", task.id), &task.body, cx);
            pane = pane.child(
                div()
                    .pt(px(4.))
                    .text_sm()
                    .text_color(theme.fg_muted())
                    .child(crate::components::markdown_block(&entity, cx)),
            );
        }

        Self::tab_pane("task-overview-scroll")
            .child(pane)
            .into_any_element()
    }

    /// The Brief tab — the spec and its verdict.
    pub(crate) fn render_brief(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let Some(task) = self.task_view(cx) else {
            return self.render_missing_task(cx);
        };

        let mut pane = div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .max_w(CONTENT_MAX_WIDTH);

        let Some(SpecView {
            id: spec_id,
            complexity,
            content,
            human_authored,
        }) = task.spec
        else {
            return Self::tab_pane("task-brief-scroll")
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap(px(6.))
                        .child(div().text_sm().text_color(theme.fg()).child("No brief yet"))
                        .child(
                            div()
                                .max_w(px(420.))
                                .text_center()
                                .text_xs()
                                .text_color(theme.fg_muted())
                                .child(
                                    "A Scout writes one when its run concludes; Build Now \
                                     writes one from the issue body.",
                                ),
                        ),
                )
                .into_any_element();
        };

        let mut header = format!("SPEC · {}", complexity.as_str().to_uppercase());
        if human_authored {
            // Said rather than left to be inferred from a scout link that
            // isn't there: nobody but the author ever read this one.
            header.push_str(" · HUMAN-AUTHORED");
        }
        pane = pane.child(div().text_xs().text_color(theme.fg_muted()).child(header));

        // The words that sent this back, above the re-review they shaped.
        if let Some(feedback) = task.review_feedback.clone() {
            pane = pane.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .p(px(8.))
                    .rounded(px(6.))
                    .border_1()
                    .border_color(theme.border_subtle())
                    .bg(theme.surface_secondary())
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.fg_muted())
                            .child("REVIEW FEEDBACK"),
                    )
                    .child(div().text_sm().text_color(theme.fg()).child(feedback)),
            );
        }

        // Review form: one draft, four exits, beside the text they rule on.
        // "Request Changes" renders a needs_revision verdict — the text
        // travels with the spec to the re-scout. "Ask" routes the text (plus
        // task/spec context) into the orchestrator conversation for anything
        // that isn't a verdict yet: "is this already done?", "should we close
        // this?". Reject lives here, quieter than Approve — in practice you
        // ask before you reject.
        if let Some(spec_id) = &task.pending_spec {
            let has_text = !self.review_input.read(cx).content().trim().is_empty();
            pane = pane.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(
                        div()
                            .h(px(72.))
                            .p(px(4.))
                            .rounded(px(6.))
                            .border_1()
                            .border_color(theme.border_secondary())
                            .bg(theme.bg())
                            .text_sm()
                            .child(text_area(&self.review_input, cx).size_full()),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .items_center()
                            .gap(px(6.))
                            .child(self.form_button(
                                "approve-spec-brief",
                                "Approve",
                                gpui::hsla(135. / 360., 0.55, 0.45, 1.),
                                true,
                                cx.listener({
                                    let id = spec_id.clone();
                                    move |this, _: &ClickEvent, _window, cx| {
                                        let feedback = this.take_review_draft(cx);
                                        let id = id.clone();
                                        this.app_state.update(cx, |state, cx| {
                                            state.review_spec(
                                                id,
                                                SpecQueueStatus::Approved,
                                                feedback,
                                                cx,
                                            )
                                        });
                                    }
                                }),
                                cx,
                            ))
                            .child(self.form_button(
                                "request-changes",
                                "Request Changes",
                                gpui::hsla(35. / 360., 0.80, 0.55, 1.),
                                has_text,
                                cx.listener({
                                    let id = spec_id.clone();
                                    move |this, _: &ClickEvent, _window, cx| {
                                        let Some(text) = this.take_review_draft(cx) else {
                                            return;
                                        };
                                        let id = id.clone();
                                        this.app_state.update(cx, |state, cx| {
                                            state.review_spec(
                                                id,
                                                SpecQueueStatus::NeedsRevision,
                                                Some(text),
                                                cx,
                                            )
                                        });
                                    }
                                }),
                                cx,
                            ))
                            .child(self.form_button(
                                "ask-orchestrator",
                                "Ask Orchestrator",
                                theme.fg(),
                                has_text,
                                cx.listener(|this, _: &ClickEvent, _window, cx| {
                                    this.ask_about_selected_spec(cx);
                                }),
                                cx,
                            ))
                            .child(div().flex_1())
                            .child(self.form_button(
                                "reject-spec",
                                "Reject",
                                gpui::hsla(0., 0.75, 0.55, 1.),
                                true,
                                cx.listener({
                                    let id = spec_id.clone();
                                    move |this, _: &ClickEvent, _window, cx| {
                                        let feedback = this.take_review_draft(cx);
                                        let id = id.clone();
                                        this.app_state.update(cx, |state, cx| {
                                            state.review_spec(
                                                id,
                                                SpecQueueStatus::Rejected,
                                                feedback,
                                                cx,
                                            )
                                        });
                                    }
                                }),
                                cx,
                            )),
                    ),
            );
        }

        // The spec itself — markdown at the source (agent output), rendered
        // through the shared cache. Shown whatever the queue has decided:
        // it is the best description of the work there is.
        let entity = self
            .markdown_cache()
            .entity(format!("spec:{spec_id}"), &content, cx);
        pane = pane.child(
            div()
                .p(px(8.))
                .rounded(px(6.))
                .bg(theme.bg())
                .text_sm()
                .text_color(theme.fg())
                .child(crate::components::markdown_block(&entity, cx)),
        );

        Self::tab_pane("task-brief-scroll")
            .child(pane)
            .into_any_element()
    }

    /// The sentence a tab shows when history points at a task the working
    /// set no longer holds.
    fn render_missing_task(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        div()
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .p(px(16.))
            .text_sm()
            .text_color(theme.fg_muted())
            .child("That task is no longer in the working set.")
            .into_any_element()
    }

    /// The "recovered implementation" block: a build finished this task and
    /// its branch could not be pushed, so its commits are a file on the server
    /// host and nowhere else.
    ///
    /// **Recovery does not happen in this app, and the block does not pretend
    /// otherwise.** The file is on the server's disk and the `git fetch` runs
    /// in whatever checkout the human works in, so the command is shown in
    /// full and copyable rather than hidden behind a Recover button that would
    /// have to lie about where it ran. `base_sha` is stated beside it because
    /// the bundle is thin: it carries the build's commits and not the commit
    /// they grew from, so the fetch only reconstructs the branch in a
    /// repository that already has that base.
    ///
    /// Delete arms on the first click and fires on the second. There is no
    /// undo and no second copy — [`Workspace::bundle_delete_armed`].
    pub(crate) fn render_bundle(
        &self,
        bundle: RejectedBundle,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let armed = self.bundle_delete_armed.as_ref() == Some(&bundle.build_id);
        let build_id: BuildId = bundle.build_id.clone();

        let mut block = div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .p(px(8.))
            .rounded(px(6.))
            .border_1()
            .border_color(BUNDLE_ACCENT.opacity(0.5))
            .bg(BUNDLE_ACCENT.opacity(0.08))
            .child(
                div()
                    .text_xs()
                    .text_color(BUNDLE_ACCENT)
                    .child("RECOVERED IMPLEMENTATION — this build's branch never landed"),
            )
            .child(div().text_xs().text_color(theme.fg_muted()).child(format!(
                "{} · {} old · branch {}",
                byte_size(bundle.bytes),
                time::relative(bundle.created_at),
                bundle.branch,
            )));

        if let Some(reason) = bundle.exit_reason.clone() {
            block = block.child(div().text_xs().text_color(theme.fg()).child(reason));
        }

        block = block
            .child(div().text_xs().text_color(theme.fg_muted()).child(
                match bundle.base_sha.clone() {
                    // The bundle's prerequisite, said rather than implied:
                    // the fetch fails in a repository that does not have
                    // this commit, and that is not obvious from the error.
                    Some(base) => format!("Run in a checkout that has {base}:"),
                    None => "Run in a checkout that has this build's base commit:".to_string(),
                },
            ))
            .child(
                div()
                    .p(px(6.))
                    .rounded(px(5.))
                    .bg(theme.bg())
                    .text_xs()
                    .text_color(theme.fg())
                    .child(bundle.recovery_command.clone()),
            );

        let delete_label = if armed {
            "Really delete — this is the only copy"
        } else {
            "Delete"
        };
        block
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap(px(6.))
                    .child(self.action_button(
                        "copy-recovery-command",
                        "Copy Command",
                        None,
                        {
                            let command = bundle.recovery_command.clone();
                            move |_: &ClickEvent, _window, cx: &mut gpui::App| {
                                cx.write_to_clipboard(ClipboardItem::new_string(command.clone()))
                            }
                        },
                        cx,
                    ))
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("delete-bundle")
                            .px(px(8.))
                            .py(px(3.))
                            .rounded(px(5.))
                            .border_1()
                            .border_color(theme.border_secondary())
                            .cursor_pointer()
                            .text_xs()
                            .text_color(if armed {
                                gpui::hsla(0., 0.75, 0.55, 1.)
                            } else {
                                theme.fg_muted()
                            })
                            .hover({
                                let hover_bg = theme.surface_secondary();
                                move |el| el.bg(hover_bg)
                            })
                            .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                                if this.bundle_delete_armed.as_ref() == Some(&build_id) {
                                    this.bundle_delete_armed = None;
                                    let id = build_id.clone();
                                    this.app_state
                                        .update(cx, |state, cx| state.delete_bundle(id, cx));
                                } else {
                                    this.bundle_delete_armed = Some(build_id.clone());
                                }
                                cx.notify();
                            }))
                            .child(delete_label),
                    ),
            )
            .into_any_element()
    }

    /// The trimmed review draft, clearing the composer — `None` if empty.
    pub(crate) fn take_review_draft(&mut self, cx: &mut Context<Self>) -> Option<String> {
        let text = self.review_input.read(cx).content().trim().to_string();
        if text.is_empty() {
            return None;
        }
        self.review_input
            .update(cx, |input, cx| input.set_content("", cx));
        Some(text)
    }

    /// A submit button for the review form; renders inert and dimmed until
    /// the draft has text.
    fn form_button(
        &self,
        id: &'static str,
        label: &'static str,
        color: Hsla,
        enabled: bool,
        on_click: impl Fn(&ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let base = div()
            .id(id)
            .px(px(8.))
            .py(px(3.))
            .rounded(px(5.))
            .border_1()
            .border_color(theme.border_secondary())
            .text_xs();
        if enabled {
            base.text_color(color)
                .cursor_pointer()
                .hover({
                    let hover_bg = theme.surface_secondary();
                    move |el| el.bg(hover_bg)
                })
                .on_click(on_click)
                .child(label)
                .into_any_element()
        } else {
            base.text_color(theme.fg_muted())
                .opacity(0.5)
                .child(label)
                .into_any_element()
        }
    }

    fn action_button(
        &self,
        id: &'static str,
        label: &'static str,
        color: Option<Hsla>,
        on_click: impl Fn(&ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let text = color.unwrap_or_else(|| theme.fg());
        div()
            .id(id)
            .px(px(8.))
            .py(px(3.))
            .rounded(px(5.))
            .border_1()
            .border_color(theme.border_secondary())
            .cursor_pointer()
            .text_xs()
            .text_color(text)
            .hover({
                let hover_bg = theme.surface_secondary();
                move |el| el.bg(hover_bg)
            })
            .on_click(on_click)
            .child(label)
            .into_any_element()
    }
}
