//! The view model: what the shell shows, and the lazygit-flavored
//! keymap that turns keys into wire requests. Core's data is always
//! the truth — deliveries update this model as they arrive; nothing
//! here re-implements core logic.

use std::collections::{BTreeMap, VecDeque};

use ratatui::widgets::ListState;
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler as _;

use crate::draft::Draft;
use zconned_material::{CLAIM_KIND_TS, MsgRef, Value};
use zconned_stdio_types::{
    Entry, Error as FrameError, Fresh, IdentitiesGet, IdentityRemarkReset, IdentityRemarkSet,
    IncomingFrame, MsgCursorSet, MsgDraftCommit, MsgDraftContentSet, MsgDraftGet, MsgPackage,
    MsgThreadsGet, MsgThreadsLogGet, Peer, PeerSet, PeersGet, SelfGet, incoming_frame::Method,
    outgoing_frame::Frame as Out,
};

use crate::clipboard;
use crate::core::Process;
use crate::events::Evt;

/// Which bottom box the peers popup shows: the pasted-card input,
/// the read-card-from-file path box, the save-card-to-file path
/// box, or the authors tab's remark input. `None` = the plain list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardFlow {
    None,
    InputCard,
    ReadCard,
    SaveCard,
    InputRemark,
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

/// The content a region can hold — the mode's 1..4 menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentKind {
    Threads,
    Transcript,
    Authors,
    Mailboxes,
}

impl ContentKind {
    /// The title word (`1 threads`).
    pub fn title(self) -> &'static str {
        match self {
            ContentKind::Threads => "threads",
            ContentKind::Transcript => "transcript",
            ContentKind::Authors => "authors",
            ContentKind::Mailboxes => "mailboxes",
        }
    }

    /// The mode order (1..4).
    pub fn nth(n: usize) -> ContentKind {
        [
            ContentKind::Threads,
            ContentKind::Transcript,
            ContentKind::Authors,
            ContentKind::Mailboxes,
        ][n]
    }
}

/// One region: which content it shows, plus that content's VIEW
/// state. The view state is per-region on purpose — the same content
/// open in both regions keeps independent cursors; the DATA it
/// renders stays global (the core's truth is shared).
#[derive(Debug)]
pub struct Viewport {
    pub content: ContentKind,
    pub threads_sel: ListState,
    pub transcript_sel: ListState,
    pub authors_sel: ListState,
    pub mailboxes_sel: ListState,
    /// This region's transcript viewport row count at the last
    /// render — the PgUp/PgDn page.
    pub page_rows: u16,
}

impl Viewport {
    /// The ACTIVE content's selection (the list-motion layer works
    /// on whatever the region currently shows).
    fn sel_mut(&mut self) -> &mut ListState {
        match self.content {
            ContentKind::Threads => &mut self.threads_sel,
            ContentKind::Transcript => &mut self.transcript_sel,
            ContentKind::Authors => &mut self.authors_sel,
            ContentKind::Mailboxes => &mut self.mailboxes_sel,
        }
    }

    fn new(content: ContentKind) -> Self {
        Self {
            content,
            threads_sel: ListState::default(),
            transcript_sel: ListState::default(),
            authors_sel: ListState::default(),
            mailboxes_sel: ListState::default(),
            page_rows: 0,
        }
    }
}

/// What holds the focus: a numbered region, or the composer (the
/// transcript region's sub-window — never a region of its own).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Region(usize),
    Compose,
}

#[derive(Debug)]
pub struct App {
    pub focus: Focus,
    /// The regions (two now; the later layout engine grows the
    /// vector without a rewrite).
    pub regions: Vec<Viewport>,
    /// The composer renders inside its HOST region's transcript —
    /// fixed at summon time.
    pub compose_host: usize,
    /// The mode: the in-place content menu over the focused region
    /// (a modal — it eats every key until closed).
    pub mode_menu: bool,
    pub threads: Vec<MsgRef>,
    pub entries: Vec<Entry>,
    /// The thread whose log the transcript currently shows (the reply
    /// base and the arrival-routing hint).
    pub open_thread: Option<MsgRef>,
    /// The multi-line compose draft (the local editor state; the
    /// single-line boxes below keep tui-input).
    pub draft: Draft,
    /// The compose surface is summoned/collapsed. When visible but
    /// unfocused it is STANDBY: the whole UI behaves exactly like
    /// the default state, plus one Esc function (Esc hides it).
    pub show_compose: bool,
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
    /// The lazygit-style help overlay (opened with `?`); it eats all
    /// keys until closed (Esc / ? / q).
    pub show_help: bool,
    /// The help overlay's scroll offset (j/k; a short terminal
    /// scrolls the card instead of clipping it).
    pub help_scroll: u16,
    pub peer_input: Input,
    /// The active bottom box in the peers popup.
    pub card_flow: CardFlow,
    /// The file-path box's text (for `read card from` / `save card
    /// to`).
    pub file_input: Input,
    /// The active MAIN-window file-path box.
    pub file_box: FileBox,
    /// The card staged for the save-to-file flow: the SELECTED entry
    /// in the peers popup (my card or a peer's), resolved at `s`
    /// time — a set refresh between `s` and Enter never repoints it.
    pub save_card: Option<String>,
    /// The open transcript's per-row overlay facts (msg digest →
    /// the verified signer anchors + the author's remark): rebuilt
    /// wholesale by a log answer, patched by package re-pushes.
    pub locals: BTreeMap<Vec<u8>, Overlay>,
    /// The AUTHOR registry: every sender identity observed this
    /// session — observed on an envelope until a roster mention
    /// proves authorship (verified; signatures never retract, the
    /// upgrade is one-way), plus the last stated remark. The authors
    /// domain of the profile popup; separate from the mailbox book
    /// even though both index by the anchor.
    pub authors: BTreeMap<Vec<u8>, AuthorEntry>,
    /// The remark input (the authors content's bottom box).
    pub remark_input: Input,
    /// Set between a remark mutation request and its IdentitiesPush
    /// receipt: the receipt then also re-fetches the displayed
    /// thread's log (delivered packages are never echoed).
    pub awaiting_remark_receipt: bool,
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

/// The card text: the canonical single-line base64 of a peer record
/// (the very form the core decodes a pasted card from; the inverse
/// codec, not core logic — the core stays the identity's truth).
pub(crate) fn card_text(peer: &Peer) -> String {
    use base64::Engine as _;
    use prost::Message as _;
    base64::engine::general_purpose::STANDARD.encode(peer.encode_to_vec())
}

/// The inverse: a card text decodes back to the record (DISPLAY
/// only — identity questions always go to the core).
pub fn peer_from_card(card: &str) -> Option<Peer> {
    use base64::Engine as _;
    use prost::Message as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(card.trim())
        .ok()?;
    Peer::decode(bytes.as_slice()).ok()
}

/// A peer's addressing anchor: the fingerprint of its signing key —
/// the same derivation the core applies, and the key rosters and
/// row authors are addressed by.
pub fn anchor_of(peer: &Peer) -> Vec<u8> {
    match <[u8; zconned_crypto::DIGEST_LEN]>::try_from(peer.sign_pub.as_slice()) {
        Ok(sign) => zconned_crypto::Fingerprint::of(&sign).digest().to_vec(),
        Err(_) => Vec::new(),
    }
}

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

/// The AUTHOR registry's row: whether a roster ever proved the
/// identity's authorship (one-way), and the last stated local remark.
#[derive(Debug, Default, Clone)]
pub struct AuthorEntry {
    pub verified: bool,
    pub remark: Option<String>,
}

/// A row's overlay facts: the verified signer anchors + the author's
/// local remark (the delivery package's Local group, reshaped for
/// the display).
#[derive(Debug, Default, Clone)]
pub struct Overlay {
    pub signers: Vec<Vec<u8>>,
    pub author_remark: Option<String>,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        Self {
            focus: Focus::Region(0),
            regions: vec![
                Viewport::new(ContentKind::Threads),
                Viewport::new(ContentKind::Transcript),
            ],
            compose_host: 1,
            mode_menu: false,
            threads: Vec::new(),
            entries: Vec::new(),
            open_thread: None,
            draft: Draft::new(),
            show_compose: false,
            cursor: Vec::new(),
            new_thread_intent: false,
            staged: None,
            peers: Vec::new(),
            self_card: None,
            show_help: false,
            help_scroll: 0,
            peer_input: Input::default(),
            card_flow: CardFlow::None,
            file_input: Input::default(),
            file_box: FileBox::None,
            save_card: None,
            locals: BTreeMap::new(),
            authors: BTreeMap::new(),
            remark_input: Input::default(),
            awaiting_remark_receipt: false,
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

    /// The AUTHOR registry's wholesale get: the answer (or a remark
    /// set/reset receipt) arrives as an IdentitiesPush.
    fn refresh_identities(&mut self) {
        self.outbox.push(IncomingFrame {
            method: Some(Method::IdentitiesGet(IdentitiesGet {})),
        });
    }

    /// The forward focus cycle: region 1 → region 2 → region 1.
    /// The composer is NOT in the cycle — `i`/`n` are its only
    /// entries (its standby face is the default state).
    fn cycle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Region(0) => Focus::Region(1),
            _ => Focus::Region(0),
        };
    }

    /// The reverse cycle (Ctrl+k): region 2 → region 1 → region 2.
    fn cycle_focus_back(&mut self) {
        self.focus = match self.focus {
            Focus::Region(0) => Focus::Region(1),
            _ => Focus::Region(0),
        };
    }

    /// Direct structural jumps: to a numbered region (Ctrl+h/l, the
    /// arrows, 1/2; the composer's moving mode rides the same layer).
    fn nav_to_region(&mut self, region: usize) {
        self.focus = Focus::Region(region.min(self.regions.len().saturating_sub(1)));
    }

    /// The focused region's index (the composer has none — its host
    /// region stands in for view-state purposes).
    pub(crate) fn focus_region(&self) -> usize {
        match self.focus {
            Focus::Region(i) => i.min(self.regions.len().saturating_sub(1)),
            Focus::Compose => self.compose_host.min(self.regions.len().saturating_sub(1)),
        }
    }

    /// PgUp/PgDn: the selection moves by a page (the focused
    /// region's last rendered transcript viewport height; a sane
    /// page before first paint).
    fn page_sel(&mut self, dir: isize) {
        let page = self.regions[self.focus_region()].page_rows.max(2) as isize - 1;
        self.move_sel(dir * page);
    }

    fn move_sel(&mut self, delta: isize) {
        let fi = self.focus_region();
        let kind = self.regions[fi].content;
        let (list, len) = match kind {
            ContentKind::Threads => (&mut self.regions[fi].threads_sel, self.threads.len()),
            ContentKind::Transcript => (&mut self.regions[fi].transcript_sel, self.entries.len()),
            ContentKind::Authors => (&mut self.regions[fi].authors_sel, self.authors.len()),
            ContentKind::Mailboxes => (&mut self.regions[fi].mailboxes_sel, self.peers.len() + 1),
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

    /// The active list's length (for g/G jump-to-end).
    fn sel_len(&self) -> usize {
        let fi = self.focus_region();
        match self.regions[fi].content {
            ContentKind::Threads => self.threads.len(),
            ContentKind::Transcript => self.entries.len(),
            ContentKind::Authors => self.authors.len(),
            ContentKind::Mailboxes => self.peers.len() + 1,
        }
    }

    /// g/Home: jump the active list's selection to the top.
    fn sel_home(&mut self) {
        let fi = self.focus_region();
        let has = self.sel_len() > 0;
        self.regions[fi]
            .sel_mut()
            .select(if has { Some(0) } else { None });
    }

    /// G/End: jump the active list's selection to the bottom.
    fn sel_end(&mut self) {
        let fi = self.focus_region();
        let n = self.sel_len();
        self.regions[fi]
            .sel_mut()
            .select(if n > 0 { Some(n - 1) } else { None });
    }

    /// Switch a region's content (the mode's 1..4, `p`, opening a
    /// thread). Switching the composer's host region away from the
    /// transcript collapses the composer — its surface is gone.
    fn set_region_content(&mut self, region: usize, kind: ContentKind) {
        let region = region.min(self.regions.len().saturating_sub(1));
        if self.regions[region].content == kind {
            return;
        }
        if self.show_compose && self.compose_host == region {
            self.show_compose = false;
            if self.focus == Focus::Compose {
                self.focus = Focus::Region(region);
            }
        }
        self.regions[region].content = kind;
        match kind {
            // One refresh per activation (cheap; the sets are small).
            ContentKind::Mailboxes => {
                self.regions[region].mailboxes_sel.select(Some(0));
                self.refresh_peers();
                self.refresh_self();
                self.refresh_identities();
            }
            // A fresh wholesale get per activation of the registry.
            ContentKind::Authors => {
                self.regions[region].authors_sel.select(Some(0));
                self.refresh_identities();
            }
            _ => {}
        }
    }

    /// Enter on a threads region: fetch the selected thread's log
    /// (reads are cursor-independent — browsing never moves the
    /// anchor) and turn the region into its transcript.
    fn open_selected_thread(&mut self, region: usize) {
        let fi = region.min(self.regions.len().saturating_sub(1));
        let thread = self.regions[fi]
            .threads_sel
            .selected()
            .and_then(|i| self.threads.get(i).cloned());
        if let Some(t) = thread {
            self.entries.clear();
            self.locals.clear();
            self.open_thread = Some(t.clone());
            self.outbox.push(IncomingFrame {
                method: Some(Method::MsgThreadsLogGet(MsgThreadsLogGet {
                    thread: Some(t.clone()),
                })),
            });
            self.set_region_content(fi, ContentKind::Transcript);
            self.regions[fi].transcript_sel.select(Some(0));
        }
    }

    /// The selected author's anchor (BTreeMap order = anchor order).
    fn selected_author(&self) -> Option<Vec<u8>> {
        let i = self.regions[self.focus_region()].authors_sel.selected()?;
        self.authors.keys().nth(i).cloned()
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

    /// The selected transcript row of the FOCUSED region (the
    /// yank/write target).
    fn selected_entry(&self) -> Option<&zconned_stdio_types::Entry> {
        let i = self.regions[self.focus_region()]
            .transcript_sel
            .selected()?;
        self.entries.get(i)
    }

    /// The selected entry's card text, per the focused region's
    /// content: mailboxes — row 0 = my card (verbatim from the
    /// core), the rest = the peer set, re-encoded canonically (the
    /// card text IS the record's canonical single-line base64);
    /// authors — the anchor's card, only if the book holds that
    /// mailbox.
    fn selected_card(&self) -> Option<String> {
        match self.regions[self.focus_region()].content {
            ContentKind::Authors => self
                .selected_author()
                .and_then(|a| self.peers.iter().find(|p| anchor_of(p) == a).map(card_text)),
            ContentKind::Mailboxes => {
                match self.regions[self.focus_region()].mailboxes_sel.selected() {
                    Some(i) if i > 0 => self.peers.get(i - 1).map(card_text),
                    _ => self.self_card.clone(),
                }
            }
            _ => None,
        }
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
        use ratatui::crossterm::event::{Event, KeyCode};
        match key.code {
            KeyCode::Enter => {
                let path = self.expanded_path();
                match (self.file_box, path) {
                    (_, None) => self.flash = Some("no path given".into()),
                    (FileBox::SaveCompose, Some(p)) => {
                        let body = self.draft.text();
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
                            self.draft.clear();
                            self.draft.insert_str(text.trim_end_matches(['\r', '\n']));
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

    /// The send face of the composer (Ctrl/Alt-Enter): the commit
    /// flow, one hand-typed frame per step (cursor → content →
    /// commit). An unset cursor replies to the OPEN thread (at = its
    /// tip); with no open thread (or a new-thread intent) it starts a
    /// fresh chain. Sending collapses the composer — the text is on
    /// its way, the pane goes back to the reading surface.
    /// The composer's key face: the SEND keys first (Ctrl/Alt-Enter
    /// must not be eaten by the structural branch), then the
    /// structural layer (Ctrl+hjkl works from the caret too), then
    /// the editor. Esc steps down to STANDBY: the focus returns to
    /// the transcript region and the whole UI behaves like the
    /// default state — only Esc now also hides the composer.
    fn on_key_compose(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        use ratatui::crossterm::event::{KeyCode, KeyModifiers};
        // Send: either spelling (terminal support varies).
        if key.code == KeyCode::Enter
            && key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            self.compose_send();
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT)
        {
            match key.code {
                KeyCode::Char('h') | KeyCode::Left => self.nav_to_region(0),
                KeyCode::Char('l') | KeyCode::Right => self.nav_to_region(1),
                KeyCode::Char('j') | KeyCode::Down => self.cycle_focus(),
                KeyCode::Char('k') | KeyCode::Up => self.cycle_focus_back(),
                KeyCode::Tab => self.cycle_focus(),
                _ => return,
            }
            return;
        }
        match key.code {
            // Leave typing: standby (visible, unfocused, the normal
            // UI everywhere; Esc again hides the composer).
            KeyCode::Esc => self.focus = Focus::Region(self.compose_host),
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.draft.clear();
            }
            // The compose channels: copy/paste through the clipboard
            // (may be unavailable), read/write through a file (always
            // available).
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let text = self.draft.text().trim().to_owned();
                self.flash = Some(if text.is_empty() {
                    "nothing to copy".into()
                } else {
                    clipboard::set(&text)
                        .map(|()| "compose copied to clipboard".to_owned())
                        .unwrap_or_else(|reason| format!("{reason} — use Ctrl-S to save a file"))
                });
            }
            KeyCode::Char('v') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                match clipboard::get() {
                    Ok(text) => {
                        self.draft.insert_str(text.trim_end_matches(['\r', '\n']));
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
                self.draft.handle_key(key);
            }
        }
    }

    fn compose_send(&mut self) {
        let text = self.draft.text().trim().to_owned();
        if text.is_empty() {
            return;
        }
        self.draft.clear();
        self.show_compose = false;
        self.focus = Focus::Region(self.compose_host);
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

    /// Keys while a card/remark/path flow input is up (the input
    /// boxes live at the bottom of their region content); the flow
    /// eats every key until it ends. The LIST posture around the
    /// flows (add/copy/save, j/k) belongs to the content tables.
    fn on_key_card_flow(&mut self, key: ratatui::crossterm::event::KeyEvent) {
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
                let Some(card) = self.save_card.clone() else {
                    self.flash = Some("no card to save".into());
                    return;
                };
                let path = self.expanded_path();
                match path {
                    Some(p) => match std::fs::write(&p, format!("{card}\n")) {
                        Ok(()) => {
                            self.flash = Some(format!("card saved to {}", p.display()));
                            self.file_input.reset();
                            self.save_card = None;
                            self.card_flow = CardFlow::None;
                        }
                        Err(e) => self.flash = Some(format!("save failed: {e}")),
                    },
                    None => self.flash = Some("no path given".into()),
                }
            }
            (CardFlow::ReadCard | CardFlow::SaveCard, KeyCode::Esc) => {
                self.file_input.reset();
                self.save_card = None;
                self.card_flow = CardFlow::None;
            }
            (CardFlow::ReadCard | CardFlow::SaveCard, _) => {
                self.file_input.handle_event(&Event::Key(key));
            }
            // The remark input: Enter submits (the core answers
            // remark_required on an empty remark — pre-checked here),
            // Ctrl-V pastes, Esc cancels back to the list.
            (CardFlow::InputRemark, KeyCode::Enter) => {
                let text = self.remark_input.value().trim().to_owned();
                match (self.selected_author(), text.is_empty()) {
                    (Some(anchor), false) => {
                        self.outbox.push(IncomingFrame {
                            method: Some(Method::IdentityRemarkSet(IdentityRemarkSet {
                                anchor,
                                remark: text,
                            })),
                        });
                        self.awaiting_remark_receipt = true;
                        self.remark_input.reset();
                        self.card_flow = CardFlow::None;
                    }
                    (Some(_), true) => self.flash = Some("remark required".into()),
                    (None, _) => {
                        self.flash = Some("no identity selected".into());
                        self.card_flow = CardFlow::None;
                    }
                }
            }
            (CardFlow::InputRemark, KeyCode::Char('v'))
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                match clipboard::get() {
                    Ok(text) => {
                        for c in text.trim().chars() {
                            self.remark_input
                                .handle_event(&Event::Key(KeyEvent::from(KeyCode::Char(c))));
                        }
                    }
                    Err(reason) => self.flash = Some(reason),
                }
            }
            (CardFlow::InputRemark, KeyCode::Esc) => {
                self.remark_input.reset();
                self.card_flow = CardFlow::None;
            }
            (CardFlow::InputRemark, _) => {
                self.remark_input.handle_event(&Event::Key(key));
            }
            (CardFlow::None, _) => {}
        }
    }

    fn on_key(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        use ratatui::crossterm::event::{KeyCode, KeyModifiers};

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

        // The mode: a modal over the focused region — 1..4 picks
        // the content, Esc/m closes without change, the rest is
        // ignored (q is NOT bound: q means quit, everywhere).
        if self.mode_menu {
            match key.code {
                KeyCode::Char(c @ ('1' | '2' | '3' | '4')) => {
                    let n = c.to_digit(10).expect("checked") as usize - 1;
                    self.mode_menu = false;
                    self.set_region_content(self.focus_region(), ContentKind::nth(n));
                }
                KeyCode::Esc | KeyCode::Char('m') => self.mode_menu = false,
                _ => {}
            }
            return;
        }

        // A card/remark/path flow input eats every key.
        if self.card_flow != CardFlow::None {
            self.on_key_card_flow(key);
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

        if self.focus == Focus::Compose {
            self.on_key_compose(key);
            return;
        }

        // Region focus from here. The STRUCTURAL layer first (it
        // works over every content), then the true globals, then the
        // generic list motion, then the focused content's own keys.
        match key.code {
            // The help overlay (lazygit-style); the input panes
            // keep typing a literal '?'.
            KeyCode::Char('?') => self.show_help = true,
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Tab => self.cycle_focus(),
            // The structural layer: Ctrl+hjkl navigates from ANY
            // focus (plain hjkl belongs to the focused content: the
            // lists scroll, the composer types).
            KeyCode::Char('h') if key.modifiers == KeyModifiers::CONTROL => self.nav_to_region(0),
            KeyCode::Char('l') if key.modifiers == KeyModifiers::CONTROL => self.nav_to_region(1),
            KeyCode::Char('j') if key.modifiers == KeyModifiers::CONTROL => self.cycle_focus(),
            KeyCode::Char('k') if key.modifiers == KeyModifiers::CONTROL => self.cycle_focus_back(),
            // The arrow keys mirror hjkl on the structural layer too.
            KeyCode::Left if key.modifiers == KeyModifiers::CONTROL => self.nav_to_region(0),
            KeyCode::Right if key.modifiers == KeyModifiers::CONTROL => self.nav_to_region(1),
            KeyCode::Up if key.modifiers == KeyModifiers::CONTROL => self.cycle_focus_back(),
            KeyCode::Down if key.modifiers == KeyModifiers::CONTROL => self.cycle_focus(),
            // The numbered regions: the direct jump (no cycling).
            KeyCode::Char('1') => self.nav_to_region(0),
            KeyCode::Char('2') => self.nav_to_region(1),
            // The mode: the in-place content menu over this
            // region. `o` stays free for the transcript's future
            // open-file key (the multimedia era).
            KeyCode::Char('m') => self.mode_menu = true,
            // The mailboxes, right here: focus stays, the content
            // swaps.
            KeyCode::Char('p') => {
                let fi = self.focus_region();
                self.set_region_content(fi, ContentKind::Mailboxes);
            }
            KeyCode::Char('h') => self.nav_to_region(0),
            KeyCode::Char('l') => self.nav_to_region(1),
            // The core's stderr ring — the diagnosis window (a talky
            // core: --core "zconned -v").
            KeyCode::Char('L') => {
                self.show_log = !self.show_log;
                self.log_scroll = 0;
            }
            // Standby composer: Esc hides it (its one extra
            // function over the default state).
            KeyCode::Esc if self.show_compose => {
                self.show_compose = false;
            }
            // Generic list motion over the focused region's active
            // content.
            KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
            KeyCode::Home | KeyCode::Char('g') => self.sel_home(),
            KeyCode::End | KeyCode::Char('G') => self.sel_end(),
            // The transcript pages by the region's last rendered
            // viewport.
            KeyCode::PageUp
                if self.regions[self.focus_region()].content == ContentKind::Transcript =>
            {
                self.page_sel(-1)
            }
            KeyCode::PageDown
                if self.regions[self.focus_region()].content == ContentKind::Transcript =>
            {
                self.page_sel(1)
            }
            _ => self.on_key_content(key),
        }
    }

    /// The focused content's OWN keys (the content layer — what the
    /// structural layer and the generic list motion leave behind).
    fn on_key_content(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        use ratatui::crossterm::event::KeyCode;
        let fi = self.focus_region();
        match self.regions[fi].content {
            ContentKind::Threads => match key.code {
                KeyCode::Char('t') => self.refresh_threads(),
                KeyCode::Enter => self.open_selected_thread(fi),
                _ => {}
            },
            ContentKind::Transcript => match key.code {
                // The transcript text channels: yank to the
                // clipboard, save to a file (the path box).
                KeyCode::Char('y') => self.transcript_yank(),
                KeyCode::Char('w') => {
                    self.file_input.reset();
                    self.file_box = FileBox::SaveTranscript;
                }
                KeyCode::Char('r') => self.refresh_draft(),
                // New-thread compose: the next send starts a fresh
                // chain even though a thread transcript is open.
                KeyCode::Char('n') => {
                    self.summon_compose(fi, true);
                    self.flash = Some("new thread".into());
                }
                // Summon the composer (leaving it was Esc — symmetric
                // entry; typing starts immediately). The composer is
                // the transcript's sub-window: other contents have
                // no compose.
                KeyCode::Char('i') => self.summon_compose(fi, false),
                _ => {}
            },
            ContentKind::Authors => match key.code {
                // The remark edit on the selected identity (the
                // registry is the remark's authority).
                KeyCode::Char('r') => {
                    self.remark_input.reset();
                    self.card_flow = CardFlow::InputRemark;
                }
                // Clear the selected identity's remark (the
                // registry's reset verb; the receipt is an
                // IdentitiesPush).
                KeyCode::Char('R') => match self.selected_author() {
                    Some(anchor) => {
                        self.outbox.push(IncomingFrame {
                            method: Some(Method::IdentityRemarkReset(IdentityRemarkReset {
                                anchor,
                            })),
                        });
                        self.awaiting_remark_receipt = true;
                    }
                    None => self.flash = Some("no identity selected".into()),
                },
                KeyCode::Char('c') => self.copy_selected_card(),
                KeyCode::Char('s') => self.save_selected_card(),
                _ => {}
            },
            ContentKind::Mailboxes => match key.code {
                // Adding a card is a MAILBOX-domain action.
                KeyCode::Char('a') => {
                    self.peer_input.reset();
                    self.card_flow = CardFlow::InputCard;
                }
                KeyCode::Char('c') => self.copy_selected_card(),
                KeyCode::Char('s') => self.save_selected_card(),
                _ => {}
            },
        }
    }

    /// Summon the composer into `region`'s transcript: the region
    /// becomes the transcript if it is not already (the composer has
    /// no other home), then the focus lands on the composer in the
    /// typing mode.
    fn summon_compose(&mut self, region: usize, new_thread: bool) {
        let fi = region.min(self.regions.len().saturating_sub(1));
        self.set_region_content(fi, ContentKind::Transcript);
        if new_thread {
            self.new_thread_intent = true;
        }
        self.show_compose = true;
        self.compose_host = fi;
        self.focus = Focus::Compose;
    }

    /// Copy the SELECTED card to the clipboard (a mailbox's or — in
    /// the authors content — an author whose card the book holds;
    /// the clipboard may be unavailable — the file channel is the
    /// fallback).
    fn copy_selected_card(&mut self) {
        self.flash = Some(match self.selected_card() {
            Some(card) => clipboard::set(&card)
                .map(|()| "card copied to clipboard".to_owned())
                .unwrap_or_else(|reason| format!("{reason} — use s to save a file")),
            None => "no card to copy".into(),
        });
    }

    /// Save the SELECTED card to a file (resolved NOW — a set
    /// refresh before Enter never repoints it).
    fn save_selected_card(&mut self) {
        match self.selected_card() {
            Some(card) => {
                self.file_input.reset();
                self.save_card = Some(card);
                self.card_flow = CardFlow::SaveCard;
            }
            None => self.flash = Some("no card to save".into()),
        }
    }

    /// Frames arrive ANYTIME; each updates the view — the interface
    /// renders what the core reports.
    fn on_frame(&mut self, f: zconned_stdio_types::OutgoingFrame) {
        match f.frame {
            // The ONE delivery unit: a network arrival, the own commit
            // receipt, and a roster-driven re-push all speak this
            // frame; the interface never branches on the provenance.
            Some(Out::MsgPackagePush(p)) => {
                if let Some(pkg) = p.package {
                    self.ingest_package(pkg);
                }
            }
            Some(Out::MsgCursorPush(p)) => self.cursor = p.refs,
            Some(Out::MsgDraftPush(d)) => self.staged = Some((d.content, d.claims)),
            Some(Out::MsgThreadsPush(p)) => {
                self.threads = p.threads.into_iter().filter_map(|t| t.thread).collect();
                self.clamp_sels(ContentKind::Threads, self.threads.len());
            }
            Some(Out::MsgThreadsLogPush(p)) => {
                // The log answer: one package per message, transcript
                // order. The rows AND their overlays are replaced
                // WHOLESALE (the answer states the full thread state);
                // every package feeds the AUTHOR registry.
                self.entries = Vec::with_capacity(p.packages.len());
                self.locals = p
                    .packages
                    .into_iter()
                    .filter_map(|pkg| {
                        let entry = pkg.entry?;
                        let overlay = pkg.local.unwrap_or_default();
                        let digest = entry.meta.as_ref()?.digest.clone();
                        let from = entry
                            .meta
                            .as_ref()
                            .map(|m| m.from.clone())
                            .unwrap_or_default();
                        self.harvest_author(&from, false);
                        for s in &overlay.signer {
                            self.harvest_author(&s.anchor, true);
                        }
                        self.entries.push(entry);
                        Some((
                            digest,
                            Overlay {
                                signers: overlay.signer.iter().map(|s| s.anchor.clone()).collect(),
                                author_remark: overlay.remark,
                            },
                        ))
                    })
                    .collect();
                self.clamp_sels(ContentKind::Transcript, self.entries.len());
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
            // The registry's wholesale answer AND the remark
            // set/reset receipt (dual-role): the remark is the
            // registry's fact — overwritten both ways; the verified
            // flag stays a package-side fact. No echo: delivered
            // packages keep their compose-time remarks — after a
            // mutation, the displayed thread's log is re-fetched.
            Some(Out::IdentitiesPush(p)) => {
                for i in &p.identities {
                    self.authors.entry(i.anchor.clone()).or_default().remark = i.remark.clone();
                }
                if self.awaiting_remark_receipt {
                    self.awaiting_remark_receipt = false;
                    if let Some(t) = self.open_thread.clone() {
                        self.outbox.push(IncomingFrame {
                            method: Some(Method::MsgThreadsLogGet(MsgThreadsLogGet {
                                thread: Some(t),
                            })),
                        });
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

    /// The one ingest path for the msg domain: every package (a live
    /// push, the own commit receipt, or a log-answer row) upserts by
    /// the entry's digest. The frame's provenance is never branched
    /// on — the interface derives newness (first sight of the
    /// digest) and own-ness (the awaiting-commit flag) locally.
    fn ingest_package(&mut self, pkg: MsgPackage) {
        let Some(e) = pkg.entry else { return };
        let overlay = pkg.local.unwrap_or_default();
        let digest = e.meta.as_ref().map(|m| m.digest.clone());
        let from = e.meta.as_ref().map(|m| m.from.clone()).unwrap_or_default();

        // The AUTHOR registry: the envelope anchor is an OBSERVATION
        // until a roster mention states it (verified is a one-way
        // upgrade). The remark is NOT harvested here — the registry
        // (the IdentitiesPush face) is its only authority; the
        // package's remark serves the row display only.
        self.harvest_author(&from, false);
        for s in &overlay.signer {
            self.harvest_author(&s.anchor, true);
        }
        // The row's overlay store (the verified signer anchors + the
        // author's remark), keyed by the digest.
        if let Some(d) = digest.clone() {
            self.locals.insert(
                d,
                Overlay {
                    signers: overlay.signer.iter().map(|s| s.anchor.clone()).collect(),
                    author_remark: overlay.remark,
                },
            );
        }

        // The commit receipt vs an arrival: the awaiting flag (set by
        // the commit request) tells them apart — a rare arrival racing
        // in between is misattributed (a day-one residual).
        let receipt = self.awaiting_commit;
        if receipt {
            self.awaiting_commit = false;
            if let Some(d) = digest.clone() {
                self.sent.push((d, !self.peers.is_empty()));
            }
        }

        // Upsert by digest: a first sight routes (append live /
        // adopt the thread), a re-push replaces the held row in
        // place (its roster may have grown).
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
                self.follow_transcripts(n - 1);
            } else if self.open_thread.is_none() {
                // Nothing open: show the row and adopt its thread (a
                // fresh commit's first prev IS the thread id — the
                // genesis parent ref).
                self.entries = vec![e.clone()];
                self.open_thread = e.prev.first().cloned();
                self.follow_transcripts(0);
            }
            self.refresh_threads();
            self.flash = Some("msg arrived".into());
        } else if let Some(d) = digest
            && let Some(slot) = self
                .entries
                .iter_mut()
                .find(|x| x.meta.as_ref().is_some_and(|m| m.digest == d))
        {
            *slot = e;
        }

        // A stamp receipt releases the queued text: the tip now
        // includes the stamp row, so the text's position intent
        // addresses it (its chain parent). Only the receipt frame
        // releases — a roster re-push racing in between must not
        // fire the text early.
        if receipt && let Some(text) = self.pending_text.take() {
            self.send_text(text);
        }
    }

    /// One identity observation into the AUTHOR registry. A roster
    /// mention proves verified authorship (signatures never retract —
    /// the upgrade is one-way). The remark never rides this path:
    /// the registry is its only authority.
    fn harvest_author(&mut self, anchor: &[u8], verified: bool) {
        if anchor.is_empty() {
            return;
        }
        let entry = self.authors.entry(anchor.to_vec()).or_default();
        if verified {
            entry.verified = true;
        }
    }

    /// Clamp one content's selection in EVERY region (the data the
    /// regions render changed wholesale; the view state follows).
    fn clamp_sels(&mut self, kind: ContentKind, n: usize) {
        for vp in &mut self.regions {
            let list = match kind {
                ContentKind::Threads => &mut vp.threads_sel,
                ContentKind::Transcript => &mut vp.transcript_sel,
                ContentKind::Authors => &mut vp.authors_sel,
                ContentKind::Mailboxes => &mut vp.mailboxes_sel,
            };
            let sel = list.selected().map(|i| i.min(n.saturating_sub(1)));
            list.select(sel.filter(|_| n > 0));
        }
    }

    /// Every transcript-viewing region follows the newest row (an
    /// arrival is an attention event — the live tail is the point).
    fn follow_transcripts(&mut self, row: usize) {
        for vp in &mut self.regions {
            if vp.content == ContentKind::Transcript {
                vp.transcript_sel.select(Some(row));
            }
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
            Evt::Frame(f) => self.on_frame(*f),
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
    // Newlines/tabs are PRINTABLE here (the drafts are multi-line);
    // they render as visible marks instead of breaking the row or
    // falling back to the hex preview.
    let printable = lossy
        .chars()
        .filter(|c| !matches!(c, '\n' | '\r' | '\t'))
        .all(|c| !c.is_control());
    let text = if printable {
        lossy
            .replace('\r', "")
            .replace('\n', " \u{23ce} ")
            .replace('\t', " \u{2423} ")
    } else {
        format!("{:02x?}", &data[..data.len().min(8)])
    };
    let count = text.chars().count();
    if count > 48 {
        format!("{}\u{2026}", text.chars().take(48).collect::<String>())
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switching_the_host_region_away_collapses_the_composer() {
        let mut app = App::new();
        // Region 2 (index 1) holds the transcript by default.
        app.summon_compose(1, false);
        assert_eq!(app.focus, Focus::Compose);
        assert!(app.show_compose);
        app.set_region_content(1, ContentKind::Mailboxes);
        assert!(!app.show_compose, "the composer's surface is gone");
        assert_eq!(app.focus, Focus::Region(1), "the focus lands back here");
    }

    #[test]
    fn region_content_activation_fires_the_refreshes() {
        let mut app = App::new();
        app.set_region_content(0, ContentKind::Authors);
        assert_eq!(app.regions[0].content, ContentKind::Authors);
        assert!(
            app.outbox
                .iter()
                .any(|f| matches!(f.method, Some(Method::IdentitiesGet(_))))
        );
        app.outbox.clear();
        app.set_region_content(0, ContentKind::Mailboxes);
        assert!(app.outbox.iter().any(|f| matches!(
            f.method,
            Some(Method::PeersGet(_)) | Some(Method::SelfGet(_)) | Some(Method::IdentitiesGet(_))
        )));
    }

    #[test]
    fn opening_a_thread_turns_the_region_into_its_transcript() {
        let mut app = App::new();
        app.threads = vec![MsgRef {
            ver_major: 1,
            ver_minor: 0,
            algo: "ed25519".into(),
            digest: vec![1, 2, 3],
        }];
        app.regions[0].threads_sel.select(Some(0));
        app.open_selected_thread(0);
        assert_eq!(app.regions[0].content, ContentKind::Transcript);
        assert!(
            app.outbox
                .iter()
                .any(|f| matches!(f.method, Some(Method::MsgThreadsLogGet(_))))
        );
        assert!(app.open_thread.is_some());
    }

    #[test]
    fn a_wholesale_data_change_clamps_every_regions_view() {
        let mut app = App::new();
        // Both regions show threads; one sits on a now-gone row.
        app.set_region_content(1, ContentKind::Threads);
        app.regions[0].threads_sel.select(Some(5));
        app.regions[1].threads_sel.select(Some(9));
        app.clamp_sels(ContentKind::Threads, 2);
        assert_eq!(app.regions[0].threads_sel.selected(), Some(1));
        assert_eq!(app.regions[1].threads_sel.selected(), Some(1));
        // The empty set clears the selection everywhere.
        app.clamp_sels(ContentKind::Threads, 0);
        assert_eq!(app.regions[0].threads_sel.selected(), None);
    }

    #[test]
    fn the_mode_number_selects_and_closes() {
        let mut app = App::new();
        app.mode_menu = true;
        // Route a '3' through the key handler: authors, and closed.
        app.on_key(ratatui::crossterm::event::KeyEvent::from(
            ratatui::crossterm::event::KeyCode::Char('3'),
        ));
        assert!(!app.mode_menu);
        assert_eq!(app.regions[0].content, ContentKind::Authors);
    }
}
