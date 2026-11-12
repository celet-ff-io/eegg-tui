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
    MsgThreadDelete, MsgThreadLogGet, MsgThreadsGet, Peer, PeerSet, PeersGet, SelfGet,
    incoming_frame::Method, outgoing_frame::Frame as Out,
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

    /// The position in the mode order (1..4).
    pub fn index(self) -> usize {
        match self {
            ContentKind::Threads => 0,
            ContentKind::Transcript => 1,
            ContentKind::Authors => 2,
            ContentKind::Mailboxes => 3,
        }
    }
}

/// The temp window's view index: the dispatch table reads it
/// through `viewport`, so the temp window has full dispatch
/// parity with a region (ruling 10) without being a region.
pub(crate) const TEMP_VIEW: usize = usize::MAX;

/// The window lattice: WHICH of the four lattice slots hold a
/// window, and the two split ratios. The slots are POSITIONS —
/// 1 top-left, 2 top-right, 3 bottom-left, 4 bottom-right — and
/// they never renumber: closing a window leaves a hole (rendered
/// dark), opening one fills the position back. The regions vector
/// holds the ACTIVE windows in reading order (slot 1, 2, 3, 4);
/// its length always equals the active count.
#[derive(Debug)]
pub struct Lattice {
    /// The left column's share of the base width (0.0..1.0).
    pub column: f64,
    /// The top row's share of the base height (0.0..1.0).
    pub row: f64,
    /// Slots 1..4 hold a window.
    pub active: [bool; 4],
}

impl Default for Lattice {
    fn default() -> Self {
        // The first screen: the threads list on the narrow left,
        // the transcript on the wide right, the bottom row empty
        // (it collapses to zero until a window fills it).
        Self {
            column: 0.30,
            row: 0.5,
            active: [true, true, false, false],
        }
    }
}

/// The resize step (the ratio chords move the split by this
/// fraction of the base). A named constant today; the config door
/// is declared — a per-user step moves it into the layout config
/// when a real demand shows up.
const RESIZE_STEP: f64 = 0.05;

/// The resize/zoom floors of the lattice: the render guard dims
/// any quadrant below these (ui.rs owns the geometry).
const RATIO_FLOOR: f64 = 0.05;

/// The chord focus arms' directions (the nvim window moves).
#[derive(Clone, Copy)]
enum Dir {
    Left,
    Right,
    Up,
    Down,
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
    /// The temp window (a modal overlay — it owns the keys while
    /// it is up; closing returns focus to the origin region).
    Temp,
}

#[derive(Debug)]
pub struct App {
    pub focus: Focus,
    /// The regions (the ACTIVE lattice windows in reading order;
    /// the layout engine grows the vector without a rewrite).
    pub regions: Vec<Viewport>,
    /// The window lattice: which slots hold a window + the split
    /// ratios. The slots are positions (never renumbered) — this
    /// is layout METADATA, the regions vector is its reading-order
    /// projection.
    pub lattice: Lattice,
    /// The window-op prefix: armed by Ctrl+W outside the typing
    /// layer; the next event completes a chord or cancels.
    pub prefix_armed: bool,
    /// The zoom lens: the focused slot fills the base while the
    /// lattice (set + ratios) stays frozen underneath. Any focus
    /// move or geometry op ends it.
    pub zoom: bool,
    /// The fresh slot: a just-created window whose FIRST face is
    /// the mode picker — going back then closes the slot (the temp
    /// window's fresh-until-pick pattern, in-layout).
    pub fresh_slot: Option<usize>,
    /// The base area at the last render (the too-small guard for
    /// the window ops runs against the last known geometry).
    pub last_base: (u16, u16),
    /// The composer renders inside its HOST region's transcript —
    /// fixed at summon time.
    pub compose_host: usize,
    /// The mode: entering it turns the focused view into the
    /// selection surface — a modal that eats every key until
    /// closed. `mode_sel` is the highlighted entry (j/k move it,
    /// Enter opens it, 1-4 jump direct).
    pub mode_menu: bool,
    pub mode_sel: usize,
    /// The fingerprint popup: Some(full form) while shown — a
    /// frozen snapshot of the selected identity's anchor at open
    /// time (the popup eats every key, so the selection cannot
    /// drift under it). Esc/m/q close; c copies the full form.
    pub fp_shown: Option<String>,
    /// The temp window: at most one (ruling 9). A modal overlay —
    /// the popups stack over it, Esc pops it back to the origin
    /// region. `temp_fresh` = the picker is its first face (no
    /// content chosen yet; going back then closes the window).
    pub temp: Option<Viewport>,
    pub temp_origin: usize,
    pub temp_fresh: bool,
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
    /// The new-thread row is armed: the threads list shows a
    /// synthetic "new thread" entry (no id yet) and the transcript
    /// windows track the fresh thread. It lands on the first sent
    /// message's commit receipt; moving the threads selection to a
    /// normal thread tears it down (n re-arms).
    pub new_thread_row: bool,
    /// The thread the transcript had open when the new-thread state
    /// armed (the teardown restores it — a fresh log fetch).
    pub pre_new_thread: Option<MsgRef>,
    /// The transcript's meta switches (the principal: the meta took
    /// too much room): t toggles the local timestamps, T the
    /// message fingerprints. Global (every transcript window).
    pub show_ts: bool,
    pub show_fp: bool,
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
            lattice: Lattice::default(),
            prefix_armed: false,
            zoom: false,
            fresh_slot: None,
            last_base: (0, 0),
            compose_host: 1,
            mode_menu: false,
            mode_sel: 0,
            fp_shown: None,
            temp: None,
            temp_origin: 0,
            temp_fresh: false,
            threads: Vec::new(),
            entries: Vec::new(),
            open_thread: None,
            draft: Draft::new(),
            show_compose: false,
            cursor: Vec::new(),
            new_thread_intent: false,
            new_thread_row: false,
            pre_new_thread: None,
            show_ts: true,
            show_fp: false,
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
        self.unzoom();
        self.focus = match self.focus {
            Focus::Region(0) => Focus::Region(1),
            _ => Focus::Region(0),
        };
    }

    /// The reverse cycle (Ctrl+k): region 2 → region 1 → region 2.
    fn cycle_focus_back(&mut self) {
        self.unzoom();
        self.focus = match self.focus {
            Focus::Region(0) => Focus::Region(1),
            _ => Focus::Region(0),
        };
    }

    /// Direct structural jumps: to a numbered region (Ctrl+h/l, the
    /// arrows; the composer's moving mode rides the same layer).
    fn nav_to_region(&mut self, region: usize) {
        self.unzoom();
        self.focus = Focus::Region(region.min(self.regions.len().saturating_sub(1)));
    }

    /// End the zoom lens (the uniform rule: any focus move or
    /// geometry op reveals the frozen layout first).
    fn unzoom(&mut self) {
        self.zoom = false;
    }

    /// The focused REGION's index (None = the temp window holds the
    /// focus — it has no slot; the slot-class window ops refuse).
    fn region_focus(&self) -> Option<usize> {
        match self.focus {
            Focus::Region(i) => Some(i.min(self.regions.len().saturating_sub(1))),
            Focus::Temp | Focus::Compose => None,
        }
    }

    /// The slot number (1..4) of the window at region index `i` —
    /// the NUMBER the titles and the jumps speak. The temp window
    /// is unnumbered (it has no slot).
    pub(crate) fn slot_number(&self, i: usize) -> usize {
        let mut seen = 0;
        for (p, active) in self.lattice.active.iter().enumerate() {
            if *active {
                seen += 1;
                if seen == i + 1 {
                    return p + 1;
                }
            }
        }
        unreachable!("the regions vector and the lattice disagree")
    }

    /// The region index of the window numbered `n` (1..4), if that
    /// lattice slot holds one.
    pub(crate) fn slot_index(&self, n: usize) -> Option<usize> {
        if !(1..=4).contains(&n) || !self.lattice.active[n - 1] {
            return None;
        }
        Some(self.lattice.active[..n - 1].iter().filter(|a| **a).count())
    }

    /// The numbers address LATTICE POSITIONS: jumping to a number
    /// focuses the window at that slot; an empty slot refuses (a
    /// number never creates or destroys a window).
    fn nav_to_number(&mut self, n: usize) {
        match self.slot_index(n) {
            Some(i) => self.nav_to_region(i),
            None => self.flash = Some(format!("window {n} is empty")),
        }
    }

    // -----------------------------------------------------------------
    // The window-op chord family (Ctrl+W prefix): create/close/zoom/
    // resize/focus — the nvim window-command vocabulary over the
    // lattice. The prefix arms in the region- and temp-focused
    // dispatches; the composer's typing layer keeps Ctrl+W = the
    // word drop (the nvim layering).

    /// The armed state's next event: a valid second stroke executes
    /// and disarms; Esc and the prefix again cancel silently;
    /// anything else cancels with a flash (the nvim-beep analog).
    fn on_key_chord(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        use ratatui::crossterm::event::{KeyCode, KeyModifiers};
        self.prefix_armed = false; // every path completes or cancels
        match key.code {
            KeyCode::Esc => {}
            KeyCode::Char('w') if key.modifiers == KeyModifiers::CONTROL => {}
            KeyCode::Char('v') => self.chord_create(false),
            KeyCode::Char('s') => self.chord_create(true),
            KeyCode::Char('c') => self.chord_close(),
            KeyCode::Char('z') => self.chord_zoom(),
            // The mode over the focused VIEWPORT (the temp included:
            // the viewport-class op survives the temp's isolation).
            KeyCode::Char('m') => self.open_mode_menu(),
            // The nvim-exact focus arms (the reserved class): bare
            // h/j/k/l and their Ctrl- forms both work.
            KeyCode::Char('h') => self.chord_focus_dir(Dir::Left),
            KeyCode::Char('l') => self.chord_focus_dir(Dir::Right),
            KeyCode::Char('j') => self.chord_focus_dir(Dir::Down),
            KeyCode::Char('k') => self.chord_focus_dir(Dir::Up),
            KeyCode::Char('=') => self.chord_equalize(),
            KeyCode::Char('<') => self.chord_resize_column(false),
            KeyCode::Char('>') => self.chord_resize_column(true),
            KeyCode::Char('+') => self.chord_resize_row(true),
            KeyCode::Char('-') => self.chord_resize_row(false),
            _ => self.flash = Some("the window op canceled".into()),
        }
    }

    /// `Ctrl+W v/s`: activate the focused slot's RIGHT (v) or BELOW
    /// (s) neighbor; the new slot's first face = the mode picker,
    /// fresh-until-pick. Refusals: no neighbor there, the neighbor
    /// already open ("layout full" at 4/4), or the terminal too
    /// small for the target's floors — the last one SILENT (the
    /// render rule: an unfittable slot never opens, no flash).
    fn chord_create(&mut self, below: bool) {
        self.unzoom();
        let Some(i) = self.region_focus() else {
            self.flash = Some("focus a slot first".into());
            return;
        };
        let n = self.slot_number(i);
        let (row, col) = ((n - 1) / 2, (n - 1) % 2);
        let (tr, tc) = if below {
            (row + 1, col)
        } else {
            (row, col + 1)
        };
        if tr > 1 || tc > 1 {
            self.flash = Some(
                if below {
                    "no window below"
                } else {
                    "no window to the right"
                }
                .into(),
            );
            return;
        }
        let t = tr * 2 + tc;
        if self.lattice.active[t] {
            self.flash = Some(if self.lattice.active.iter().filter(|a| **a).count() == 4 {
                "layout full".into()
            } else {
                format!("window {} is already open", t + 1)
            });
            return;
        }
        // The silent too-small guard: probe the target's quadrant
        // under the would-be active set against the last known
        // geometry; below the floors the chord does nothing.
        let mut probe = self.lattice.active;
        probe[t] = true;
        let base = ratatui::prelude::Rect::new(0, 0, self.last_base.0, self.last_base.1);
        let fits = crate::ui::lattice_rects(base, self.lattice.column, self.lattice.row, probe)[t]
            .is_some_and(|q| q.width >= crate::ui::MIN_SLOT_W && q.height >= crate::ui::MIN_SLOT_H);
        if !fits {
            return;
        }
        // Activate: the regions vector gains the window at its
        // reading-order position; the picker is its first face.
        self.lattice.active[t] = true;
        let at = self.lattice.active[..t].iter().filter(|a| **a).count();
        self.regions.insert(at, Viewport::new(ContentKind::Threads));
        self.focus = Focus::Region(at);
        self.fresh_slot = Some(at);
        self.mode_menu = true;
        self.mode_sel = 0;
    }

    /// `Ctrl+W c`: close the FOCUSED slot. The ids are positions —
    /// no renumbering; the last window refuses (quit stays the
    /// explicit q); the focus lands on the nearest active slot in
    /// reading order; the draft survives (the composer hides to
    /// standby; i re-summons from any transcript window).
    fn chord_close(&mut self) {
        self.unzoom();
        let Some(i) = self.region_focus() else {
            self.flash = Some("focus a slot first".into());
            return;
        };
        self.close_slot_index(i);
    }

    /// Close the window at region index `i` (the chord close and
    /// the fresh picker's back share this path).
    fn close_slot_index(&mut self, i: usize) {
        let live = self.lattice.active.iter().filter(|a| **a).count();
        if live <= 1 {
            self.flash = Some("the last window stays".into());
            return;
        }
        let n = self.slot_number(i);
        // The nearest active slot in reading order: the predecessor,
        // else the successor (the reading order never shifts).
        let target_slot = (0..n - 1)
            .rev()
            .find(|&p| self.lattice.active[p])
            .or_else(|| (n..4).find(|&p| self.lattice.active[p]))
            .map(|p| p + 1)
            .expect("checked: more than one slot live");
        // Index-based bookkeeping re-homes through slot numbers
        // BEFORE the mutation (the indices shift with the removal).
        let host_slot = (self.compose_host != TEMP_VIEW && self.compose_host < self.regions.len())
            .then(|| self.slot_number(self.compose_host));
        let origin_slot =
            (self.temp_origin < self.regions.len()).then(|| self.slot_number(self.temp_origin));
        self.lattice.active[n - 1] = false;
        self.regions.remove(i);
        if self.show_compose && self.compose_host == i {
            // Standby, not destroyed: the draft is construction
            // state and survives the window.
            self.show_compose = false;
        }
        if let Some(s) = host_slot
            .filter(|s| self.lattice.active[s - 1])
        {
            self.compose_host = self.slot_index(s).expect("checked active");
        }
        if let Some(s) = origin_slot {
            let alive = self.lattice.active[s - 1];
            let s = if alive { s } else { target_slot };
            self.temp_origin = self.slot_index(s).expect("target is active");
        }
        self.focus = Focus::Region(self.slot_index(target_slot).expect("target is active"));
        self.fresh_slot = None;
    }

    /// `Ctrl+W z`: the zoom lens toggle (tmux mnemonic). The
    /// focused slot fills the base; the lattice freezes underneath
    /// and comes back exactly.
    fn chord_zoom(&mut self) {
        if self.region_focus().is_none() {
            self.flash = Some("focus a slot first".into());
            return;
        }
        self.zoom = !self.zoom;
    }

    /// The focus arms: the nearest active slot in that direction
    /// along the focused slot's row/column; the edge refuses (the
    /// nvim edge-beep analog).
    fn chord_focus_dir(&mut self, dir: Dir) {
        self.unzoom();
        let Some(i) = self.region_focus() else {
            self.flash = Some("focus a slot first".into());
            return;
        };
        let n = self.slot_number(i);
        let (row, col) = ((n - 1) / 2, (n - 1) % 2);
        let scan: Box<dyn Iterator<Item = usize>> = match dir {
            Dir::Left => Box::new((0..col).rev()),
            Dir::Right => Box::new(col + 1..2),
            Dir::Up => Box::new((0..row).rev()),
            Dir::Down => Box::new(row + 1..2),
        };
        let (vary_row, word) = match dir {
            Dir::Left => (false, "to the left"),
            Dir::Right => (false, "to the right"),
            Dir::Up => (true, "above"),
            Dir::Down => (true, "below"),
        };
        let target = scan
            .map(|v| if vary_row { (v, col) } else { (row, v) })
            .map(|(r, c)| r * 2 + c)
            .find(|&p| self.lattice.active[p]);
        match target {
            Some(p) => {
                let idx = self.slot_index(p + 1).expect("checked active");
                self.nav_to_region(idx);
            }
            None => self.flash = Some(format!("no window {word}")),
        }
    }

    /// The column ratio only means something when BOTH columns are
    /// live; a collapsed column has nothing to resize (flash).
    fn chord_resize_column(&mut self, widen: bool) {
        self.unzoom();
        let Some(i) = self.region_focus() else {
            self.flash = Some("focus a slot first".into());
            return;
        };
        let n = self.slot_number(i);
        let col = (n - 1) % 2;
        let left_live = self.lattice.active[0] || self.lattice.active[2];
        let right_live = self.lattice.active[1] || self.lattice.active[3];
        if !(left_live && right_live) {
            self.flash = Some("no column split".into());
            return;
        }
        // '>' widens the focused slot's column: on the left that
        // grows the share, on the right it shrinks it.
        let grow = (col == 0) == widen;
        let next = self.lattice.column + if grow { RESIZE_STEP } else { -RESIZE_STEP };
        if next < RATIO_FLOOR || next > 1.0 - RATIO_FLOOR {
            self.flash = Some("no room".into());
            return;
        }
        self.lattice.column = next.clamp(RATIO_FLOOR, 1.0 - RATIO_FLOOR);
    }

    /// The row ratio only means something when BOTH rows are live.
    fn chord_resize_row(&mut self, grow: bool) {
        self.unzoom();
        let Some(i) = self.region_focus() else {
            self.flash = Some("focus a slot first".into());
            return;
        };
        let n = self.slot_number(i);
        let top_live = self.lattice.active[0] || self.lattice.active[1];
        let bottom_live = self.lattice.active[2] || self.lattice.active[3];
        if !(top_live && bottom_live) {
            self.flash = Some("no row split".into());
            return;
        }
        // '+' grows the focused slot's row: on the top that grows
        // the top share, on the bottom it shrinks it.
        let grow_share = ((n - 1) / 2 == 0) == grow;
        let next = self.lattice.row
            + if grow_share {
                RESIZE_STEP
            } else {
                -RESIZE_STEP
            };
        if next < RATIO_FLOOR || next > 1.0 - RATIO_FLOOR {
            self.flash = Some("no room".into());
            return;
        }
        self.lattice.row = next.clamp(RATIO_FLOOR, 1.0 - RATIO_FLOOR);
    }

    /// `Ctrl+W =`: both ratios back to 50/50 (the stored values
    /// matter even under a collapse — they return with the slot).
    fn chord_equalize(&mut self) {
        self.unzoom();
        if self.region_focus().is_none() {
            self.flash = Some("focus a slot first".into());
            return;
        }
        self.lattice.column = 0.5;
        self.lattice.row = 0.5;
    }

    /// The focused region's index (the composer has none — its host
    /// region stands in for view-state purposes).
    pub(crate) fn focus_region(&self) -> usize {
        match self.focus {
            Focus::Region(i) => i.min(self.regions.len().saturating_sub(1)),
            Focus::Temp => TEMP_VIEW,
            Focus::Compose => self.compose_host,
        }
    }

    /// Any VIEW by index: a region (0/1) or the temp window
    /// (TEMP_VIEW) — one dispatch table for both (ruling 10).
    pub(crate) fn viewport(&self, i: usize) -> &Viewport {
        if i == TEMP_VIEW {
            self.temp.as_ref().expect("the temp viewport")
        } else {
            &self.regions[i]
        }
    }

    pub(crate) fn viewport_mut(&mut self, i: usize) -> &mut Viewport {
        if i == TEMP_VIEW {
            self.temp.as_mut().expect("the temp viewport")
        } else {
            &mut self.regions[i]
        }
    }

    /// Focus a VIEW: a region (0/1) or the temp window (TEMP_VIEW).
    fn set_focus_view(&mut self, i: usize) {
        self.focus = if i == TEMP_VIEW {
            Focus::Temp
        } else {
            Focus::Region(i)
        };
    }

    /// f: show the selected identity's FULL fingerprint (the F1
    /// full notation — the human cross-check channel). The mailboxes
    /// self row has no roster anchor (the core holds that key): the
    /// card channels (c/s) are the verification path there.
    fn open_fp_popup(&mut self) {
        let anchor = match self.viewport(self.focus_region()).content {
            ContentKind::Authors => self.selected_author(),
            // The transcript's people are their messages: f = the
            // SELECTED row's author's full fingerprint.
            ContentKind::Transcript => {
                let from = self
                    .viewport(self.focus_region())
                    .transcript_sel
                    .selected()
                    .and_then(|i| self.entries.get(i))
                    .and_then(|e| e.meta.as_ref())
                    .map(|m| m.from.clone())
                    .unwrap_or_default();
                if from.is_empty() {
                    self.flash = Some("the selected row has no author".into());
                }
                (!from.is_empty()).then_some(from)
            }
            ContentKind::Mailboxes => {
                match self.viewport(self.focus_region()).mailboxes_sel.selected() {
                    Some(i) if i > 0 => self.peers.get(i - 1).map(anchor_of),
                    _ => {
                        self.flash = Some(
                            "the self row: verify through the card (c/s — the core holds the key)"
                                .into(),
                        );
                        return;
                    }
                }
            }
            _ => None,
        };
        match anchor {
            Some(a) if a.len() == zconned_crypto::DIGEST_LEN => {
                self.fp_shown = Some(fp_full(&a));
            }
            _ => self.flash = Some("no identity selected".into()),
        }
    }

    /// m OUTSIDE the modes picker: switch the focused view to the
    /// modes picker (the principal — m's meaning is "switch to the
    /// mode-select state"; only INSIDE the picker does m mean
    /// back). The fresh temp starts at the top; else at the
    /// current kind.
    fn open_mode_menu(&mut self) {
        self.mode_menu = true;
        self.mode_sel =
            if (self.focus == Focus::Temp && self.temp_fresh) || self.fresh_slot.is_some() {
                0
            } else {
                self.viewport(self.focus_region()).content.index()
            };
    }

    /// Open the temp window (x): a modal overlay whose FIRST face
    /// is the modes picker (the principal — the quick profile is
    /// open-temp + pick; `p` is retired). Focus memory: the origin
    /// region is remembered for the pop-back (ruling 11).
    fn open_temp(&mut self) {
        self.temp_origin = self.focus_region();
        self.temp = Some(Viewport::new(ContentKind::Threads));
        self.temp_fresh = true;
        self.mode_menu = true;
        self.mode_sel = 0;
        self.focus = Focus::Temp;
    }

    /// Close the temp window: focus returns to the origin region
    /// (ruling 11; the layout and the regions are untouched). A
    /// composer hosted by the temp collapses with it (its surface
    /// is gone) and its draft goes with the window.
    fn close_temp(&mut self) {
        if self.compose_host == TEMP_VIEW {
            self.show_compose = false;
            self.draft.clear();
            self.compose_host = self.temp_origin;
        }
        self.mode_menu = false;
        self.temp = None;
        self.temp_fresh = false;
        self.focus = Focus::Region(self.temp_origin);
    }

    /// PgUp/PgDn: the selection moves by a page (the focused
    /// region's last rendered transcript viewport height; a sane
    /// page before first paint).
    fn page_sel(&mut self, dir: isize) {
        let page = (self.viewport(self.focus_region()).page_rows.max(2) as isize) - 1;
        self.move_sel(dir * page);
    }

    fn move_sel(&mut self, delta: isize) {
        let fi = self.focus_region();
        let kind = self.viewport(fi).content;
        let len = match kind {
            ContentKind::Threads => self.threads.len() + usize::from(self.new_thread_row),
            ContentKind::Transcript => self.entries.len(),
            ContentKind::Authors => self.authors.len(),
            ContentKind::Mailboxes => self.peers.len() + 1,
        };
        let list = self.viewport_mut(fi).sel_mut();
        let Some(i) = list.selected() else {
            if len > 0 {
                list.select(Some(0));
            }
            return;
        };
        let next = (i as isize + delta).clamp(0, len.saturating_sub(1) as isize) as usize;
        list.select(Some(next));
        self.new_thread_row_guard();
    }

    /// The new-thread row is armed: any threads selection that
    /// LEAVES it (any row but the top) tears the state down — the
    /// row vanishes and the transcript returns to the pre-n thread.
    fn new_thread_row_guard(&mut self) {
        if !self.new_thread_row {
            return;
        }
        let fi = self.focus_region();
        if self.viewport(fi).content != ContentKind::Threads {
            return;
        }
        if self
            .viewport(fi)
            .threads_sel
            .selected()
            .is_some_and(|i| i > 0)
        {
            self.cancel_new_thread();
        }
    }

    /// The active list's length (for g/G jump-to-end).
    fn sel_len(&self) -> usize {
        let fi = self.focus_region();
        match self.viewport(fi).content {
            ContentKind::Threads => self.threads.len() + usize::from(self.new_thread_row),
            ContentKind::Transcript => self.entries.len(),
            ContentKind::Authors => self.authors.len(),
            ContentKind::Mailboxes => self.peers.len() + 1,
        }
    }

    /// g/Home: jump the active list's selection to the top.
    fn sel_home(&mut self) {
        let fi = self.focus_region();
        let has = self.sel_len() > 0;
        self.viewport_mut(fi)
            .sel_mut()
            .select(if has { Some(0) } else { None });
    }

    /// G/End: jump the active list's selection to the bottom.
    fn sel_end(&mut self) {
        let fi = self.focus_region();
        let n = self.sel_len();
        self.viewport_mut(fi)
            .sel_mut()
            .select(if n > 0 { Some(n - 1) } else { None });
        self.new_thread_row_guard();
    }

    /// Switch a view's content (the mode's 1..4, opening a
    /// thread). Switching the composer's host view away from the
    /// transcript collapses the composer — its surface is gone.
    fn set_region_content(&mut self, region: usize, kind: ContentKind) {
        let region = if region == TEMP_VIEW {
            TEMP_VIEW
        } else {
            region.min(self.regions.len().saturating_sub(1))
        };
        // A pick ends the fresh-until-pick state (the fresh slot's
        // content is now real; its back rows read the chosen page).
        // NOTE: before the same-content early return — picking the
        // slot's standing content is still a pick.
        self.fresh_slot = None;
        if self.viewport(region).content == kind {
            return;
        }
        if self.show_compose && self.compose_host == region {
            self.show_compose = false;
            if self.focus == Focus::Compose {
                self.set_focus_view(region);
            }
        }
        self.viewport_mut(region).content = kind;
        match kind {
            // One refresh per activation (cheap; the sets are small).
            ContentKind::Mailboxes => {
                self.viewport_mut(region).mailboxes_sel.select(Some(0));
                self.refresh_peers();
                self.refresh_self();
                self.refresh_identities();
            }
            // A fresh wholesale get per activation of the registry.
            ContentKind::Authors => {
                self.viewport_mut(region).authors_sel.select(Some(0));
                self.refresh_identities();
            }
            _ => {}
        }
        if region == TEMP_VIEW {
            // Content chosen: the temp window is no longer fresh
            // (its back entries now mean the chosen page).
            self.temp_fresh = false;
        }
    }

    /// Enter on a threads region: fetch the selected thread's log
    /// (reads are cursor-independent — browsing never moves the
    /// anchor) and turn the region into its transcript.
    /// n: the new-thread state. The threads list gains a synthetic
    /// "new thread" row (no id yet — styled apart) at the TOP and
    /// the selection lands on it; the transcript windows track the
    /// fresh (empty) thread; the composer stands by in the first
    /// transcript window WITHOUT taking focus. The first sent
    /// message's commit receipt lands the thread (the row
    /// vanishes); moving the selection to a normal thread tears the
    /// state down (n re-arms). Entered from inside a transcript
    /// (n there), the composer goes straight to typing.
    fn begin_new_thread(&mut self, from_transcript: bool) {
        if from_transcript {
            let fi = self.focus_region();
            self.show_compose = true;
            self.compose_host = fi;
            self.focus = Focus::Compose;
        }
        if self.new_thread_row {
            return; // idempotent; the typing entry already handled
        }
        self.pre_new_thread = self.open_thread.clone();
        self.new_thread_row = true;
        self.new_thread_intent = true;
        // The transcript windows track the fresh thread: empty.
        self.entries.clear();
        self.locals.clear();
        self.open_thread = None;
        self.follow_transcripts(0);
        // Every threads view snaps onto the synthetic row.
        for vp in self.regions.iter_mut().chain(self.temp.iter_mut()) {
            if vp.content == ContentKind::Threads {
                vp.threads_sel.select(Some(0));
            }
        }
        // Standby composer in the first transcript window (regions
        // first, then the temp window) — no focus move.
        if !from_transcript {
            let host = (0..self.regions.len())
                .find(|&r| self.regions[r].content == ContentKind::Transcript)
                .or_else(|| {
                    (self
                        .temp
                        .as_ref()
                        .is_some_and(|vp| vp.content == ContentKind::Transcript))
                    .then_some(TEMP_VIEW)
                });
            if let Some(h) = host {
                self.show_compose = true;
                self.compose_host = h;
            }
        }
        self.flash = Some("new thread".into());
    }

    /// The new-thread state tears down (the selection moved to a
    /// normal thread): the row vanishes, the wire intent clears,
    /// the standby composer hides, and the transcript returns to
    /// the pre-n thread (a fresh log fetch when there was one).
    fn cancel_new_thread(&mut self) {
        if !self.new_thread_row {
            return;
        }
        self.new_thread_row = false;
        self.new_thread_intent = false;
        self.show_compose = false;
        self.entries.clear();
        self.locals.clear();
        self.open_thread = self.pre_new_thread.take();
        if let Some(t) = self.open_thread.clone() {
            self.outbox.push(IncomingFrame {
                method: Some(Method::MsgThreadLogGet(MsgThreadLogGet { thread: Some(t) })),
            });
        }
        self.follow_transcripts(0);
        self.flash = Some("new thread dropped".into());
    }

    /// Enter in the threads content: the selected thread shows in
    /// the OTHER transcript windows (the threads window itself
    /// never flips its content — the old hijack is retired); with
    /// no transcript window anywhere, Enter does nothing. No focus
    /// move (a preview key).
    fn enter_from_threads(&mut self) {
        let any_transcript = self
            .regions
            .iter()
            .any(|r| r.content == ContentKind::Transcript)
            || self
                .temp
                .as_ref()
                .is_some_and(|vp| vp.content == ContentKind::Transcript);
        if !any_transcript {
            return;
        }
        // The synthetic row: the tracking is already active.
        let Some(Some(thread)) = self.selected_thread() else {
            return;
        };
        let was_armed = self.new_thread_row;
        self.entries.clear();
        self.locals.clear();
        self.open_thread = Some(thread);
        self.outbox.push(IncomingFrame {
            method: Some(Method::MsgThreadLogGet(MsgThreadLogGet {
                thread: self.open_thread.clone(),
            })),
        });
        self.follow_transcripts(0);
        // Adopting a normal thread tears the new-thread state down.
        if was_armed {
            self.new_thread_row = false;
            self.new_thread_intent = false;
            self.show_compose = false;
            self.pre_new_thread = None;
        }
    }

    /// d in the threads content: delete the SELECTED thread (B8 —
    /// a point op, LOCAL store curation; counterparts keep their
    /// copies). The synthetic new-thread row has nothing to delete
    /// (a no-op). The receipt is the wholesale threads push, which
    /// re-renders the list and drops an open deleted thread's
    /// transcript view (an arriving mail re-grows the thread —
    /// expect resurrection; a mute is a future feature).
    fn delete_selected_thread(&mut self) {
        let Some(Some(thread)) = self.selected_thread() else {
            return; // no selection, or the synthetic row
        };
        self.outbox.push(IncomingFrame {
            method: Some(Method::MsgThreadDelete(MsgThreadDelete {
                thread: Some(thread),
            })),
        });
        self.flash = Some("deleting thread".into());
    }

    /// The threads selection resolved against the synthetic row:
    /// None = no selection; Some(None) = the new-thread row;
    /// Some(Some(t)) = a real thread.
    fn selected_thread(&self) -> Option<Option<MsgRef>> {
        let i = self.viewport(self.focus_region()).threads_sel.selected()?;
        if self.new_thread_row && i == 0 {
            return Some(None);
        }
        self.threads
            .get(i - usize::from(self.new_thread_row))
            .cloned()
            .map(Some)
    }

    /// The selected author's anchor (BTreeMap order = anchor order).
    fn selected_author(&self) -> Option<Vec<u8>> {
        let i = self.viewport(self.focus_region()).authors_sel.selected()?;
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
        let i = self
            .viewport(self.focus_region())
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
        match self.viewport(self.focus_region()).content {
            ContentKind::Authors => self
                .selected_author()
                .and_then(|a| self.peers.iter().find(|p| anchor_of(p) == a).map(card_text)),
            ContentKind::Mailboxes => {
                match self.viewport(self.focus_region()).mailboxes_sel.selected() {
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
            // The temp window is modal: a composer hosted by it has
            // no structural way out (the Esc stack pops: input ->
            // temp -> origin region).
            if self.compose_host == TEMP_VIEW {
                return;
            }
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
            KeyCode::Esc => self.set_focus_view(self.compose_host),
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
        self.set_focus_view(self.compose_host);
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
                // m switches to the modes picker (its meaning
                // everywhere outside the picker itself).
                KeyCode::Char('m') => {
                    self.show_help = false;
                    self.help_scroll = 0;
                    self.open_mode_menu();
                }
                _ => {}
            }
            return;
        }

        if self.file_box != FileBox::None {
            self.on_key_file_box(key);
            return;
        }

        // The mode: the WHOLE window is the selection surface —
        // j/k move the highlight, Enter opens it, 1..4 jump
        // direct, Esc/m goes back without change, q quits (its
        // global meaning, listed in the modes frame too).
        if self.mode_menu {
            match key.code {
                KeyCode::Char(c @ ('1' | '2' | '3' | '4')) => {
                    let n = c.to_digit(10).expect("checked") as usize - 1;
                    self.mode_menu = false;
                    self.set_region_content(self.focus_region(), ContentKind::nth(n));
                }
                KeyCode::Char('j') | KeyCode::Down => self.mode_sel = (self.mode_sel + 1).min(3),
                KeyCode::Char('k') | KeyCode::Up => self.mode_sel = self.mode_sel.saturating_sub(1),
                KeyCode::Enter => {
                    self.mode_menu = false;
                    self.set_region_content(self.focus_region(), ContentKind::nth(self.mode_sel));
                }
                // Back: q shares the m/Esc semantics (the row sits
                // between them in the frame — it is NOT quit here;
                // the global q-quit stays on the main surfaces).
                KeyCode::Esc | KeyCode::Char('m') | KeyCode::Char('q') => {
                    if self.focus == Focus::Temp && self.temp_fresh {
                        // The picker is the temp window's first face:
                        // going back means closing the window (no
                        // content created).
                        self.close_temp();
                    } else if let Some(f) = self.fresh_slot.take() {
                        // The fresh slot's picker back = close the
                        // fresh slot: nothing was created yet.
                        if self.focus == Focus::Region(f) {
                            self.close_slot_index(f);
                        }
                        self.mode_menu = false;
                    } else {
                        self.mode_menu = false;
                    }
                }
                _ => {}
            }
            return;
        }

        // The armed window-op prefix eats every key until a chord
        // completes or cancels (no overlay can open meanwhile).
        if self.prefix_armed {
            self.on_key_chord(key);
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
                // m switches to the modes picker (its meaning
                // everywhere outside the picker itself).
                KeyCode::Char('m') => {
                    self.show_log = false;
                    self.log_scroll = 0;
                    self.open_mode_menu();
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

        // The fingerprint popup: the selected identity's FULL form
        // (a frozen snapshot). Esc/q close (q follows the log
        // popup's precedent: a stray q quits from the main surface,
        // never from inside a popup); c copies the full form; m
        // switches to the modes picker (m's meaning everywhere
        // outside the picker itself).
        if self.fp_shown.is_some() {
            match key.code {
                KeyCode::Char('c') => {
                    let text = self.fp_shown.clone().expect("checked");
                    self.flash = Some(clipboard::set(&text).map_or_else(
                        |reason| format!("{reason} — the full form stays on screen"),
                        |()| "full fingerprint copied".to_owned(),
                    ));
                }
                KeyCode::Char('m') => {
                    self.fp_shown = None;
                    self.open_mode_menu();
                }
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.fp_shown = None;
                }
                _ => {}
            }
            return;
        }

        // The temp window: a modal overlay (the profile-popup
        // pattern — the popups above stack over it). No structural
        // navigation out of it: Esc pops it back to the origin
        // region, q = back means closing it (a layer above the base
        // layout; the FIRST layer's back is the quit — the regions
        // below), everything else runs with full dispatch parity
        // (ruling 10).
        if self.temp.is_some() && self.focus != Focus::Compose {
            // The window-op prefix arms here too (D-Q2: arming is
            // harmless; the slot-class chords answer with the flash,
            // the viewport-class chord reaches the temp).
            if key.code == KeyCode::Char('w') && key.modifiers == KeyModifiers::CONTROL {
                self.prefix_armed = true;
                return;
            }
            match key.code {
                KeyCode::Char('?') => self.show_help = true,
                KeyCode::Char('q') => self.close_temp(),
                KeyCode::Char('L') => {
                    self.show_log = true;
                    self.log_scroll = 0;
                }
                // The picker: m opens it.
                KeyCode::Char('m') => self.open_mode_menu(),
                // Standby composer: Esc hides it first (its one
                // extra function over the default state).
                KeyCode::Esc if self.show_compose => {
                    self.show_compose = false;
                }
                KeyCode::Esc => self.close_temp(),
                KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
                KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
                KeyCode::Home | KeyCode::Char('g') => self.sel_home(),
                KeyCode::End | KeyCode::Char('G') => self.sel_end(),
                KeyCode::PageUp if self.viewport(TEMP_VIEW).content == ContentKind::Transcript => {
                    self.page_sel(-1)
                }
                KeyCode::PageDown
                    if self.viewport(TEMP_VIEW).content == ContentKind::Transcript =>
                {
                    self.page_sel(1)
                }
                _ => self.on_key_content(key),
            }
            return;
        }

        if self.focus == Focus::Compose {
            self.on_key_compose(key);
            return;
        }

        // Region focus from here. The window-op prefix arms first
        // (the nvim layering: the composer's typing layer keeps
        // Ctrl+W = word drop; the popups and the picker eat keys
        // before this line, so they never arm it).
        if key.code == KeyCode::Char('w') && key.modifiers == KeyModifiers::CONTROL {
            self.prefix_armed = true;
            return;
        }
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
            // The numbered windows: the direct jump by LATTICE SLOT
            // (an empty slot refuses with a flash; a number never
            // creates or destroys a window).
            KeyCode::Char(c @ ('1' | '2' | '3' | '4')) => {
                self.nav_to_number(c.to_digit(10).expect("checked") as usize);
            }
            // The mode: the whole-window content picker over
            // this region, the cursor starting on the current
            // content. `o` stays free for the transcript's future
            // open-file key (the multimedia era).
            KeyCode::Char('m') => self.open_mode_menu(),
            // The temp window (x): a modal overlay whose first face
            // is the modes picker — the quick profile is open-temp +
            // pick (`p` is retired).
            KeyCode::Char('x') => self.open_temp(),
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
                if self.viewport(self.focus_region()).content == ContentKind::Transcript =>
            {
                self.page_sel(-1)
            }
            KeyCode::PageDown
                if self.viewport(self.focus_region()).content == ContentKind::Transcript =>
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
        match self.viewport(fi).content {
            ContentKind::Threads => match key.code {
                KeyCode::Char('t') => self.refresh_threads(),
                // The new-thread state: the row arms, the
                // transcript windows track, the composer stands by —
                // no focus move, no window hijack.
                KeyCode::Char('n') => self.begin_new_thread(false),
                // Preview: the OTHER transcript windows switch to
                // the selected thread; with none, nothing happens.
                KeyCode::Enter => self.enter_from_threads(),
                // Delete the selected thread (B8, local store
                // curation): the receipt is the threads push.
                KeyCode::Char('d') => self.delete_selected_thread(),
                _ => {}
            },
            ContentKind::Transcript => match key.code {
                // The transcript text channels: yank to the
                // clipboard, save to a file (the path box).
                KeyCode::Char('y') => self.transcript_yank(),
                // The selected row's author's full fingerprint.
                KeyCode::Char('f') => self.open_fp_popup(),
                // The meta switches: t = the local timestamps, T =
                // the message fingerprints.
                KeyCode::Char('t') => {
                    self.show_ts = !self.show_ts;
                    self.flash = Some(if self.show_ts {
                        "timestamps on".into()
                    } else {
                        "timestamps off".into()
                    });
                }
                KeyCode::Char('T') => {
                    self.show_fp = !self.show_fp;
                    self.flash = Some(if self.show_fp {
                        "fingerprints on".into()
                    } else {
                        "fingerprints off".into()
                    });
                }
                KeyCode::Char('w') => {
                    self.file_input.reset();
                    self.file_box = FileBox::SaveTranscript;
                }
                KeyCode::Char('r') => self.refresh_draft(),
                // The same new-thread state, entered from inside a
                // transcript: the composer goes straight to typing.
                KeyCode::Char('n') => self.begin_new_thread(true),
                // Summon the composer (leaving it was Esc — symmetric
                // entry; typing starts immediately). The composer is
                // the transcript's sub-window: other contents have
                // no compose.
                KeyCode::Char('i') => self.summon_compose(fi),
                _ => {}
            },
            ContentKind::Authors => match key.code {
                // The remark edit on the selected identity (the
                // registry is the remark's authority).
                // The full fingerprint of the selected identity
                // (the human cross-check channel; c inside the
                // popup copies it).
                KeyCode::Char('f') => self.open_fp_popup(),
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
                // The selected peer's full fingerprint.
                KeyCode::Char('f') => self.open_fp_popup(),
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
    fn summon_compose(&mut self, region: usize) {
        let fi = if region == TEMP_VIEW {
            TEMP_VIEW
        } else {
            region.min(self.regions.len().saturating_sub(1))
        };
        self.set_region_content(fi, ContentKind::Transcript);
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
                self.clamp_sels(
                    ContentKind::Threads,
                    self.threads.len() + usize::from(self.new_thread_row),
                );
                // B8's receipt doubles as the delete echo: if the
                // OPEN thread is gone from the surviving list, its
                // transcript view drops to empty (the store's truth;
                // an arriving mail re-grows the thread and the
                // adoption path handles it).
                if self
                    .open_thread
                    .as_ref()
                    .is_some_and(|t| !self.threads.contains(t))
                {
                    self.entries.clear();
                    self.locals.clear();
                    self.open_thread = None;
                    self.follow_transcripts(0);
                }
            }
            Some(Out::MsgThreadLogPush(p)) => {
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
                        digest_preview(&r.digest),
                        preview_bytes(&v.data),
                        v.kind
                    ),
                    (Some(r), None) => {
                        format!("resource {} = (missing)", digest_preview(&r.digest))
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
                            method: Some(Method::MsgThreadLogGet(MsgThreadLogGet {
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
            } else if self.open_thread.is_none() && (!self.new_thread_row || receipt) {
                // Nothing open: show the row and adopt its thread (a
                // fresh commit's first prev IS the thread id — the
                // genesis parent ref). A foreign arrival never
                // hijacks the armed new-thread tracking; OUR commit
                // receipt does — the row's promise lands here.
                self.entries = vec![e.clone()];
                self.open_thread = e.prev.first().cloned();
                self.follow_transcripts(0);
                if self.new_thread_row {
                    self.new_thread_row = false;
                    self.pre_new_thread = None;
                }
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
        for vp in self.regions.iter_mut().chain(self.temp.iter_mut()) {
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
        for vp in self.regions.iter_mut().chain(self.temp.iter_mut()) {
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

/// A machine-id / content-digest head preview: the first 6 bytes,
/// continuous lowercase hex. NOT an identity fingerprint — the
/// identity display is `fp_short` (F1); the MsgRef thread-id forms
/// and the content/resource digests keep this continuous preview.
pub fn digest_preview(b: &[u8]) -> String {
    b.iter().take(6).map(|x| format!("{x:02x}")).collect()
}

/// The identity fingerprint's FULL display form (F1's ratified
/// notation): `sha3-256:<64 lowercase hex>` — the canon `Display`
/// over an already-computed digest (the pin test ties the two).
/// The human cross-check channel; verification still goes through
/// the card text.
pub fn fp_full(digest: &[u8]) -> String {
    format!(
        "{}:{}",
        zconned_crypto::DIGEST_ALGO,
        digest
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
}

/// The identity fingerprint's SHORT display form (F1 — the
/// standard fingerprint display notation memo): the digest's
/// FIRST 8 bytes, lowercase
/// hex, grouped 2 bytes per 4-hex block, space-joined (`1a2b 3c4d
/// 5e6f 7890`). DISPLAY ONLY — any verification decision uses the
/// full `sha3-256:<hex>` notation. This mirrors the canon
/// `zconned_crypto::Fingerprint::short` over an ALREADY-COMPUTED
/// digest (these display sites hold the anchor — the digest — with
/// the key long gone); the pin test ties the two forms together.
pub fn fp_short(digest: &[u8]) -> String {
    if digest.len() < 8 {
        // A malformed anchor can never produce a short form; the
        // interface renders what the core reports — an empty head.
        return String::new();
    }
    digest[..8]
        .chunks(2)
        .map(|pair| format!("{:02x}{:02x}", pair[0], pair[1]))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The FULL printable text of a text/plain row (no preview
/// truncation — the renderer wraps it at the window width). The
/// same visible marks as the preview: newlines render as an
/// arrow, tabs as an open box, \r is dropped; non-printable
/// bytes fall back to the bounded hex preview.
pub fn full_text(data: &[u8]) -> String {
    let lossy = String::from_utf8_lossy(data).into_owned();
    let printable = lossy
        .chars()
        .filter(|c| !matches!(c, '\n' | '\r' | '\t'))
        .all(|c| !c.is_control());
    if printable {
        lossy
            .replace('\r', "")
            .replace('\n', " \u{23ce} ")
            .replace('\t', " \u{2423} ")
    } else {
        let n = data.len().min(8);
        format!("{:02x?}", &data[..n])
    }
}

/// Character-level hard wrap by DISPLAY width (CJK chars count
/// double — the terminal does; unicode-width decides). One chunk
/// per line, each at most `max` columns wide. An empty text
/// yields one empty line (the row still renders).
pub fn wrap_width(text: &str, max: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthChar;
    let max = max.max(1);
    let mut lines: Vec<String> = vec![String::new()];
    let mut w = 0usize;
    for c in text.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw > max {
            lines.push(String::new());
            w = 0;
        }
        lines.last_mut().expect("seeded").push(c);
        w += cw;
    }
    lines
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
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(c: char) -> KeyEvent {
        KeyEvent::from(KeyCode::Char(c))
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn the_window_prefix_arms_completes_and_cancels() {
        let mut app = App::new();
        // The prefix arms outside the typing layer.
        app.on_key(ctrl('w'));
        assert!(app.prefix_armed);
        // The prefix again = cancel (no cycle op; Tab exists).
        app.on_key(ctrl('w'));
        assert!(!app.prefix_armed);
        assert_eq!(app.regions.len(), 2);
        // An unknown second stroke cancels with a flash.
        app.on_key(ctrl('w'));
        app.on_key(key('x'));
        assert!(!app.prefix_armed);
        assert_eq!(app.flash.as_deref(), Some("the window op canceled"));
        // Esc cancels silently (the earlier flash is stale state;
        // clear it to see the silence).
        app.flash = None;
        app.on_key(ctrl('w'));
        app.on_key(KeyEvent::from(KeyCode::Esc));
        assert!(!app.prefix_armed);
        assert!(app.flash.is_none() || app.flash.as_deref() != Some("the window op canceled"));
    }

    #[test]
    fn ctrl_w_v_creates_the_right_neighbor_fresh_until_pick() {
        let mut app = App::new();
        // The default {1,2}: slot 1's right neighbor is already open.
        app.on_key(ctrl('w'));
        app.on_key(key('v'));
        assert_eq!(app.flash.as_deref(), Some("window 2 is already open"));
        // Close slot 2, then v re-opens it as a FRESH slot: the
        // picker is its first face and its back closes the slot.
        app.focus = Focus::Region(1);
        app.on_key(ctrl('w'));
        app.on_key(key('c'));
        assert_eq!(app.regions.len(), 1);
        app.last_base = (100, 30); // a geometry the guard accepts
        app.on_key(ctrl('w'));
        app.on_key(key('v'));
        assert_eq!(app.regions.len(), 2);
        assert!(app.mode_menu && app.fresh_slot == Some(1));
        assert_eq!(app.focus, Focus::Region(1));
        // Back (Esc) on the fresh picker closes the fresh slot.
        app.on_key(KeyEvent::from(KeyCode::Esc));
        assert_eq!(app.regions.len(), 1);
        assert!(!app.mode_menu && app.fresh_slot.is_none());
        assert_eq!(app.focus, Focus::Region(0));
    }

    #[test]
    fn ctrl_w_s_creates_below_and_a_pick_lands_the_content() {
        let mut app = App::new();
        app.last_base = (100, 30); // a geometry the guard accepts
        app.on_key(ctrl('w'));
        app.on_key(key('s'));
        assert_eq!(app.regions.len(), 3);
        assert_eq!(app.lattice.active, [true, true, true, false]);
        assert_eq!(app.focus, Focus::Region(2)); // slot 3's index
        assert!(app.mode_menu && app.fresh_slot == Some(2));
        // Picking content 4 (mailboxes) lands it and ends fresh.
        app.on_key(key('4'));
        assert!(!app.mode_menu && app.fresh_slot.is_none());
        assert_eq!(app.regions[2].content, ContentKind::Mailboxes);
    }

    #[test]
    fn ctrl_w_c_closes_without_renumbering_and_the_last_window_stays() {
        let mut app = App::new();
        // {1,3}: closing slot 3 leaves slot 1's number untouched.
        app.lattice.active = [true, false, true, false];
        app.regions = vec![
            Viewport::new(ContentKind::Threads),
            Viewport::new(ContentKind::Transcript),
        ];
        app.focus = Focus::Region(1);
        app.on_key(ctrl('w'));
        app.on_key(key('c'));
        assert_eq!(app.lattice.active, [true, false, false, false]);
        assert_eq!(app.regions.len(), 1);
        assert_eq!(app.focus, Focus::Region(0));
        // The last window refuses to close (quit stays the q).
        app.on_key(ctrl('w'));
        app.on_key(key('c'));
        assert_eq!(app.flash.as_deref(), Some("the last window stays"));
        assert_eq!(app.regions.len(), 1);
    }

    #[test]
    fn the_draft_survives_a_host_close() {
        let mut app = App::new();
        app.draft.insert_str("draft text");
        app.show_compose = true;
        app.compose_host = 1;
        app.focus = Focus::Region(1);
        app.on_key(ctrl('w'));
        app.on_key(key('c'));
        // The composer hides to standby; the draft is construction
        // state and survives; i re-summons from any transcript.
        assert!(!app.show_compose);
        assert_eq!(app.draft.text(), "draft text");
        assert_eq!(app.focus, Focus::Region(0));
    }

    #[test]
    fn the_focus_arms_move_along_the_row_and_refuse_at_the_edge() {
        let mut app = App::new();
        // h from the leftmost slot: the edge refuses.
        app.on_key(ctrl('w'));
        app.on_key(key('h'));
        assert_eq!(app.flash.as_deref(), Some("no window to the left"));
        // l moves along the row.
        app.on_key(ctrl('w'));
        app.on_key(key('l'));
        assert_eq!(app.focus, Focus::Region(1));
        // After s creates slot 3 (its picker up: pick a content to
        // leave the modal), j/k walk the column.
        app.last_base = (100, 30);
        app.on_key(ctrl('w'));
        app.on_key(key('s'));
        assert_eq!(app.focus, Focus::Region(2));
        app.on_key(key('2')); // the pick ends the fresh picker
        assert!(!app.mode_menu);
        // The focus sits on slot 4 (s ran from slot 2: BELOW is 4).
        // k walks UP the column to slot 2; j back down to slot 4.
        app.on_key(ctrl('w'));
        app.on_key(key('k'));
        assert_eq!(app.focus, Focus::Region(1));
        app.on_key(ctrl('w'));
        app.on_key(key('j'));
        assert_eq!(app.focus, Focus::Region(2));
    }

    #[test]
    fn zoom_toggles_and_any_focus_or_geometry_op_unzooms() {
        let mut app = App::new();
        app.on_key(ctrl('w'));
        app.on_key(key('z'));
        assert!(app.zoom);
        // A focus move ends the lens first, then the op runs.
        app.on_key(KeyEvent::from(KeyCode::Tab));
        assert!(!app.zoom);
        assert_eq!(app.focus, Focus::Region(1));
        // z again zooms; a geometry chord unzooms and resizes.
        app.on_key(ctrl('w'));
        app.on_key(key('z'));
        assert!(app.zoom);
        app.on_key(ctrl('w'));
        app.on_key(key('>'));
        assert!(!app.zoom);
        // The focus sits on the RIGHT slot after the Tab: widening
        // it shrinks the left share.
        assert!((app.lattice.column - 0.25).abs() < 1e-9);
    }

    #[test]
    fn resize_moves_the_focused_sides_and_inert_splits_flash() {
        let mut app = App::new();
        // {1,2}: the bottom row is collapsed — the row chords are
        // inert; the column chords work.
        app.on_key(ctrl('w'));
        app.on_key(key('+'));
        assert_eq!(app.flash.as_deref(), Some("no row split"));
        app.on_key(ctrl('w'));
        app.on_key(key('>'));
        assert!((app.lattice.column - 0.35).abs() < 1e-9);
        // Widening from the RIGHT slot shrinks the left share.
        app.focus = Focus::Region(1);
        app.on_key(ctrl('w'));
        app.on_key(key('>'));
        assert!((app.lattice.column - 0.30).abs() < 1e-9);
        // = equalizes both ratios (the stored row survives a
        // collapse and returns with the slot).
        app.on_key(ctrl('w'));
        app.on_key(key('='));
        assert_eq!(app.lattice.column, 0.5);
        assert_eq!(app.lattice.row, 0.5);
        // {1,3}: the right column is collapsed — the column chords
        // are inert there; the row chords work.
        app.lattice.active = [true, false, true, false];
        app.regions = vec![
            Viewport::new(ContentKind::Threads),
            Viewport::new(ContentKind::Transcript),
        ];
        app.on_key(ctrl('w'));
        app.on_key(key('<'));
        assert_eq!(app.flash.as_deref(), Some("no column split"));
        app.on_key(ctrl('w'));
        app.on_key(key('-'));
        // The focus sits on slot 3 (the bottom row): shrinking it
        // grows the top share.
        assert!((app.lattice.row - 0.55).abs() < 1e-9);
    }

    #[test]
    fn the_too_small_target_is_a_silent_noop() {
        let mut app = App::new();
        // A 30x20 base: slot 3's quadrant would be 9 wide — below
        // the floor; the chord does NOTHING (no flash, no state).
        app.last_base = (30, 20);
        app.on_key(ctrl('w'));
        app.on_key(key('s'));
        assert_eq!(app.lattice.active, [true, true, false, false]);
        assert!(app.flash.is_none());
        // A fitting base opens the slot normally.
        app.last_base = (100, 30);
        app.on_key(ctrl('w'));
        app.on_key(key('s'));
        assert_eq!(app.lattice.active, [true, true, true, false]);
        assert_eq!(app.regions.len(), 3);
    }

    #[test]
    fn the_temp_refuses_slot_ops_but_takes_the_mode() {
        let mut app = App::new();
        app.temp = Some(Viewport::new(ContentKind::Threads));
        app.temp_fresh = false;
        app.focus = Focus::Temp;
        // The slot-class chords flash (the temp has no slot).
        app.on_key(ctrl('w'));
        app.on_key(key('v'));
        assert_eq!(app.flash.as_deref(), Some("focus a slot first"));
        // The viewport-class chord reaches the temp: the mode opens
        // over it.
        app.on_key(ctrl('w'));
        app.on_key(key('m'));
        assert!(app.mode_menu);
    }

    use super::*;

    #[test]
    fn switching_the_host_region_away_collapses_the_composer() {
        let mut app = App::new();
        // Region 2 (index 1) holds the transcript by default.
        app.summon_compose(1);
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
    fn threads_enter_shows_the_selected_thread_in_transcript_windows() {
        let mut app = App::new();
        app.set_region_content(0, ContentKind::Threads);
        app.set_region_content(1, ContentKind::Transcript);
        app.threads = vec![MsgRef {
            ver_major: 1,
            ver_minor: 0,
            algo: "ed25519".into(),
            digest: vec![1, 2, 3],
        }];
        app.regions[0].threads_sel.select(Some(0));
        app.on_key(ratatui::crossterm::event::KeyEvent::from(
            ratatui::crossterm::event::KeyCode::Enter,
        ));
        // The threads window never flips its content (the old
        // hijack is retired); the transcript window adopts.
        assert_eq!(app.regions[0].content, ContentKind::Threads);
        assert_eq!(app.focus, Focus::Region(0));
        assert!(app.open_thread.is_some());
        assert!(
            app.outbox
                .iter()
                .any(|f| matches!(f.method, Some(Method::MsgThreadLogGet(_))))
        );
    }

    #[test]
    fn threads_enter_without_transcript_window_does_nothing() {
        let mut app = App::new();
        // Region 2 (index 1) defaults to the transcript — park it on
        // threads too, so NO window shows a transcript.
        app.set_region_content(1, ContentKind::Threads);
        app.threads = vec![MsgRef {
            ver_major: 1,
            ver_minor: 0,
            algo: "ed25519".into(),
            digest: vec![1, 2, 3],
        }];
        app.regions[0].threads_sel.select(Some(0));
        app.on_key(ratatui::crossterm::event::KeyEvent::from(
            ratatui::crossterm::event::KeyCode::Enter,
        ));
        assert!(app.open_thread.is_none());
        assert!(
            app.outbox
                .iter()
                .all(|f| !matches!(f.method, Some(Method::MsgThreadLogGet(_))))
        );
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
    fn the_mode_moves_and_opens() {
        let mut app = App::new();
        app.mode_menu = true;
        app.mode_sel = app.regions[0].content.index();
        // Route keys through the handler: j moves the cursor,
        // Enter opens the highlighted kind.
        app.on_key(ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Char('j'),
            ratatui::crossterm::event::KeyModifiers::NONE,
        ));
        assert_eq!(app.mode_sel, 1);
        app.on_key(ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Enter,
            ratatui::crossterm::event::KeyModifiers::NONE,
        ));
        assert!(!app.mode_menu);
        assert_eq!(app.regions[0].content, ContentKind::Transcript);

        // Re-open: a number key still jumps direct.
        app.mode_menu = true;
        app.on_key(ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Char('3'),
            ratatui::crossterm::event::KeyModifiers::NONE,
        ));
        assert!(!app.mode_menu);
        assert_eq!(app.regions[0].content, ContentKind::Authors);
    }

    #[test]
    fn the_temp_window_opens_picks_and_pops() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        let mut app = App::new();
        app.focus = Focus::Region(1);
        app.regions[1].content = ContentKind::Mailboxes;
        // x from region 1: the temp window opens straight into the
        // picker, the origin remembered.
        app.on_key(KeyEvent::from(KeyCode::Char('x')));
        assert!(app.temp.is_some());
        assert!(app.mode_menu);
        assert!(app.temp_fresh);
        assert!(matches!(app.focus, Focus::Temp));
        assert_eq!(app.temp_origin, 1);
        // Esc in the fresh picker: the window closes, no content.
        app.on_key(KeyEvent::from(KeyCode::Esc));
        assert!(app.temp.is_none());
        assert!(!app.mode_menu);
        assert!(matches!(app.focus, Focus::Region(1)));
        // Reopen, pick 2 (transcript): content set, no longer fresh.
        app.on_key(KeyEvent::from(KeyCode::Char('x')));
        app.on_key(KeyEvent::from(KeyCode::Char('2')));
        assert!(!app.mode_menu);
        assert!(!app.temp_fresh);
        assert_eq!(
            app.viewport(crate::app::TEMP_VIEW).content,
            ContentKind::Transcript
        );
        // Esc pops back to the origin; the regions are untouched.
        app.on_key(KeyEvent::from(KeyCode::Esc));
        assert!(app.temp.is_none());
        assert!(matches!(app.focus, Focus::Region(1)));
        assert_eq!(app.regions[1].content, ContentKind::Mailboxes);
    }

    #[test]
    fn n_arms_the_new_thread_row_without_hijacking() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        let mut app = App::new();
        app.set_region_content(0, ContentKind::Threads);
        app.set_region_content(1, ContentKind::Transcript);
        // Two real threads: j off the synthetic row must land on a
        // NORMAL thread (the list holds row + both).
        app.threads = vec![
            MsgRef {
                ver_major: 1,
                ver_minor: 0,
                algo: "ed25519".into(),
                digest: vec![1, 1, 1],
            },
            MsgRef {
                ver_major: 1,
                ver_minor: 0,
                algo: "ed25519".into(),
                digest: vec![2, 2, 2],
            },
        ];
        app.open_thread = Some(MsgRef {
            ver_major: 1,
            ver_minor: 0,
            algo: "ed25519".into(),
            digest: vec![5, 5, 5],
        });
        app.on_key(KeyEvent::from(KeyCode::Char('n')));
        assert!(app.new_thread_row && app.new_thread_intent);
        // No window hijack, no focus move.
        assert_eq!(app.regions[0].content, ContentKind::Threads);
        assert_eq!(app.focus, Focus::Region(0));
        // The transcript windows track the fresh thread; the
        // composer stands by, unfocused.
        assert!(app.open_thread.is_none() && app.entries.is_empty());
        assert!(app.show_compose && app.compose_host == 1);
        // j off the row tears the state down and restores.
        app.on_key(KeyEvent::from(KeyCode::Char('j')));
        assert!(!app.new_thread_row && !app.new_thread_intent);
        assert!(!app.show_compose);
        assert!(
            app.open_thread
                .as_ref()
                .is_some_and(|t| t.digest == vec![5, 5, 5])
        );
        assert!(app.outbox.iter().any(|f| matches!(
            &f.method,
            Some(Method::MsgThreadLogGet(g)) if g.thread.as_ref().is_some_and(|t| t.digest == vec![5, 5, 5])
        )));
    }

    #[test]
    fn a_foreign_arrival_never_hijacks_the_armed_new_thread() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        let mut app = App::new();
        app.set_region_content(1, ContentKind::Transcript);
        app.on_key(KeyEvent::from(KeyCode::Char('n')));
        assert!(app.new_thread_row);
        // A foreign arrival (not our receipt) must not adopt.
        app.ingest_package(MsgPackage {
            entry: Some(Entry {
                meta: Some(zconned_stdio_types::RowMeta {
                    digest: vec![7u8; 32],
                    from: vec![9u8; 32],
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        });
        assert!(app.new_thread_row && app.open_thread.is_none() && app.entries.is_empty());
        // OUR commit receipt lands the thread (the row's promise).
        app.awaiting_commit = true;
        app.ingest_package(MsgPackage {
            entry: Some(Entry {
                prev: vec![MsgRef {
                    ver_major: 1,
                    ver_minor: 0,
                    algo: "ed25519".into(),
                    digest: vec![3u8; 32],
                }],
                meta: Some(zconned_stdio_types::RowMeta {
                    digest: vec![8u8; 32],
                    from: vec![9u8; 32],
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        });
        assert!(!app.new_thread_row);
        assert!(
            app.open_thread
                .as_ref()
                .is_some_and(|t| t.digest == vec![3u8; 32])
        );
        assert_eq!(app.entries.len(), 1);
    }

    #[test]
    fn n_in_the_transcript_tracks_and_types_at_once() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        let mut app = App::new();
        // The transcript content: n enters the same new-thread
        // state, with the composer going straight to typing.
        app.set_region_content(0, ContentKind::Transcript);
        app.focus = Focus::Region(0);
        app.open_thread = Some(MsgRef {
            ver_major: 1,
            ver_minor: 0,
            algo: "ed25519".into(),
            digest: vec![5, 5, 5],
        });
        app.on_key(KeyEvent::from(KeyCode::Char('n')));
        assert!(app.new_thread_row && app.new_thread_intent);
        assert!(app.show_compose);
        assert!(matches!(app.focus, Focus::Compose));
        // The transcript tracks the fresh thread, not the old one.
        assert!(app.open_thread.is_none() && app.entries.is_empty());
    }

    #[test]
    fn t_toggles_the_timestamps_and_capital_t_the_fingerprints() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        let mut app = App::new();
        app.set_region_content(0, ContentKind::Transcript);
        app.focus = Focus::Region(0);
        assert!(app.show_ts && !app.show_fp);
        app.on_key(KeyEvent::from(KeyCode::Char('t')));
        assert!(!app.show_ts);
        app.on_key(KeyEvent::from(KeyCode::Char('T')));
        assert!(app.show_fp);
        app.on_key(KeyEvent::from(KeyCode::Char('t')));
        app.on_key(KeyEvent::from(KeyCode::Char('T')));
        assert!(app.show_ts && !app.show_fp);
    }

    #[test]
    fn wrap_width_counts_display_width_not_chars() {
        // CJK chars are double width: 星团 fills 4 columns, so "ab"
        // must wrap even though the char count would fit.
        assert_eq!(
            wrap_width("\u{661f}\u{56e2}ab", 4),
            vec!["\u{661f}\u{56e2}", "ab"]
        );
        assert_eq!(wrap_width("abcdefgh", 3), vec!["abc", "def", "gh"]);
        assert_eq!(wrap_width("", 10), vec![""]);
    }

    #[test]
    fn d_deletes_the_selected_thread_and_the_receipt_drops_its_view() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        let mut app = App::new();
        app.set_region_content(0, ContentKind::Threads);
        app.focus = Focus::Region(0);
        let t1 = MsgRef {
            ver_major: 1,
            ver_minor: 0,
            algo: "ed25519".into(),
            digest: vec![1, 1, 1],
        };
        let t2 = MsgRef {
            ver_major: 1,
            ver_minor: 0,
            algo: "ed25519".into(),
            digest: vec![2, 2, 2],
        };
        app.threads = vec![t1.clone(), t2.clone()];
        app.regions[0].threads_sel.select(Some(0));
        app.open_thread = Some(t1.clone());
        app.entries = vec![Entry::default()];
        app.on_key(KeyEvent::from(KeyCode::Char('d')));
        assert!(app.outbox.iter().any(|f| matches!(
            &f.method,
            Some(Method::MsgThreadDelete(del)) if del.thread.as_ref() == Some(&t1)
        )));
        // The receipt: the surviving list only — the open deleted
        // thread's transcript view drops to empty.
        app.on_frame(zconned_stdio_types::OutgoingFrame {
            frame: Some(Out::MsgThreadsPush(zconned_stdio_types::MsgThreadsPush {
                threads: vec![zconned_stdio_types::ThreadEntry {
                    thread: Some(t2.clone()),
                }],
            })),
        });
        assert_eq!(app.threads, vec![t2.clone()]);
        assert!(app.open_thread.is_none() && app.entries.is_empty());
        // Resurrection (a re-grown thread re-appears): the list
        // re-renders; nothing else breaks.
        app.on_frame(zconned_stdio_types::OutgoingFrame {
            frame: Some(Out::MsgThreadsPush(zconned_stdio_types::MsgThreadsPush {
                threads: vec![
                    zconned_stdio_types::ThreadEntry {
                        thread: Some(t1.clone()),
                    },
                    zconned_stdio_types::ThreadEntry { thread: Some(t2) },
                ],
            })),
        });
        assert_eq!(app.threads.len(), 2);
    }

    #[test]
    fn d_on_the_synthetic_new_thread_row_is_a_noop() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        let mut app = App::new();
        app.set_region_content(0, ContentKind::Threads);
        app.focus = Focus::Region(0);
        app.on_key(KeyEvent::from(KeyCode::Char('n')));
        assert!(app.new_thread_row);
        app.outbox.clear();
        app.on_key(KeyEvent::from(KeyCode::Char('d')));
        assert!(
            app.outbox
                .iter()
                .all(|f| !matches!(f.method, Some(Method::MsgThreadDelete(_))))
        );
    }

    #[test]
    fn f_in_the_transcript_shows_the_selected_row_author() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        let mut app = App::new();
        app.set_region_content(0, ContentKind::Transcript);
        app.focus = Focus::Region(0);
        app.entries = vec![Entry {
            meta: Some(zconned_stdio_types::RowMeta {
                from: vec![7u8; zconned_crypto::DIGEST_LEN],
                ..Default::default()
            }),
            ..Default::default()
        }];
        app.regions[0].transcript_sel.select(Some(0));
        app.on_key(KeyEvent::from(KeyCode::Char('f')));
        assert!(
            app.fp_shown
                .as_deref()
                .is_some_and(|s| s.starts_with("sha3-256:"))
        );
        app.on_key(KeyEvent::from(KeyCode::Esc));
        assert!(app.fp_shown.is_none());
        // A row without an author: the flash, no popup.
        app.entries[0].meta = None;
        app.on_key(KeyEvent::from(KeyCode::Char('f')));
        assert!(app.fp_shown.is_none() && app.flash.is_some());
    }

    #[test]
    fn q_in_the_temp_window_closes_it_not_the_app() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        let mut app = App::new();
        app.on_key(KeyEvent::from(KeyCode::Char('x')));
        assert!(app.temp.is_some());
        // The picker is already the fresh temp's first face: pick
        // the mailboxes directly (m here would BACK = close).
        app.on_key(KeyEvent::from(KeyCode::Char('4')));
        assert_eq!(app.temp.as_ref().unwrap().content, ContentKind::Mailboxes);
        // q = back: the window closes, the app lives.
        app.on_key(KeyEvent::from(KeyCode::Char('q')));
        assert!(app.temp.is_none() && !app.quit);
    }

    #[test]
    fn the_numbers_address_lattice_slots_and_empty_slots_refuse() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        let mut app = App::new();
        // The default lattice {1,2}: the numbers map to the
        // reading-order regions, and the titles speak the slot
        // number.
        assert_eq!(app.slot_number(0), 1);
        assert_eq!(app.slot_number(1), 2);
        assert_eq!(app.slot_index(1), Some(0));
        assert_eq!(app.slot_index(2), Some(1));
        // 1/2 jump to the windows; 3/4 hit EMPTY slots — they
        // refuse with a flash and never move the focus.
        app.on_key(KeyEvent::from(KeyCode::Char('2')));
        assert_eq!(app.focus, Focus::Region(1));
        app.on_key(KeyEvent::from(KeyCode::Char('3')));
        assert_eq!(app.focus, Focus::Region(1));
        assert_eq!(app.flash.as_deref(), Some("window 3 is empty"));
        app.on_key(KeyEvent::from(KeyCode::Char('4')));
        assert_eq!(app.focus, Focus::Region(1));
        assert_eq!(app.flash.as_deref(), Some("window 4 is empty"));
        app.on_key(KeyEvent::from(KeyCode::Char('1')));
        assert_eq!(app.focus, Focus::Region(0));
    }

    #[test]
    fn the_slot_mapping_survives_a_hole_between_live_slots() {
        let mut app = App::new();
        // {1,4}: the regions vector holds the ACTIVE windows in
        // reading order — region index 1 is WINDOW 4, not 2.
        app.lattice.active = [true, false, false, true];
        app.regions = vec![
            Viewport::new(ContentKind::Threads),
            Viewport::new(ContentKind::Mailboxes),
        ];
        assert_eq!(app.slot_number(0), 1);
        assert_eq!(app.slot_number(1), 4);
        assert_eq!(app.slot_index(1), Some(0));
        assert_eq!(app.slot_index(4), Some(1));
        assert_eq!(app.slot_index(2), None);
        assert_eq!(app.slot_index(3), None);
    }

    #[test]
    fn m_outside_the_picker_switches_to_the_modes_picker() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        let mut app = App::new();
        // Inside the fingerprint popup: m closes it AND switches to
        // the modes picker (m never means "close").
        app.focus = Focus::Region(1);
        app.regions[1].content = ContentKind::Authors;
        app.fp_shown = Some("sha3-256:00".into());
        app.on_key(KeyEvent::from(KeyCode::Char('m')));
        assert!(app.fp_shown.is_none());
        assert!(app.mode_menu);
        app.on_key(KeyEvent::from(KeyCode::Esc));

        // Inside the modes picker itself: m means back — and q
        // shares it (the row sits between m and Esc; it is NOT
        // quit there).
        app.mode_menu = true;
        app.on_key(KeyEvent::from(KeyCode::Char('m')));
        assert!(!app.mode_menu);
        app.mode_menu = true;
        app.on_key(KeyEvent::from(KeyCode::Char('q')));
        assert!(!app.mode_menu && !app.quit);
    }

    #[test]
    fn f_shows_the_full_fingerprint_and_esc_closes() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        let mut app = App::new();
        app.focus = Focus::Region(1);
        app.regions[1].content = ContentKind::Authors;
        app.authors.insert(
            vec![9u8; zconned_crypto::DIGEST_LEN],
            crate::app::AuthorEntry {
                verified: true,
                remark: None,
            },
        );
        app.regions[1].authors_sel.select(Some(0));
        app.on_key(KeyEvent::from(KeyCode::Char('f')));
        let shown = app.fp_shown.clone().expect("shown");
        assert!(shown.starts_with("sha3-256:"));
        assert_eq!(
            shown.len(),
            "sha3-256:".len() + 2 * zconned_crypto::DIGEST_LEN
        );
        app.on_key(KeyEvent::from(KeyCode::Esc));
        assert!(app.fp_shown.is_none());
    }

    #[test]
    fn fp_short_is_the_canon_short_form_of_the_same_digest() {
        // F1: the short form is the full form's GROUPED HEAD — the
        // local helper (over an already-computed digest) must equal
        // the canon `Fingerprint::short` (over the key) byte for
        // byte, or the two UIs cannot be cross-checked.
        let sign = [7u8; zconned_crypto::SIGN_PUB_LEN];
        let fp = zconned_crypto::Fingerprint::of(&sign);
        assert_eq!(fp_short(fp.digest()), fp.short());
        // The shape itself: 8 bytes, 4 grouped blocks.
        let text = fp.short();
        assert_eq!(text.len(), 4 * 4 + 3);
        assert_eq!(text.matches(' ').count(), 3);
        // And the FULL form helper matches the canon Display.
        assert_eq!(fp_full(fp.digest()), fp.to_string());
    }

    #[test]
    fn p_is_retired_the_quick_profile_is_temp_plus_pick() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        let mut app = App::new();
        app.on_key(KeyEvent::from(KeyCode::Char('p')));
        // p does nothing: no temp window, no content swap.
        assert!(app.temp.is_none());
        assert!(!app.mode_menu);
        assert_eq!(app.regions[0].content, ContentKind::Threads);
        assert_eq!(app.regions[1].content, ContentKind::Transcript);
    }
}
