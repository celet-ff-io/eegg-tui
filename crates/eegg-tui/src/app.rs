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
    /// This session's sent rows: (content digest, was mailed) — a
    /// commit mails only when the target set is non-empty. The ordinal
    /// in this vector = the row's chain seq within the session.
    pub sent: Vec<(Vec<u8>, bool)>,
    /// Set between a commit request and its receipt (the next
    /// msg_added); the receipt is told from an arrival by this flag.
    pub awaiting_commit: bool,
    /// The per-counterpart receipt frontiers (the mail_acked stream).
    pub acked: Vec<Acked>,
    pub last_error: Option<FrameError>,
    /// One-shot notice (commit receipt / resource answer / write failure).
    pub flash: Option<String>,
    /// The core was handed a relay at spawn (its visit ran at
    /// startup; surviving spawn = the connect succeeded).
    pub relayed: bool,
    /// The core's stderr ring, viewable on demand (the diagnosis
    /// window — never rendered into the main surface).
    pub show_log: bool,
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

/// One counterpart's receipt frontier, as the core stated it.
/// `base` anchors the chain seq to THIS session: the first statement
/// for an anchor minus the commits already made at that moment (the
/// attach statements land before any request reply; a first live
/// statement on a fresh chain compensates to 0). `up_to` is the
/// running max (statements are unmerged and monotone per anchor).
/// One counterpart's receipt frontier, as the core stated it.
#[derive(Debug)]
pub struct Acked {
    anchor: Vec<u8>,
    base: u64,
    up_to: u64,
}

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
            sent: Vec::new(),
            awaiting_commit: false,
            acked: Vec::new(),
            last_error: None,
            flash: None,
            relayed: false,
            show_log: false,
            core_alive: true,
            core_exit: None,
            core_stderr: VecDeque::new(),
            outbox: Vec::new(),
            quit: false,
        }
    }

    /// The attach-time requests: the thread list, the peer set, and
    /// — RELAYED CORES ONLY — the core's own identity card (a
    /// stdio-only core has no mail identity; asking would only earn
    /// an error).
    pub fn session_open(&mut self, relayed: bool) {
        self.refresh_threads();
        self.refresh_peers();
        if relayed {
            self.refresh_self();
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
        self.awaiting_commit = true;
        self.outbox.push(IncomingFrame {
            method: Some(Method::MsgDraftCommit(MsgDraftCommit {})),
        });
        self.input.reset();
    }

    /// Whether the row with this digest is confirmed received: it was
    /// mailed, the target set is non-empty, and EVERY stated frontier
    /// covers the row's MAILED ordinal (a commit mails one mail to
    /// every set member; a chain seq counts mailed commits only).
    pub fn row_received(&self, digest: &[u8]) -> Option<bool> {
        let (_, mailed) = self.sent.iter().find(|(d, _)| d == digest)?;
        if !mailed || self.acked.is_empty() {
            return Some(false);
        }
        let seq = self
            .sent
            .iter()
            .filter(|(_, m)| *m)
            .position(|(d, _)| d == digest)
            .unwrap_or(0) as u64
            + 1;
        Some(
            self.acked
                .iter()
                .all(|a| a.up_to.saturating_sub(a.base) >= seq),
        )
    }

    /// How far the receipt frontiers reach into this session's mailed
    /// rows (for the status line): (covered, mailed).
    pub fn delivered(&self) -> (u64, usize) {
        let mailed = self.sent.iter().filter(|(_, m)| *m).count();
        let covered = self
            .acked
            .iter()
            .map(|a| a.up_to.saturating_sub(a.base))
            .min()
            .unwrap_or(0)
            .min(mailed as u64);
        (covered, mailed)
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
                self.peers_sel.select((n > 0).then_some((i + 1).min(n - 1)));
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
            // The core's stderr ring — the diagnosis window (a talky
            // core: --core "zconned -v").
            KeyCode::Char('l') => self.show_log = !self.show_log,
            // The peers popup: the set + the core's own card, one
            // refresh per open (cheap, and the set is small).
            KeyCode::Char('p') => {
                self.show_peers = true;
                self.peer_input_active = false;
                self.peers_sel.select((!self.peers.is_empty()).then_some(0));
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
                    // one statement; the awaiting flag tells them apart
                    // (set by the commit request; a rare arrival racing
                    // in between is misattributed — a day-one residual).
                    let digest = e.meta.as_ref().map(|m| m.digest.clone());
                    if self.awaiting_commit {
                        self.awaiting_commit = false;
                        if let Some(d) = digest.clone() {
                            self.sent.push((d, !self.peers.is_empty()));
                        }
                    }
                    // Dedup by digest guards against a re-announced
                    // row (a refetch, a replay). Routing: a row
                    // continuing the OPEN thread (its prev addresses
                    // the transcript's tip) appends live; anything
                    // else only refreshes the thread list.
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
                        } else if self.open_thread.is_none() {
                            // Nothing open: show the row and adopt its
                            // thread (a fresh commit's first prev IS
                            // the thread id — the genesis parent ref).
                            self.entries = vec![e.clone()];
                            self.open_thread = e.prev.first().cloned();
                            self.transcript_sel.select(Some(0));
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
            // The receipt frontier for one counterpart. Statements
            // are unmerged and monotone per anchor; the first one
            // anchors the session base. Self-records in the target
            // set state their own frontier — the same commit stream,
            // the same math, no special casing needed.
            Some(Out::MailAcked(m)) => {
                if m.up_to == 0 {
                    return;
                }
                match self.acked.iter_mut().find(|a| a.anchor == m.to_mailbox) {
                    Some(a) => a.up_to = a.up_to.max(m.up_to),
                    None => {
                        let mailed_so_far = self.sent.iter().filter(|(_, m)| *m).count() as u64;
                        self.acked.push(Acked {
                            base: m.up_to.saturating_sub(mailed_so_far),
                            anchor: m.to_mailbox,
                            up_to: m.up_to,
                        })
                    }
                }
            }
            Some(Out::Error(e)) => {
                // The known all-default decode: the core received a
                // verb it does not know — a stale core build.
                let mut e = e;
                if e.code == "internal" && e.message == "no op in frame" {
                    e.message = "stale core build — rebuild it: cargo build -p zconned --bin zconned"
                        .into();
                }
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
