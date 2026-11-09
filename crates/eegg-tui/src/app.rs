//! The view model: what the shell shows, and the lazygit-flavored
//! keymap that turns keys into wire requests. Core's data is always
//! the truth — deliveries update this model as they arrive; nothing
//! here re-implements core logic.

use std::collections::VecDeque;

use ratatui::widgets::ListState;
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler as _;
use zconned_material::{MsgRef, Value};
use zconned_stdio_types::{
    Entry, Error as FrameError, Fresh, IncomingFrame, MsgCursorSet, MsgDraftCommit,
    MsgDraftContentSet, MsgDraftGet, MsgThreadsGet, MsgThreadsLogGet,
    incoming_frame::Method,
    outgoing_frame::Frame as Out,
};

use crate::core::Core;
use crate::events::Evt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Threads,
    Transcript,
    Compose,
}

pub struct App {
    pub focus: Pane,
    pub threads: Vec<MsgRef>,
    pub thread_sel: ListState,
    pub entries: Vec<Entry>,
    pub transcript_sel: ListState,
    pub input: Input,
    /// The staged commit base (from msg_cursor_push; empty = not set).
    pub cursor: Vec<MsgRef>,
    /// The working draft (from msg_draft_push).
    pub staged: Option<(Option<Value>, Vec<Value>)>,
    pub last_error: Option<FrameError>,
    /// One-shot notice (commit receipt / resource answer / write failure).
    pub flash: Option<String>,
    pub core_alive: bool,
    /// Exit status once the core was reaped (kept for the exit code).
    pub core_exit: Option<std::process::ExitStatus>,
    pub core_stderr: VecDeque<String>,
    /// Requests accumulated by key handling; the main loop (the sole
    /// stdin writer) drains and writes them in order.
    pub outbox: Vec<IncomingFrame>,
    pub quit: bool,
}

const STDERR_TAIL: usize = 40;

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        Self {
            focus: Pane::Threads,
            threads: Vec::new(),
            thread_sel: ListState::default(),
            entries: Vec::new(),
            transcript_sel: ListState::default(),
            input: Input::default(),
            cursor: Vec::new(),
            staged: None,
            last_error: None,
            flash: None,
            core_alive: true,
            core_exit: None,
            core_stderr: VecDeque::new(),
            outbox: Vec::new(),
            quit: false,
        }
    }

    pub fn refresh_threads(&mut self) {
        self.outbox.push(IncomingFrame {
            method: Some(Method::MsgThreadsGet(MsgThreadsGet {})),
        });
    }

    fn refresh_draft(&mut self) {
        self.outbox.push(IncomingFrame {
            method: Some(Method::MsgDraftGet(MsgDraftGet {})),
        });
    }

    fn cycle_focus(&mut self) {
        self.focus = match self.focus {
            Pane::Threads => Pane::Transcript,
            Pane::Transcript => Pane::Compose,
            Pane::Compose => Pane::Threads,
        };
    }

    fn move_sel(&mut self, delta: isize) {
        let (list, len) = match self.focus {
            Pane::Threads => (&mut self.thread_sel, self.threads.len()),
            Pane::Transcript => (&mut self.transcript_sel, self.entries.len()),
            Pane::Compose => return,
        };
        let Some(i) = list.selected() else {
            if len > 0 {
                list.select(Some(0));
            }
            return;
        };
        let next = (i as isize + delta).clamp(0, len.saturating_sub(1) as isize) as usize;
        list.select(Some(next));
    }

    /// Enter on the threads pane: fetch the selected thread's log
    /// (reads are cursor-independent — browsing never moves the anchor).
    fn open_selected_thread(&mut self) {
        let thread = self
            .thread_sel
            .selected()
            .and_then(|i| self.threads.get(i).cloned());
        if let Some(t) = thread {
            self.entries.clear();
            self.transcript_sel.select(Some(0));
            self.outbox.push(IncomingFrame {
                method: Some(Method::MsgThreadsLogGet(MsgThreadsLogGet {
                    thread: Some(t),
                })),
            });
            self.focus = Pane::Transcript;
        }
    }

    /// Enter on the compose pane: the commit flow, one hand-typed
    /// frame per step (cursor → content → commit).
    fn compose_send(&mut self) {
        let text = self.input.value().trim().to_owned();
        if text.is_empty() {
            return;
        }
        if self.cursor.is_empty() {
            self.outbox.push(IncomingFrame {
                method: Some(Method::MsgCursorSet(MsgCursorSet {
                    at: Vec::new(),
                    fresh: Some(Fresh {}),
                })),
            });
        }
        self.outbox.push(IncomingFrame {
            method: Some(Method::MsgDraftContentSet(MsgDraftContentSet {
                kind: "text/plain".into(),
                data: text.into_bytes(),
            })),
        });
        self.outbox.push(IncomingFrame {
            method: Some(Method::MsgDraftCommit(MsgDraftCommit {})),
        });
        self.input.reset();
    }

    fn on_key(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        use ratatui::crossterm::event::{Event, KeyCode};

        // Dead-core posture: nothing but the way out.
        if !self.core_alive {
            if matches!(key.code, KeyCode::Char('q')) {
                self.quit = true;
            }
            return;
        }

        if self.focus == Pane::Compose {
            match key.code {
                KeyCode::Tab => self.cycle_focus(),
                KeyCode::Esc => self.input.reset(),
                KeyCode::Enter => self.compose_send(),
                _ => {
                    self.input.handle_event(&Event::Key(key));
                }
            }
            return;
        }

        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Tab => self.cycle_focus(),
            KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
            KeyCode::Home | KeyCode::Char('g') => match self.focus {
                Pane::Threads => self.thread_sel.select(Some(0)),
                Pane::Transcript => self.transcript_sel.select(Some(0)),
                Pane::Compose => {}
            },
            KeyCode::End | KeyCode::Char('G') => match self.focus {
                Pane::Threads => {
                    let n = self.threads.len();
                    self.thread_sel.select(if n > 0 { Some(n - 1) } else { None });
                }
                Pane::Transcript => {
                    let n = self.entries.len();
                    self.transcript_sel
                        .select(if n > 0 { Some(n - 1) } else { None });
                }
                Pane::Compose => {}
            },
            KeyCode::Char('t') if self.focus == Pane::Threads => self.refresh_threads(),
            KeyCode::Char('r') => self.refresh_draft(),
            KeyCode::Enter if self.focus == Pane::Threads => self.open_selected_thread(),
            _ => {}
        }
    }

    /// Frames arrive ANYTIME; each updates the view (B20: render what
    /// core reports).
    fn on_frame(&mut self, f: zconned_stdio_types::OutgoingFrame) {
        match f.frame {
            Some(Out::MsgAdded(a)) => {
                if let Some(e) = a.entry {
                    self.entries.push(e);
                    self.flash = Some("committed".into());
                }
            }
            Some(Out::MsgCursorPush(p)) => self.cursor = p.refs,
            Some(Out::MsgDraftPush(d)) => self.staged = Some((d.content, d.claims)),
            Some(Out::MsgThreadsPush(p)) => {
                self.threads = p.threads.into_iter().filter_map(|t| t.thread).collect();
                let n = self.threads.len();
                let sel = self.thread_sel.selected().map(|i| i.min(n.saturating_sub(1)));
                self.thread_sel.select(sel.filter(|_| n > 0));
            }
            Some(Out::MsgThreadsLogPush(p)) => {
                self.entries = p.entries;
                let n = self.entries.len();
                let sel = self
                    .transcript_sel
                    .selected()
                    .map(|i| i.min(n.saturating_sub(1)));
                self.transcript_sel.select(sel.filter(|_| n > 0));
            }
            Some(Out::ResourcePush(p)) => {
                self.flash = Some(match (p.r#ref.as_ref(), p.value) {
                    (Some(r), _) if r.digest.is_empty() => "resource: empty ref".into(),
                    (Some(r), Some(v)) => format!(
                        "resource {} = {} ({})",
                        short_hex(&r.digest),
                        preview_bytes(&v.data),
                        v.kind
                    ),
                    (Some(r), None) => {
                        format!("resource {} = (missing)", short_hex(&r.digest))
                    }
                    (None, _) => "resource: no ref".into(),
                });
            }
            Some(Out::Error(e)) => {
                self.last_error = Some(e);
                self.flash = None;
            }
            None => {} // all-default frame: acked, no death
        }
    }

    pub fn stream_broken(&mut self, message: String) {
        self.core_alive = false;
        self.flash = Some(message);
    }

    /// The main event pump: everything funnels through here.
    pub fn on_event(&mut self, evt: Evt, core: &mut Option<Core>) {
        match evt {
            Evt::Key(k) => {
                self.flash = None;
                self.on_key(k);
            }
            Evt::Resize => {} // each draw queries the real size
            Evt::Frame(f) => self.on_frame(f),
            Evt::CoreStderr(line) => {
                if self.core_stderr.len() >= STDERR_TAIL {
                    self.core_stderr.pop_front();
                }
                self.core_stderr.push_back(line);
            }
            Evt::CoreGone(broken) => {
                self.core_alive = false;
                self.flash = broken.as_ref().map(|e| e.to_string());
                // Reap promptly: stdin drop, then wait() — strictly in
                // that order.
                if let Some(c) = core.take() {
                    self.core_exit = c.shutdown().ok();
                }
            }
        }
    }
}

pub fn short_hex(b: &[u8]) -> String {
    b.iter().take(6).map(|x| format!("{x:02x}")).collect()
}

pub fn preview_bytes(data: &[u8]) -> String {
    let lossy = String::from_utf8_lossy(data).into_owned();
    let text = if lossy.chars().any(|c| c.is_control()) {
        format!("{:02x?}", &data[..data.len().min(8)])
    } else {
        lossy
    };
    let count = text.chars().count();
    if count > 48 {
        format!("{}\u{2026}", text.chars().take(48).collect::<String>())
    } else {
        text
    }
}
