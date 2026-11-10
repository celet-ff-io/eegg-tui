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
    MsgDraftContentSet, MsgDraftGet, MsgThreadsGet, MsgThreadsLogGet, Peer, PeerSet, PeersGet,
    SelfGet, incoming_frame::Method, outgoing_frame::Frame as Out,
};

use crate::core::Process;
use crate::events::Evt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Threads,
    Transcript,
    Compose,
}

#[derive(Debug)]
pub struct App {
    pub focus: Pane,
    pub threads: Vec<MsgRef>,
    pub thread_sel: ListState,
    pub entries: Vec<Entry>,
    pub transcript_sel: ListState,
    /// The thread whose log the transcript currently shows (the reply
    /// base and the arrival-routing hint).
    pub open_thread: Option<MsgRef>,
    pub input: Input,
    /// The staged commit base (from msg_cursor_push; empty = not set).
    pub cursor: Vec<MsgRef>,
    /// Set by the new-thread key: the next send starts a FRESH chain
    /// even though a thread transcript is open.
    pub new_thread_intent: bool,
    /// The working draft (from msg_draft_push).
    pub staged: Option<(Option<Value>, Vec<Value>)>,
    /// The send-target set (the whole resting set, replaced on every
    /// delivery) and the core's own identity card (verbatim — the
    /// copy source for the counterpart's paste-in).
    pub peers: Vec<Peer>,
    pub self_card: Option<String>,
    pub show_peers: bool,
    pub peers_sel: ListState,
    pub peer_input: Input,
    pub peer_input_active: bool,
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
            open_thread: None,
            input: Input::default(),
            cursor: Vec::new(),
            new_thread_intent: false,
            staged: None,
            peers: Vec::new(),
            self_card: None,
            show_peers: false,
            peers_sel: ListState::default(),
            peer_input: Input::default(),
            peer_input_active: false,
            last_error: None,
            flash: None,
            core_alive: true,
            core_exit: None,
            core_stderr: VecDeque::new(),
            outbox: Vec::new(),
            quit: false,
        }
    }

    /// The attach-time requests: the thread list, the peer set, and
    /// the core's own identity card.
    pub fn session_open(&mut self) {
        self.refresh_threads();
        self.refresh_peers();
        self.refresh_self();
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

    fn refresh_peers(&mut self) {
        self.outbox.push(IncomingFrame {
            method: Some(Method::PeersGet(PeersGet {})),
        });
    }

    fn refresh_self(&mut self) {
        self.outbox.push(IncomingFrame {
            method: Some(Method::SelfGet(SelfGet {})),
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
            self.open_thread = Some(t.clone());
            self.outbox.push(IncomingFrame {
                method: Some(Method::MsgThreadsLogGet(MsgThreadsLogGet {
                    thread: Some(t),
                })),
            });
            self.focus = Pane::Transcript;
        }
    }

    /// The thread continuation base for a reply: the transcript's
    /// last seen entry, addressed by its digest (the message ref).
    fn open_thread_tip(&self) -> Option<MsgRef> {
        let last = self.entries.last()?;
        let meta = last.meta.as_ref()?;
        Some(MsgRef {
            ver_major: 1,
            ver_minor: 0,
            algo: meta.algo.clone(),
            digest: meta.digest.clone(),
        })
    }

    /// Enter on the compose pane: the commit flow, one hand-typed
    /// frame per step (cursor → content → commit). An unset cursor
    /// replies to the OPEN thread (at = its tip); with no open thread
    /// (or a new-thread intent) it starts a fresh chain.
    fn compose_send(&mut self) {
        let text = self.input.value().trim().to_owned();
        if text.is_empty() {
            return;
        }
        if self.cursor.is_empty() {
            let reply_base = if self.new_thread_intent {
                None
            } else {
                self.open_thread_tip()
            };
            match reply_base {
                Some(tip) => {
                    self.outbox.push(IncomingFrame {
                        method: Some(Method::MsgCursorSet(MsgCursorSet {
                            at: vec![tip],
                            fresh: None,
                        })),
                    });
                }
                None => {
                    self.outbox.push(IncomingFrame {
                        method: Some(Method::MsgCursorSet(MsgCursorSet {
                            at: Vec::new(),
                            fresh: Some(Fresh {}),
                        })),
                    });
                }
            }
        }
        self.new_thread_intent = false;
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

    /// Keys while the peers popup is up; `false` = the popup closed
    /// (the caller's key handling ends here).
    fn on_key_peers(&mut self, key: ratatui::crossterm::event::KeyEvent) -> bool {
        use ratatui::crossterm::event::{Event, KeyCode};
        match (self.peer_input_active, key.code) {
            // Typing the pasted card: Enter submits, Esc cancels the
            // input (a second Esc closes the popup).
            (true, KeyCode::Enter) => {
                let card = self.peer_input.value().trim().to_owned();
                if !card.is_empty() {
                    self.outbox.push(IncomingFrame {
                        method: Some(Method::PeerSet(PeerSet {
                            peer: None,
                            card: Some(card),
                        })),
                    });
                    self.peer_input.reset();
                    self.peer_input_active = false;
                    self.flash = Some("peer submitted".into());
                }
            }
            (true, KeyCode::Esc) => {
                self.peer_input.reset();
                self.peer_input_active = false;
            }
            (true, _) => {
                self.peer_input.handle_event(&Event::Key(key));
            }
            // Listing posture.
            (false, KeyCode::Char('a')) => self.peer_input_active = true,
            (false, KeyCode::Up | KeyCode::Char('k')) => {
                let n = self.peers.len();
                let i = self.peers_sel.selected().unwrap_or(0);
                self.peers_sel
                    .select((n > 0).then_some(i.saturating_sub(1)));
            }
            (false, KeyCode::Down | KeyCode::Char('j')) => {
                let n = self.peers.len();
                let i = self.peers_sel.selected().unwrap_or(0);
                self.peers_sel
                    .select((n > 0).then_some((i + 1).min(n - 1)));
            }
            (false, KeyCode::Esc | KeyCode::Char('p') | KeyCode::Char('q')) => {
                self.show_peers = false;
                return false;
            }
            (false, _) => {}
        }
        true
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

        if self.show_peers {
            self.on_key_peers(key);
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
                    self.thread_sel
                        .select(if n > 0 { Some(n - 1) } else { None });
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
            // New-thread compose: the next send starts a fresh chain
            // even though a thread transcript is open.
            KeyCode::Char('n') => {
                self.new_thread_intent = true;
                self.focus = Pane::Compose;
                self.flash = Some("new thread".into());
            }
            // The peers popup: the set + the core's own card, one
            // refresh per open (cheap, and the set is small).
            KeyCode::Char('p') => {
                self.show_peers = true;
                self.peer_input_active = false;
                self.peers_sel
                    .select((!self.peers.is_empty()).then_some(0));
                self.refresh_peers();
                self.refresh_self();
            }
            KeyCode::Enter if self.focus == Pane::Threads => self.open_selected_thread(),
            _ => {}
        }
    }

    /// Frames arrive ANYTIME; each updates the view — the interface
    /// renders what the core reports.
    fn on_frame(&mut self, f: zconned_stdio_types::OutgoingFrame) {
        match f.frame {
            Some(Out::MsgAdded(a)) => {
                if let Some(e) = a.entry {
                    // Commit receipts and network arrivals share this
                    // one statement; dedup by digest guards against a
                    // re-announced row (a refetch, a replay). Routing:
                    // a row continuing the OPEN thread (its prev
                    // addresses the transcript's tip) appends live;
                    // anything else only refreshes the thread list.
                    let digest = e.meta.as_ref().map(|m| m.digest.clone());
                    let unseen = digest.as_ref().is_none_or(|d| {
                        !self
                            .entries
                            .iter()
                            .any(|x| x.meta.as_ref().is_some_and(|m| m.digest == *d))
                    });
                    if unseen {
                        let continues = self
                            .open_thread_tip()
                            .map(|tip| e.prev.iter().any(|r| r.digest == tip.digest))
                            .unwrap_or(false);
                        if continues {
                            self.entries.push(e);
                            let n = self.entries.len();
                            self.transcript_sel.select(Some(n - 1));
                        }
                        self.refresh_threads();
                        self.flash = Some("msg arrived".into());
                    }
                }
            }
            Some(Out::MsgCursorPush(p)) => self.cursor = p.refs,
            Some(Out::MsgDraftPush(d)) => self.staged = Some((d.content, d.claims)),
            Some(Out::MsgThreadsPush(p)) => {
                self.threads = p.threads.into_iter().filter_map(|t| t.thread).collect();
                let n = self.threads.len();
                let sel = self
                    .thread_sel
                    .selected()
                    .map(|i| i.min(n.saturating_sub(1)));
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
            // The whole resting set — replaced wholesale, never
            // merged (the core's set is the truth).
            Some(Out::PeersPush(p)) => self.peers = p.peers,
            // The core's own identity card, displayed verbatim (the
            // paste source for the counterpart's address book).
            Some(Out::SelfPush(s)) => self.self_card = Some(s.card),
            // The received-frontier statement is not on the landed
            // face yet; the surface ignores it until the core
            // delivers it.
            Some(Out::MailAcked(_)) => {}
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
    pub fn on_event(&mut self, evt: Evt, core: &mut Option<Process>) {
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
