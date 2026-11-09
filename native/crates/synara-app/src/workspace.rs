//! The main window: sidebar of projects and threads, the selected thread's
//! transcript, a composer, and a status bar.

use std::cell::RefCell;
use std::rc::Rc;

use gpui_kit::base::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::base::TextView;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use synara_protocol::commands::ApprovalDecision;
use synara_protocol::ids::{ProjectId, ThreadId};
use synara_protocol::orchestration::{MessageRole, ModelSelection};
use synara_session::SessionEvent;
use tokio::sync::mpsc;

use synara_protocol::server::ResolvedKeybinding;

use crate::backend::{Backend, BackendEvent};
use crate::keymap::{self, Command};
use crate::model::{
    connection_label, current_project, nth_thread, plan_list_update, sidebar_rows, step_thread,
    thread_banner, transcript_rows, ListUpdate, SidebarRow, TranscriptRow,
};
use crate::pending::{pending_approvals, PendingApproval};
use crate::worklog::{Tone, WorkEntry};

// One action per web command id this window handles; `keymap` maps the
// server's resolved keybindings onto them.
actions!(
    synara,
    [
        NewThread,
        ToggleSidebar,
        NextThread,
        PreviousThread,
        JumpThread1,
        JumpThread2,
        JumpThread3,
        JumpThread4,
        JumpThread5,
        JumpThread6,
        JumpThread7,
        JumpThread8,
        JumpThread9,
    ]
);

/// Key context the bindings are scoped to: the whole window.
const KEY_CONTEXT: &str = "Workspace";

fn command_action(command: Command) -> Box<dyn Action> {
    match command {
        Command::NewThread => Box::new(NewThread),
        Command::ToggleSidebar => Box::new(ToggleSidebar),
        Command::NextThread => Box::new(NextThread),
        Command::PreviousThread => Box::new(PreviousThread),
        Command::JumpThread(0) => Box::new(JumpThread1),
        Command::JumpThread(1) => Box::new(JumpThread2),
        Command::JumpThread(2) => Box::new(JumpThread3),
        Command::JumpThread(3) => Box::new(JumpThread4),
        Command::JumpThread(4) => Box::new(JumpThread5),
        Command::JumpThread(5) => Box::new(JumpThread6),
        Command::JumpThread(6) => Box::new(JumpThread7),
        Command::JumpThread(7) => Box::new(JumpThread8),
        Command::JumpThread(_) => Box::new(JumpThread9),
    }
}

/// Used when a project has no default model of its own.
const FALLBACK_PROVIDER: &str = "codex";
const FALLBACK_MODEL: &str = "gpt-5.5";

mod palette {
    pub const BG: u32 = 0x1b1c1f;
    pub const SIDEBAR: u32 = 0x151618;
    pub const BORDER: u32 = 0x2c2e33;
    pub const FG: u32 = 0xe6e6e6;
    pub const MUTED: u32 = 0x8b8f98;
    pub const SELECTED: u32 = 0x2a2d33;
    pub const USER_BG: u32 = 0x24272d;
    pub const ACCENT: u32 = 0x6ea8fe;
    pub const WARN: u32 = 0xf0a35e;
    pub const ERROR: u32 = 0xf07178;
}

pub struct Workspace {
    backend: Backend,
    sidebar: Vec<SidebarRow>,
    selected: Option<ThreadId>,
    /// Shared with the list's render callback, which outlives `render`.
    transcript: Rc<RefCell<Vec<TranscriptRow>>>,
    list_state: ListState,
    composer: Entity<TextareaState>,
    status: (String, bool),
    notice: Option<String>,
    working: bool,
    /// The selected thread's error banner.
    thread_error: Option<String>,
    /// Open approvals for the selected thread, oldest first.
    approvals: Vec<PendingApproval>,
    /// Request ids answered from here and not yet settled by the server.
    answered: Vec<String>,
    focus: FocusHandle,
    sidebar_visible: bool,
    /// GPUI cannot remove bindings, so the first config wins until restart.
    keys_installed: bool,
    _subscriptions: Vec<Subscription>,
    _pump: Task<()>,
}

impl Workspace {
    pub fn new(
        backend: Backend,
        mut events: mpsc::UnboundedReceiver<BackendEvent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let composer = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 8)
                .placeholder("Message the agent. Enter sends, Shift+Enter adds a line.")
        });
        composer.update(cx, |state, cx| state.set_submit_on_enter(true, cx));
        let submit = cx.subscribe_in(
            &composer,
            window,
            |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { shift: false, .. } = event {
                    this.submit(window, cx);
                }
            },
        );
        let quit = cx.on_app_quit(|this, _| {
            this.backend.shutdown();
            async {}
        });

        // Events arrive in bursts while a turn streams; apply a burst, then
        // render once.
        let pump = cx.spawn(async move |this, cx| {
            while let Some(first) = events.recv().await {
                let mut batch = vec![first];
                while let Ok(next) = events.try_recv() {
                    batch.push(next);
                }
                let applied = this.update(cx, |this, cx| {
                    for event in batch {
                        this.on_event(event, cx);
                    }
                    cx.notify();
                });
                if applied.is_err() {
                    break;
                }
            }
        });

        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let list_state = ListState::new(0, ListAlignment::Bottom, px(800.));
        list_state.set_follow_mode(FollowMode::Tail);

        Self {
            backend,
            sidebar: Vec::new(),
            selected: None,
            transcript: Rc::default(),
            list_state,
            composer,
            status: ("Starting…".into(), false),
            notice: None,
            working: false,
            thread_error: None,
            approvals: Vec::new(),
            answered: Vec::new(),
            focus,
            sidebar_visible: true,
            keys_installed: false,
            _subscriptions: vec![submit, quit],
            _pump: pump,
        }
    }

    fn on_event(&mut self, event: BackendEvent, cx: &mut Context<Self>) {
        match event {
            BackendEvent::Keybindings(rules) => self.install_keys(&rules, cx),
            BackendEvent::Progress(text) => self.status = (text, false),
            BackendEvent::SessionStarted => self.refresh_sidebar(),
            BackendEvent::Notice(text) => self.notice = Some(text),
            BackendEvent::ThreadCreated(thread_id) => self.select(thread_id),
            BackendEvent::Session(SessionEvent::Connection(state)) => {
                self.status = connection_label(&state);
            }
            BackendEvent::Session(SessionEvent::Shell(_)) => self.refresh_sidebar(),
            BackendEvent::Session(SessionEvent::Thread { thread_id, change }) => {
                if self.selected.as_ref() == Some(&thread_id) {
                    self.refresh_transcript(change.reset);
                }
            }
            BackendEvent::Session(SessionEvent::ThreadFailed { thread_id, error }) => {
                if self.selected.as_ref() == Some(&thread_id) {
                    self.notice = Some(format!("Thread stopped updating: {error}"));
                }
            }
        }
    }

    fn install_keys(&mut self, rules: &[ResolvedKeybinding], cx: &mut Context<Self>) {
        if self.keys_installed {
            return;
        }
        let context = KeyBindingContextPredicate::parse(KEY_CONTEXT)
            .ok()
            .map(Rc::new);
        let mut installed = Vec::new();
        for (keys, command) in keymap::bindings(rules, cfg!(target_os = "macos")) {
            match KeyBinding::load(
                &keys,
                command_action(command),
                context.clone(),
                false,
                None,
                cx,
            ) {
                Ok(binding) => installed.push(binding),
                Err(error) => tracing::warn!(%keys, ?error, "skipping keybinding"),
            }
        }
        tracing::info!(count = installed.len(), "installed keybindings");
        cx.bind_keys(installed);
        self.keys_installed = true;
    }

    fn jump(&mut self, n: usize, cx: &mut Context<Self>) {
        if let Some(thread_id) = nth_thread(&self.sidebar, n) {
            self.select(thread_id);
            cx.notify();
        }
    }

    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(thread_id) = step_thread(&self.sidebar, self.selected.as_ref(), delta) {
            self.select(thread_id);
            cx.notify();
        }
    }

    fn new_thread_here(&mut self, cx: &mut Context<Self>) {
        match current_project(&self.sidebar, self.selected.as_ref()) {
            Some(project_id) => self.new_thread(project_id, cx),
            None => {
                self.notice = Some("Add a project before starting a thread".into());
                cx.notify();
            }
        }
    }

    fn refresh_sidebar(&mut self) {
        self.sidebar = self
            .backend
            .with_session(|session| sidebar_rows(&session.state().shell))
            .unwrap_or_default();
    }

    fn refresh_transcript(&mut self, reset: bool) {
        let Some(thread_id) = self.selected.clone() else {
            return;
        };
        let (rows, working, error, approvals) = self
            .backend
            .with_session(|session| {
                let state = session.state();
                state.threads.get(&thread_id).map(|store| {
                    (
                        transcript_rows(store),
                        store.is_working(),
                        store.thread().and_then(thread_banner),
                        store.thread().map(pending_approvals).unwrap_or_default(),
                    )
                })
            })
            .flatten()
            .unwrap_or_default();
        self.thread_error = error;
        self.answered
            .retain(|id| approvals.iter().any(|a| &a.request_id == id));
        self.approvals = approvals;
        let update = plan_list_update(&self.transcript.borrow(), &rows, reset);
        match update {
            ListUpdate::None => {}
            ListUpdate::Reset(count) => self.list_state.reset(count),
            ListUpdate::Splice(splices) => {
                for (range, count) in splices {
                    self.list_state.splice(range, count);
                }
            }
        }
        *self.transcript.borrow_mut() = rows;
        self.working = working;
    }

    fn select(&mut self, thread_id: ThreadId) {
        if self.selected.as_ref() == Some(&thread_id) {
            return;
        }
        self.backend.open_thread(&thread_id);
        self.selected = Some(thread_id);
        self.transcript.borrow_mut().clear();
        self.refresh_transcript(true);
        self.notice = None;
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.composer.read(cx).value().trim().to_owned();
        if text.is_empty() {
            return;
        }
        let Some(thread_id) = self.selected.clone() else {
            self.notice = Some("Pick or create a thread first".into());
            cx.notify();
            return;
        };
        self.backend.send_message(thread_id, text);
        self.composer
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.notice = None;
        cx.notify();
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(thread_id) = self.selected.clone() {
            self.backend.interrupt(thread_id);
        }
        cx.notify();
    }

    fn new_thread(&mut self, project_id: ProjectId, cx: &mut Context<Self>) {
        let model = self
            .backend
            .with_session(|session| {
                session
                    .state()
                    .shell
                    .project(&project_id)
                    .and_then(|project| project.default_model_selection.clone())
            })
            .flatten()
            .unwrap_or_else(|| ModelSelection::new(FALLBACK_PROVIDER, FALLBACK_MODEL));
        self.backend.create_thread(project_id, model);
        cx.notify();
    }

    fn selected_title(&self) -> Option<String> {
        let selected = self.selected.as_ref()?;
        self.sidebar.iter().find_map(|row| match row {
            SidebarRow::Thread { id, title, .. } if id == selected => Some(title.clone()),
            _ => None,
        })
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.sidebar.iter().map(|row| match row {
            SidebarRow::Project { id, title } => {
                let project_id = id.clone();
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .pt_3()
                    .pb_1()
                    .text_color(rgb(palette::MUTED))
                    .child(div().truncate().child(SharedString::from(title.clone())))
                    .child(
                        div()
                            .id(ElementId::from(format!("new-thread-{id}")))
                            .px_1()
                            .rounded_sm()
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(palette::SELECTED)))
                            .child("+")
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.new_thread(project_id.clone(), cx);
                            })),
                    )
                    .into_any_element()
            }
            SidebarRow::Thread {
                id,
                title,
                pinned,
                working,
                needs_attention,
                ..
            } => {
                let thread_id = id.clone();
                let selected = self.selected.as_ref() == Some(id);
                let marker = if *needs_attention {
                    Some(("●", palette::WARN))
                } else if *working {
                    Some(("●", palette::ACCENT))
                } else {
                    None
                };
                div()
                    .id(ElementId::from(format!("thread-{id}")))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .mx_1()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .when(selected, |row| row.bg(rgb(palette::SELECTED)))
                    .hover(|style| style.bg(rgb(palette::SELECTED)))
                    .child(
                        div()
                            .flex_1()
                            .truncate()
                            .child(SharedString::from(title.clone())),
                    )
                    .when(*pinned, |row| {
                        row.child(div().text_color(rgb(palette::MUTED)).child("pinned"))
                    })
                    .when_some(marker, |row, (glyph, color)| {
                        row.child(div().text_color(rgb(color)).child(glyph))
                    })
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.select(thread_id.clone());
                        cx.notify();
                    }))
                    .into_any_element()
            }
        });
        div()
            .id("sidebar")
            .flex_none()
            .w(px(260.))
            .h_full()
            .overflow_y_scroll()
            .bg(rgb(palette::SIDEBAR))
            .border_r_1()
            .border_color(rgb(palette::BORDER))
            .pb_3()
            .when(self.sidebar.is_empty(), |list| {
                list.child(
                    div()
                        .p_3()
                        .text_color(rgb(palette::MUTED))
                        .child("No projects yet"),
                )
            })
            .children(rows)
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let title = self
            .selected_title()
            .unwrap_or_else(|| "No thread selected".into());
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .h(px(40.))
            .px_4()
            .border_b_1()
            .border_color(rgb(palette::BORDER))
            .child(div().truncate().child(SharedString::from(title)))
            .when(self.working, |header| {
                header.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_3()
                        .child(div().text_color(rgb(palette::ACCENT)).child("Working…"))
                        .child(self.stop_button(cx)),
                )
            })
    }

    fn respond(
        &mut self,
        approval: &PendingApproval,
        decision: ApprovalDecision,
        cx: &mut Context<Self>,
    ) {
        let Some(thread_id) = self.selected.clone() else {
            return;
        };
        if self.answered.contains(&approval.request_id) {
            return;
        }
        self.answered.push(approval.request_id.clone());
        self.backend.respond_approval(
            thread_id,
            approval.request_id.clone(),
            approval.lifecycle_generation.clone(),
            decision,
        );
        cx.notify();
    }

    /// The oldest open approval above the composer, as on the web
    /// (`ComposerPendingApprovalPanel`): prompt, request detail, actions.
    fn render_approval(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let approval = self.approvals.first()?.clone();
        let answered = self.answered.contains(&approval.request_id);
        let count = self.approvals.len();
        let mut actions = div().flex().flex_row().gap_2();
        for (ix, (decision, label)) in approval.actions().into_iter().enumerate() {
            let target = approval.clone();
            let primary = decision == ApprovalDecision::Accept;
            actions = actions.child(
                div()
                    .id(("approval-action", ix))
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(if primary {
                        palette::ACCENT
                    } else {
                        palette::BORDER
                    }))
                    .when(primary, |button| button.text_color(rgb(palette::ACCENT)))
                    .when(answered, |button| button.opacity(0.5))
                    .when(!answered, |button| {
                        button
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(palette::SELECTED)))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.respond(&target, decision, cx)
                            }))
                    })
                    .child(label),
            );
        }
        Some(
            div()
                .mx_3()
                .mb_2()
                .p_3()
                .flex()
                .flex_col()
                .gap_2()
                .rounded_md()
                .border_1()
                .border_color(rgb(palette::ACCENT))
                .bg(rgb(palette::USER_BG))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .justify_between()
                        .child(div().child(approval.prompt()))
                        .when(count > 1, |row| {
                            row.child(
                                div()
                                    .text_color(rgb(palette::MUTED))
                                    .child(SharedString::from(format!("1 of {count}"))),
                            )
                        }),
                )
                .when_some(approval.detail.clone(), |card, detail| {
                    card.child(
                        div()
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .bg(rgb(palette::BG))
                            .font_family("monospace")
                            .text_color(rgb(palette::MUTED))
                            .child(SharedString::from(detail)),
                    )
                })
                .child(if answered {
                    div()
                        .text_color(rgb(palette::MUTED))
                        .child("Sending response…")
                        .into_any_element()
                } else {
                    actions.into_any_element()
                })
                .into_any_element(),
        )
    }

    fn stop_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("stop-turn")
            .px_2()
            .py_0p5()
            .rounded_md()
            .border_1()
            .border_color(rgb(palette::BORDER))
            .cursor_pointer()
            .hover(|style| style.bg(rgb(palette::SELECTED)))
            .child("Stop")
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.stop(cx)))
    }

    fn render_transcript(&self) -> AnyElement {
        if self.selected.is_none() {
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(rgb(palette::MUTED))
                .child("Pick a thread, or press + next to a project to start one")
                .into_any_element();
        }
        let rows = self.transcript.clone();
        list(self.list_state.clone(), move |ix, _, _| {
            let rows = rows.borrow();
            match rows.get(ix) {
                Some(TranscriptRow::Message {
                    id,
                    role,
                    text,
                    streaming,
                    pending,
                }) => {
                    message_row(id.as_str(), *role, text, *streaming, *pending).into_any_element()
                }
                Some(TranscriptRow::Work(entry)) => work_row(entry).into_any_element(),
                None => div().into_any_element(),
            }
        })
        .flex_1()
        .w_full()
        .into_any_element()
    }

    fn render_status(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (text, problem) = &self.status;
        div()
            .flex()
            .flex_row()
            .gap_3()
            .h(px(24.))
            .px_3()
            .items_center()
            .border_t_1()
            .border_color(rgb(palette::BORDER))
            .text_color(rgb(if *problem {
                palette::WARN
            } else {
                palette::MUTED
            }))
            .child(SharedString::from(text.clone()))
            .when(*problem, |bar| {
                bar.child(
                    div()
                        .id("reconnect")
                        .text_color(rgb(palette::ACCENT))
                        .cursor_pointer()
                        .child("Reconnect")
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.backend.reconnect();
                            cx.notify();
                        })),
                )
            })
            .when_some(self.notice.clone(), |bar, notice| {
                bar.child(
                    div()
                        .truncate()
                        .text_color(rgb(palette::WARN))
                        .child(SharedString::from(notice)),
                )
            })
    }
}

fn message_row(
    id: &str,
    role: MessageRole,
    text: &str,
    streaming: bool,
    pending: bool,
) -> impl IntoElement {
    let (label, background) = match role {
        MessageRole::User => ("You", Some(palette::USER_BG)),
        MessageRole::Assistant => ("Assistant", None),
        MessageRole::System => ("System", None),
    };
    let mut text = text.to_owned();
    if streaming && text.is_empty() {
        text = "…".into();
    }
    div().w_full().px_4().py_1().child(
        div()
            .flex()
            .flex_col()
            .gap_1()
            .px_3()
            .py_2()
            .rounded_lg()
            .when_some(background, |bubble, color| bubble.bg(rgb(color)))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .text_color(rgb(palette::MUTED))
                    .child(label)
                    .when(pending, |line| line.child("sending…"))
                    .when(streaming, |line| {
                        line.child(div().text_color(rgb(palette::ACCENT)).child("streaming"))
                    }),
            )
            .child(message_body(id, role, text)),
    )
}

/// Assistant text is Markdown (code blocks, lists, tables, links); what the
/// user typed shows as written.
fn message_body(id: &str, role: MessageRole, text: String) -> AnyElement {
    match role {
        MessageRole::Assistant => div()
            .w_full()
            .child(TextView::markdown(
                SharedString::from(format!("md:{id}")),
                text,
            ))
            .into_any_element(),
        MessageRole::User | MessageRole::System => div()
            .w_full()
            .child(SharedString::from(text))
            .into_any_element(),
    }
}

/// One work-log line: a tone dot, the label, and a muted detail. Failure
/// rows add the interruption sentence in the warning colour.
fn work_row(entry: &WorkEntry) -> impl IntoElement {
    let dot = match entry.tone {
        Tone::Error => palette::ERROR,
        Tone::Tool => palette::ACCENT,
        Tone::Thinking | Tone::Info => palette::MUTED,
    };
    div().w_full().px_4().child(
        div()
            .flex()
            .flex_col()
            .px_3()
            .py_0p5()
            .text_sm()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(div().size(px(6.)).rounded_full().bg(rgb(dot)))
                    .child(
                        div()
                            .text_color(rgb(if entry.tone == Tone::Error {
                                palette::ERROR
                            } else {
                                palette::MUTED
                            }))
                            .child(SharedString::from(entry.label.clone())),
                    ),
            )
            .when_some(entry.detail.clone(), |row, detail| {
                row.child(
                    div()
                        .pl_4()
                        .text_color(rgb(palette::MUTED))
                        .truncate()
                        .child(SharedString::from(detail)),
                )
            })
            .when_some(entry.failure.clone(), |row, failure| {
                row.child(
                    div()
                        .pl_4()
                        .text_color(rgb(palette::WARN))
                        .child(SharedString::from(failure)),
                )
            }),
    )
}

fn on_jump<A: Action>(root: Div, n: usize, cx: &mut Context<Workspace>) -> Div {
    root.on_action(cx.listener(move |this, _: &A, _, cx| this.jump(n, cx)))
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let root = div()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &NewThread, _, cx| this.new_thread_here(cx)))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| {
                this.sidebar_visible = !this.sidebar_visible;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &NextThread, _, cx| this.step(1, cx)))
            .on_action(cx.listener(|this, _: &PreviousThread, _, cx| this.step(-1, cx)));
        let root = on_jump::<JumpThread1>(root, 0, cx);
        let root = on_jump::<JumpThread2>(root, 1, cx);
        let root = on_jump::<JumpThread3>(root, 2, cx);
        let root = on_jump::<JumpThread4>(root, 3, cx);
        let root = on_jump::<JumpThread5>(root, 4, cx);
        let root = on_jump::<JumpThread6>(root, 5, cx);
        let root = on_jump::<JumpThread7>(root, 6, cx);
        let root = on_jump::<JumpThread8>(root, 7, cx);
        let root = on_jump::<JumpThread9>(root, 8, cx);
        root.size_full()
            .flex()
            .flex_row()
            .bg(rgb(palette::BG))
            .text_color(rgb(palette::FG))
            .text_size(px(13.))
            .when(self.sidebar_visible, |root| {
                root.child(self.render_sidebar(cx))
            })
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .flex()
                    .flex_col()
                    .min_w_0()
                    .child(self.render_header(cx))
                    .child(self.render_transcript())
                    .when_some(self.thread_error.clone(), |column, error| {
                        column.child(
                            div()
                                .mx_3()
                                .mb_2()
                                .px_3()
                                .py_2()
                                .rounded_md()
                                .border_1()
                                .border_color(rgb(palette::WARN))
                                .text_color(rgb(palette::WARN))
                                .child(SharedString::from(error)),
                        )
                    })
                    .when_some(self.render_approval(cx), |column, card| column.child(card))
                    .child(
                        div()
                            .p_3()
                            .border_t_1()
                            .border_color(rgb(palette::BORDER))
                            .child(Textarea::new(&self.composer)),
                    )
                    .child(self.render_status(cx)),
            )
    }
}
