//! The view model: what the shell shows, and the lazygit-flavored
//! keymap that turns keys into wire requests. Core's data is always
//! the truth — deliveries update this model as they arrive; nothing
//! here re-implements core logic.

use std::collections::VecDeque;

use ratatui::widgets::ListState;
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler as _;
use zconned_material::{CLAIM_KIND_TS, MsgRef, Value};
use zconned_stdio_types::{
    Entry, Error as FrameError, Fresh, IncomingFrame, MsgCursorSet, MsgDraftCommit,
    MsgDraftContentSet, MsgDraftGet, MsgThreadsGet, MsgThreadsLogGet, Peer, PeerSet, PeersGet,
    SelfGet, incoming_frame::Method, outgoing_frame::Frame as Out,
};

use crate::clipboard;
use crate::core::Process;
use crate::events::Evt;

/// Which bottom box the peers popup shows: the pasted-card input,
/// the read-card-from-file path box, or the save-card-to-file path
/// box. `None` = the plain list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardFlow {
    None,
    InputCard,
    ReadCard,
    SaveCard,
}

/// The file-path box in the MAIN window (the compose/transcript
/// channels): what Enter does with the path. Shares the path input
/// with the peers card flows — the two are never active together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileBox {
    None,
    /// Write the compose text to the file.
    SaveCompose,
    /// Replace the compose text with the file's content.
    LoadCompose,
    /// Write the selected transcript row's text to the file.
    SaveTranscript,
}

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
    /// The lazygit-style help overlay (opened with `?`); it eats all
    /// keys until closed (Esc / ? / q).
    pub show_help: bool,
    /// The help overlay's scroll offset (j/k; a short terminal
    /// scrolls the card instead of clipping it).
    pub help_scroll: u16,
    pub peers_sel: ListState,
    pub peer_input: Input,
    /// The active bottom box in the peers popup.
    pub card_flow: CardFlow,
    /// The file-path box's text (for `read card from` / `save card
    /// to`).
    pub file_input: Input,
    /// The active MAIN-window file-path box.
    pub file_box: FileBox,
    /// This session's sent rows: (content digest, was mailed) — a
    /// commit mails only when the target set is non-empty. The ordinal
    /// in this vector = the row's chain seq within the session.
    pub sent: Vec<(Vec<u8>, bool)>,
    /// Set between a commit request and its receipt (the next
    /// msg_added); the receipt is told from an arrival by this flag.
    pub awaiting_commit: bool,
    /// The per-counterpart receipt frontiers (the mail_acked stream).
    pub acked: Vec<Acked>,
    /// The compose text waiting for the stamp row's receipt (the
    /// timestamp discipline queues the text while the stamp message
    /// commits first).
    pub pending_text: Option<String>,
    /// The last timestamp-stamp send. `None` or older than the stamp
    /// window = the next text send stamps first.
    pub ts_stamped_at: Option<jiff::Timestamp>,
    pub last_error: Option<FrameError>,
    /// One-shot notice (commit receipt / resource answer / write failure).
    pub flash: Option<String>,
    /// The core was handed a relay at spawn (its visit ran at
    /// startup; surviving spawn = the connect succeeded).
    pub relayed: bool,
    /// The core's stderr ring, viewable on demand (the diagnosis
    /// window — never rendered into the main surface).
    pub show_log: bool,
    /// The log popup's scroll offset (clamped at render time).
    pub log_scroll: usize,
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
            show_help: false,
            help_scroll: 0,
            peers_sel: ListState::default(),
            peer_input: Input::default(),
            card_flow: CardFlow::None,
            file_input: Input::default(),
            file_box: FileBox::None,
            sent: Vec::new(),
            pending_text: None,
            ts_stamped_at: None,
            awaiting_commit: false,
            acked: Vec::new(),
            last_error: None,
            flash: None,
            relayed: false,
            show_log: false,
            log_scroll: 0,
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

    /// The path box's text with a leading `~/` expanded (the only
    /// tilde form honored; a bare `~` = the home dir).
    fn expanded_path(&self) -> Option<std::path::PathBuf> {
        let raw = self.file_input.value().trim();
        if raw.is_empty() {
            return None;
        }
        let home = std::env::var("HOME").unwrap_or_default();
        let path = if raw == "~" {
            home
        } else if let Some(rest) = raw.strip_prefix("~/") {
            format!("{home}/{rest}")
        } else {
            raw.to_owned()
        };
        Some(std::path::PathBuf::from(path))
    }

    /// The selected transcript row (the yank/write target).
    fn selected_entry(&self) -> Option<&zconned_stdio_types::Entry> {
        let i = self.transcript_sel.selected()?;
        self.entries.get(i)
    }

    /// The row's TEXT: the content value decoded as UTF-8. `None` =
    /// the value has not arrived (the row shows `(value missing)`).
    pub fn entry_text(e: &zconned_stdio_types::Entry) -> Option<String> {
        Some(String::from_utf8_lossy(&e.content.as_ref()?.value.as_ref()?.data).into_owned())
    }

    /// Copy the selected transcript row's text to the clipboard.
    fn transcript_yank(&mut self) {
        let flash = match self.selected_entry().map(crate::app::App::entry_text) {
            Some(Some(text)) if !text.is_empty() => clipboard::set(&text)
                .map(|()| format!("copied {} chars", text.chars().count()))
                .unwrap_or_else(|reason| format!("{reason} — use w to save a file")),
            Some(Some(_)) => "the row's text is empty".into(),
            Some(None) => "value missing — nothing to copy".into(),
            None => "no row selected".into(),
        };
        self.flash = Some(flash);
    }

    /// The MAIN-window file-path box: Enter acts, Esc cancels.
    fn on_key_file_box(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        use ratatui::crossterm::event::{Event, KeyCode, KeyEvent};
        match key.code {
            KeyCode::Enter => {
                let path = self.expanded_path();
                match (self.file_box, path) {
                    (_, None) => self.flash = Some("no path given".into()),
                    (FileBox::SaveCompose, Some(p)) => {
                        let body = self.input.value().to_owned();
                        match std::fs::write(&p, format!("{body}\n")) {
                            Ok(()) => {
                                self.flash = Some(format!("saved to {}", p.display()));
                                self.file_input.reset();
                                self.file_box = FileBox::None;
                            }
                            Err(e) => self.flash = Some(format!("save failed: {e}")),
                        }
                    }
                    (FileBox::LoadCompose, Some(p)) => match std::fs::read_to_string(&p) {
                        Ok(text) => {
                            self.input.reset();
                            for c in text.trim_end_matches(['\r', '\n']).chars() {
                                self.input
                                    .handle_event(&Event::Key(KeyEvent::from(KeyCode::Char(c))));
                            }
                            self.flash = Some("compose loaded from file".into());
                            self.file_input.reset();
                            self.file_box = FileBox::None;
                        }
                        Err(e) => self.flash = Some(format!("read failed: {e}")),
                    },
                    (FileBox::SaveTranscript, Some(p)) => {
                        let flash = match self.selected_entry().map(crate::app::App::entry_text) {
                            Some(Some(text)) => match std::fs::write(&p, format!("{text}\n")) {
                                Ok(()) => {
                                    self.file_input.reset();
                                    self.file_box = FileBox::None;
                                    format!("saved to {}", p.display())
                                }
                                Err(e) => format!("save failed: {e}"),
                            },
                            Some(None) => "value missing — nothing to save".into(),
                            None => "no row selected".into(),
                        };
                        self.flash = Some(flash);
                    }
                    (FileBox::None, _) => {}
                }
            }
            KeyCode::Esc => {
                self.file_input.reset();
                self.file_box = FileBox::None;
            }
            _ => {
                self.file_input.handle_event(&Event::Key(key));
            }
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
        self.input.reset();
        // The timestamp discipline: the interface keeps the
        // "stamped" state (a stamp within the last window). When the
        // state is down, a stamp MESSAGE commits first and the text
        // waits for its receipt (the stamp's digest is the text's
        // chain parent — unknowable until the receipt arrives).
        if self.stamp_needed() {
            self.send_stamp();
            self.awaiting_commit = true;
            self.pending_text = Some(text);
            return;
        }
        self.send_text(text);
    }

    /// Whether the stamp state is down: never stamped, or the last
    /// stamp is older than the window (five minutes — the interface
    /// clears the state by aging it out; a timer and this aging are
    /// observably identical).
    fn stamp_needed(&self) -> bool {
        const STAMP_WINDOW_SECS: i64 = 5 * 60;
        self.ts_stamped_at
            .is_none_or(|t| jiff::Timestamp::now().as_second() - t.as_second() >= STAMP_WINDOW_SECS)
    }

    /// The position intent precedes EVERY commit: the core keeps its
    /// cursor across commits, so an unstated position would chain
    /// from a stale parent and fork the thread.
    fn push_cursor_intent(&mut self) {
        let reply_base = if self.new_thread_intent {
            None
        } else {
            self.open_thread_tip()
        };
        let set = match reply_base {
            Some(tip) => MsgCursorSet {
                at: vec![tip],
                fresh: None,
            },
            None => MsgCursorSet {
                at: Vec::new(),
                fresh: Some(Fresh {}),
            },
        };
        self.new_thread_intent = false;
        self.outbox.push(IncomingFrame {
            method: Some(Method::MsgCursorSet(set)),
        });
    }

    /// The stamp message: empty text content + the core-attested
    /// timestamp claim (the core stamps the RFC3339 commit time at
    /// commit; the interface carries no value).
    fn send_stamp(&mut self) {
        self.push_cursor_intent();
        self.outbox.push(IncomingFrame {
            method: Some(Method::MsgDraftContentSet(MsgDraftContentSet {
                kind: "text/plain".into(),
                data: Vec::new(),
            })),
        });
        self.outbox.push(IncomingFrame {
            method: Some(Method::MsgDraftClaimsAdd(
                zconned_stdio_types::MsgDraftClaimsAdd {
                    kind: CLAIM_KIND_TS.into(),
                    data: Vec::new(),
                },
            )),
        });
        self.awaiting_commit = true;
        self.outbox.push(IncomingFrame {
            method: Some(Method::MsgDraftCommit(MsgDraftCommit {})),
        });
        self.ts_stamped_at = Some(jiff::Timestamp::now());
    }

    fn send_text(&mut self, text: String) {
        self.push_cursor_intent();
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
        use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
        match (self.card_flow, key.code) {
            // Typing the pasted card: Ctrl-V = paste from the
            // clipboard (the file channel is Ctrl-R), Enter submits,
            // Esc cancels back to the list.
            (CardFlow::InputCard, KeyCode::Enter) => {
                let card = self.peer_input.value().trim().to_owned();
                if !card.is_empty() {
                    self.outbox.push(IncomingFrame {
                        method: Some(Method::PeerSet(PeerSet {
                            peer: None,
                            card: Some(card),
                        })),
                    });
                    self.peer_input.reset();
                    self.card_flow = CardFlow::None;
                    self.flash = Some("peer submitted".into());
                }
            }
            (CardFlow::InputCard, KeyCode::Char('v'))
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                match clipboard::get() {
                    Ok(text) => {
                        self.peer_input.reset();
                        for c in text.trim().chars() {
                            self.peer_input
                                .handle_event(&Event::Key(KeyEvent::from(KeyCode::Char(c))));
                        }
                        self.flash = Some("pasted from clipboard".into());
                    }
                    Err(reason) => self.flash = Some(reason),
                }
            }
            (CardFlow::InputCard, KeyCode::Char('r'))
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.file_input.reset();
                self.card_flow = CardFlow::ReadCard;
            }
            (CardFlow::InputCard, KeyCode::Esc) => {
                self.peer_input.reset();
                self.card_flow = CardFlow::None;
            }
            (CardFlow::InputCard, _) => {
                self.peer_input.handle_event(&Event::Key(key));
            }
            // The file-path box (read a card from, or save the card
            // to): Enter acts, Esc goes back a level.
            (CardFlow::ReadCard, KeyCode::Enter) => {
                let path = self.expanded_path();
                match path.map(|p| std::fs::read_to_string(&p)) {
                    Some(Ok(text)) => {
                        self.peer_input.reset();
                        for c in text.trim().chars() {
                            self.peer_input
                                .handle_event(&Event::Key(KeyEvent::from(KeyCode::Char(c))));
                        }
                        self.card_flow = CardFlow::InputCard;
                        self.flash = Some("card loaded from file".into());
                    }
                    Some(Err(e)) => self.flash = Some(format!("read failed: {e}")),
                    None => self.flash = Some("no path given".into()),
                }
            }
            (CardFlow::SaveCard, KeyCode::Enter) => {
                let Some(card) = self.self_card.clone() else {
                    self.flash = Some("no card to save".into());
                    return true;
                };
                let path = self.expanded_path();
                match path {
                    Some(p) => match std::fs::write(&p, format!("{card}\n")) {
                        Ok(()) => {
                            self.flash = Some(format!("card saved to {}", p.display()));
                            self.file_input.reset();
                            self.card_flow = CardFlow::None;
                        }
                        Err(e) => self.flash = Some(format!("save failed: {e}")),
                    },
                    None => self.flash = Some("no path given".into()),
                }
            }
            (CardFlow::ReadCard | CardFlow::SaveCard, KeyCode::Esc) => {
                self.file_input.reset();
                self.card_flow = CardFlow::None;
            }
            (CardFlow::ReadCard | CardFlow::SaveCard, _) => {
                self.file_input.handle_event(&Event::Key(key));
            }
            // Listing posture.
            (CardFlow::None, KeyCode::Char('a')) => {
                self.peer_input.reset();
                self.card_flow = CardFlow::InputCard;
            }
            // Copy the core's own card to the clipboard (may be
            // unavailable — the file channel is the fallback).
            (CardFlow::None, KeyCode::Char('c')) => {
                self.flash = Some(match self.self_card.as_deref() {
                    Some(card) => clipboard::set(card)
                        .map(|()| "card copied to clipboard".to_owned())
                        .unwrap_or_else(|reason| format!("{reason} — use s to save a file")),
                    None => "no card to copy".into(),
                });
            }
            // Save the core's own card to a file.
            (CardFlow::None, KeyCode::Char('s')) => {
                if self.self_card.is_some() {
                    self.file_input.reset();
                    self.card_flow = CardFlow::SaveCard;
                } else {
                    self.flash = Some("no card to save".into());
                }
            }
            (CardFlow::None, KeyCode::Up | KeyCode::Char('k')) => {
                let n = self.peers.len();
                let i = self.peers_sel.selected().unwrap_or(0);
                self.peers_sel
                    .select((n > 0).then_some(i.saturating_sub(1)));
            }
            (CardFlow::None, KeyCode::Down | KeyCode::Char('j')) => {
                let n = self.peers.len();
                let i = self.peers_sel.selected().unwrap_or(0);
                self.peers_sel.select((n > 0).then_some((i + 1).min(n - 1)));
            }
            (CardFlow::None, KeyCode::Esc | KeyCode::Char('p') | KeyCode::Char('q')) => {
                self.show_peers = false;
                return false;
            }
            (CardFlow::None, KeyCode::Char('?')) => self.show_help = true,
            (CardFlow::None, _) => {}
        }
        true
    }

    fn on_key(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        use ratatui::crossterm::event::{Event, KeyCode, KeyEvent};

        // Dead-core posture: nothing but the way out.
        if !self.core_alive {
            if matches!(key.code, KeyCode::Char('q')) {
                self.quit = true;
            }
            return;
        }

        // The help overlay eats every key: only its own close keys
        // and the scroll land; the UI underneath stays untouched.
        if self.show_help {
            match key.code {
                KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') => {
                    self.show_help = false;
                    self.help_scroll = 0;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.help_scroll = self.help_scroll.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.help_scroll = self.help_scroll.saturating_add(1);
                }
                _ => {}
            }
            return;
        }

        if self.file_box != FileBox::None {
            self.on_key_file_box(key);
            return;
        }

        if self.show_peers {
            self.on_key_peers(key);
            return;
        }

        // The log popup eats every key: the panes underneath stay
        // untouched (a stray q must quit from the main surface,
        // never from inside the popup).
        if self.show_log {
            match key.code {
                KeyCode::Esc | KeyCode::Char('L') | KeyCode::Char('q') => {
                    self.show_log = false;
                    self.log_scroll = 0;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.log_scroll = self.log_scroll.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.log_scroll = self.log_scroll.saturating_add(1);
                }
                _ => {}
            }
            return;
        }

        if self.focus == Pane::Compose {
            use ratatui::crossterm::event::KeyModifiers;
            match key.code {
                KeyCode::Tab => self.cycle_focus(),
                KeyCode::Esc => self.input.reset(),
                KeyCode::Enter => self.compose_send(),
                // The compose channels: copy/paste through the
                // clipboard (may be unavailable), read/write through
                // a file (always available).
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    let text = self.input.value().trim().to_owned();
                    self.flash = Some(if text.is_empty() {
                        "nothing to copy".into()
                    } else {
                        clipboard::set(&text)
                            .map(|()| "compose copied to clipboard".to_owned())
                            .unwrap_or_else(|reason| {
                                format!("{reason} — use Ctrl-S to save a file")
                            })
                    });
                }
                KeyCode::Char('v') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    match clipboard::get() {
                        Ok(text) => {
                            for c in text.trim_end_matches(['\r', '\n']).chars() {
                                self.input
                                    .handle_event(&Event::Key(KeyEvent::from(KeyCode::Char(c))));
                            }
                            self.flash = Some("pasted from clipboard".into());
                        }
                        Err(reason) => self.flash = Some(reason),
                    }
                }
                KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.file_input.reset();
                    self.file_box = FileBox::LoadCompose;
                }
                KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.file_input.reset();
                    self.file_box = FileBox::SaveCompose;
                }
                _ => {
                    self.input.handle_event(&Event::Key(key));
                }
            }
            return;
        }

        match key.code {
            // The help overlay (lazygit-style); the input panes
            // keep typing a literal '?'.
            KeyCode::Char('?') => self.show_help = true,
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Tab => self.cycle_focus(),
            // The horizontal pane navigation (lazygit-style): h = the
            // left panel, l = the right one. The compose pane stays
            // Tab-only — h/j/k/l type literal text while composing.
            KeyCode::Char('h') => self.focus = Pane::Threads,
            KeyCode::Char('l') => self.focus = Pane::Transcript,
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
            // The transcript text channels: yank to the clipboard,
            // save to a file (the path box).
            KeyCode::Char('y') if self.focus == Pane::Transcript => self.transcript_yank(),
            KeyCode::Char('w') if self.focus == Pane::Transcript => {
                self.file_input.reset();
                self.file_box = FileBox::SaveTranscript;
            }
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
            KeyCode::Char('L') => {
                self.show_log = !self.show_log;
                self.log_scroll = 0;
            }
            // The peers popup: the set + the core's own card, one
            // refresh per open (cheap, and the set is small).
            KeyCode::Char('p') => {
                self.show_peers = true;
                self.card_flow = CardFlow::None;
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
                    // A stamp receipt releases the queued text: the
                    // tip now includes the stamp row, so the text's
                    // position intent addresses it (its chain
                    // parent).
                    if let Some(text) = self.pending_text.take() {
                        self.send_text(text);
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
                // A value that just landed in the core patches the
                // matching value-absent rows of the open transcript
                // in place — no full log re-read needed.
                if let (Some(r), Some(v)) = (p.r#ref.as_ref(), p.value.clone()) {
                    for e in &mut self.entries {
                        if let Some(slot) = e.content.as_mut() {
                            let absent = slot.r#ref.as_ref().is_some_and(|c| c.digest == r.digest)
                                && slot.value.is_none();
                            if absent {
                                slot.value = Some(v.clone());
                            }
                        }
                    }
                }
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
                    e.message =
                        "stale core build — rebuild it: cargo build -p zconned --bin zconned"
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
