//! The main window: sidebar of projects and threads, the selected thread's
//! transcript, a composer, and a status bar.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;

use gpui_kit::base::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use synara_protocol::ids::{MessageId, ProjectId, ThreadId};
use synara_protocol::orchestration::{MessageRole, ModelSelection};
use synara_session::SessionEvent;
use tokio::sync::mpsc;

use crate::backend::{Backend, BackendEvent};
use crate::model::{
    connection_label, plan_list_update, sidebar_rows, thread_banner, transcript_rows, ListUpdate,
    SidebarRow, TranscriptRow,
};

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
}

pub struct Workspace {
    backend: Backend,
    sidebar: Vec<SidebarRow>,
    selected: Option<ThreadId>,
    /// Shared with the list's render callback, which outlives `render`.
    transcript: Rc<RefCell<Vec<TranscriptRow>>>,
    transcript_ids: Vec<MessageId>,
    list_state: ListState,
    composer: Entity<TextareaState>,
    status: (String, bool),
    notice: Option<String>,
    working: bool,
    /// The selected thread's error banner.
    thread_error: Option<String>,
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
                        this.on_event(event);
                    }
                    cx.notify();
                });
                if applied.is_err() {
                    break;
                }
            }
        });

        let list_state = ListState::new(0, ListAlignment::Bottom, px(800.));
        list_state.set_follow_mode(FollowMode::Tail);

        Self {
            backend,
            sidebar: Vec::new(),
            selected: None,
            transcript: Rc::default(),
            transcript_ids: Vec::new(),
            list_state,
            composer,
            status: ("Starting…".into(), false),
            notice: None,
            working: false,
            thread_error: None,
            _subscriptions: vec![submit, quit],
            _pump: pump,
        }
    }

    fn on_event(&mut self, event: BackendEvent) {
        match event {
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
                    self.refresh_transcript(change.reset, &change.messages);
                }
            }
            BackendEvent::Session(SessionEvent::ThreadFailed { thread_id, error }) => {
                if self.selected.as_ref() == Some(&thread_id) {
                    self.notice = Some(format!("Thread stopped updating: {error}"));
                }
            }
        }
    }

    fn refresh_sidebar(&mut self) {
        self.sidebar = self
            .backend
            .with_session(|session| sidebar_rows(&session.state().shell))
            .unwrap_or_default();
    }

    fn refresh_transcript(&mut self, reset: bool, changed: &BTreeSet<MessageId>) {
        let Some(thread_id) = self.selected.clone() else {
            return;
        };
        let (rows, working, error) = self
            .backend
            .with_session(|session| {
                let state = session.state();
                state.threads.get(&thread_id).map(|store| {
                    (
                        transcript_rows(store),
                        store.is_working(),
                        store.thread().and_then(thread_banner),
                    )
                })
            })
            .flatten()
            .unwrap_or_default();
        self.thread_error = error;
        let ids: Vec<MessageId> = rows.iter().map(|row| row.id.clone()).collect();
        match plan_list_update(&self.transcript_ids, &ids, changed, reset) {
            ListUpdate::None => {}
            ListUpdate::Reset(count) => self.list_state.reset(count),
            ListUpdate::Splice(splices) => {
                for (range, count) in splices {
                    self.list_state.splice(range, count);
                }
            }
        }
        *self.transcript.borrow_mut() = rows;
        self.transcript_ids = ids;
        self.working = working;
    }

    fn select(&mut self, thread_id: ThreadId) {
        if self.selected.as_ref() == Some(&thread_id) {
            return;
        }
        self.backend.open_thread(&thread_id);
        self.selected = Some(thread_id);
        self.transcript_ids.clear();
        self.refresh_transcript(true, &BTreeSet::new());
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
                Some(row) => message_row(row).into_any_element(),
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

fn message_row(row: &TranscriptRow) -> impl IntoElement {
    let (label, background) = match row.role {
        MessageRole::User => ("You", Some(palette::USER_BG)),
        MessageRole::Assistant => ("Assistant", None),
        MessageRole::System => ("System", None),
    };
    let mut text = row.text.clone();
    if row.streaming && text.is_empty() {
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
                    .when(row.pending, |line| line.child("sending…"))
                    .when(row.streaming, |line| {
                        line.child(div().text_color(rgb(palette::ACCENT)).child("streaming"))
                    }),
            )
            .child(div().w_full().child(SharedString::from(text))),
    )
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_row()
            .bg(rgb(palette::BG))
            .text_color(rgb(palette::FG))
            .text_size(px(13.))
            .child(self.render_sidebar(cx))
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
