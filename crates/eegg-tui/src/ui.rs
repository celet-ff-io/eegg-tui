//! The ratatui surface: status line, three panes, key hints. Simple
//! and modern — plain blocks, one accent color for focus.

use std::io::Stdout;

use miette::IntoDiagnostic as _;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    crossterm::{
        execute,
        terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
    },
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Style, Stylize as _},
    text::{Line, Span},
    widgets::{Block, Clear, List, ListItem, Paragraph, Widget as _},
};

use crate::app::{App, CardFlow, ContentKind, Focus, anchor_of, preview_bytes, short_hex};

pub type Term = Terminal<CrosstermBackend<Stdout>>;

pub fn init_terminal() -> miette::Result<Term> {
    enable_raw_mode().into_diagnostic()?;
    execute!(std::io::stdout(), EnterAlternateScreen).into_diagnostic()?;
    Terminal::new(CrosstermBackend::new(std::io::stdout())).into_diagnostic()
}

/// Restore the terminal to cooked mode (teardown step 1; also the
/// panic hook's first act).
pub fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
}

pub fn render(app: &mut App, f: &mut Frame) {
    // The keybar's height follows its hint lines (1 normally, 2 when
    // the hints wrapped) — the wrap decision needs the width and the
    // extra-info reservation BEFORE the layout, so measure first.
    let version = format!("v{}", env!("CARGO_PKG_VERSION"));
    let hint_lines = keybar_lines(
        keybar_hints(app),
        f.area().width,
        version.chars().count() as u16 + 2,
    );
    let [status, main, keybar] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(hint_lines.len() as u16),
    ])
    .areas(f.area());
    // The two regions: region 1 is the narrow side, region 2 the
    // wide main (the later layout engine makes the split movable).
    let [r0, r1] =
        Layout::horizontal([Constraint::Percentage(30), Constraint::Percentage(70)]).areas(main);

    render_status(app, f, status);
    render_region(app, f, 0, r0);
    render_region(app, f, 1, r1);
    render_keybar(f, keybar, &hint_lines, &version);
    if app.mode_menu {
        render_mode_menu(app, f);
        return;
    }
    if app.show_log {
        render_log_popup(app, f);
    }
    if app.show_help {
        render_help_popup(app, f);
    }
    if app.file_box != crate::app::FileBox::None {
        render_file_box(app, f);
    }
}

/// One region: its content, ONE frame. The composer (when this
/// region hosts an open one) renders INSIDE the transcript frame —
/// a rule, the draft rows, the staged line; no second frame.
fn render_region(app: &mut App, f: &mut Frame, i: usize, area: ratatui::prelude::Rect) {
    match app.regions[i].content {
        ContentKind::Threads => render_threads(app, f, i, area),
        ContentKind::Transcript => render_transcript(app, f, i, area),
        ContentKind::Authors => render_authors(app, f, i, area),
        ContentKind::Mailboxes => render_mailboxes(app, f, i, area),
    }
}

/// A region border's color: yellow when the region focus is here;
/// GREEN when the composer inside it holds the focus (typing — the
/// frame encloses the focused compose, and green says so without
/// stealing yellow's region-focus meaning); dark gray otherwise.
fn region_border(app: &App, i: usize) -> Color {
    match app.focus {
        Focus::Region(f) if f == i => Color::Yellow,
        Focus::Compose if app.compose_host == i => Color::Green,
        _ => Color::DarkGray,
    }
}

/// A section rule inside a frame: NEVER yellow (yellow means the
/// region focus; a colored rule would read as a second frame).
fn rule(width: usize, label: &str) -> Span<'static> {
    Span::from(format!("{:-^width$}", label)).dark_gray()
}

/// The mode: entering it turns the WHOLE window into the
/// selection surface (the principal — not a small popup over the
/// focused region). j/k move the highlight, Enter opens it,
/// 1-4 jump direct; the current content is marked — selecting
/// it just closes.
fn render_mode_menu(app: &App, f: &mut Frame) {
    let focused = match app.focus {
        Focus::Region(i) => i.min(app.regions.len() - 1),
        Focus::Compose => app.compose_host.min(app.regions.len() - 1),
    };
    let current = app.regions[focused].content;
    let mut lines: Vec<Line> = vec![Line::from("")];
    for n in 0..4 {
        let kind = ContentKind::nth(n);
        let sel = app.mode_sel == n;
        let mark = if sel { "\u{25b6} " } else { "  " };
        let cur = if kind == current {
            "  \u{25cf} current"
        } else {
            ""
        };
        let num = Span::from(format!(" {mark}{}", n + 1));
        let num = if sel {
            num.yellow().bold()
        } else {
            num.dark_gray()
        };
        let title = Span::from(format!(" {}{cur}", kind.title()));
        let title = if sel { title.bold() } else { title.cyan() };
        lines.push(Line::from(vec![num, title]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(
        Span::from("  j/k select · Enter open · 1-4 direct · Esc/m close").dark_gray(),
    ));
    let area = f.area();
    f.render_widget(Clear, area);
    Paragraph::new(lines)
        .block(
            Block::bordered()
                .title(format!(" mode \u{2014} region {}'s content ", focused + 1))
                .border_style(Color::Magenta),
        )
        .render(area, f.buffer_mut());
}

/// The MAIN-window file-path box: what Enter will write/read is set
/// by the FileBox kind. ~/ expands.
fn render_file_box(app: &App, f: &mut Frame) {
    let (title, prefix) = match app.file_box {
        crate::app::FileBox::None => return,
        crate::app::FileBox::SaveCompose => (
            " save compose to file: Enter save \u{00b7} Esc cancel ",
            "save to> ",
        ),
        crate::app::FileBox::LoadCompose => (
            " load compose from file: Enter load \u{00b7} Esc cancel ",
            "load from> ",
        ),
        crate::app::FileBox::SaveTranscript => (
            " save row text to file: Enter save \u{00b7} Esc cancel ",
            "save to> ",
        ),
    };
    let area = centered(f.area(), 64, 3);
    f.render_widget(Clear, area);
    let text_width = area.width.saturating_sub(2 + prefix.len() as u16) as usize;
    let scroll = app.file_input.visual_scroll(text_width);
    let visible: String = app
        .file_input
        .value()
        .chars()
        .skip(scroll)
        .take(text_width)
        .collect();
    let caret = app.file_input.visual_cursor() - scroll;
    Paragraph::new(Line::from(vec![
        Span::from(prefix).yellow(),
        Span::from(visible),
    ]))
    .block(Block::bordered().title(title).border_style(Color::Magenta))
    .render(area, f.buffer_mut());
    f.set_cursor_position((area.x + 1 + prefix.len() as u16 + caret as u16, area.y + 1));
}

/// The lazygit-style help overlay: the keymap as SECTIONED CARDS —
/// one operation per line, grouped by panel — above everything (it
/// may sit over the peers/log popups too). Closed with Esc / ? / q;
/// j/k scroll it when the terminal is short.
fn render_help_popup(app: &mut App, f: &mut Frame) {
    // (true = an operation row; false = a section header.)
    let rows: [(bool, &str, &str); 55] = [
        (false, "global", ""),
        (
            true,
            "<Ctrl+h/j/k/l>",
            "navigate from anywhere (h/l the sides, j/k the ring)",
        ),
        (
            true,
            "h/l",
            "the same left/right moves (plain keys — the lists and the composer keep what they use)",
        ),
        (true, "Tab", "cycle focus between the regions"),
        (true, "m", "open the mode: switch this region's content"),
        (true, "p", "show the mailboxes here"),
        (true, "L", "core log popup (the core's stderr ring)"),
        (true, "q", "quit"),
        (false, "mode", ""),
        (
            true,
            "m",
            "the WHOLE window becomes the picker (j/k select, Enter open)",
        ),
        (
            true,
            "1..4",
            "jump direct to a content (the current closes)",
        ),
        (true, "Esc/m", "close, no change"),
        (false, "threads", ""),
        (true, "t", "refresh the thread list"),
        (true, "n", "new thread (fresh chain on the next send)"),
        (true, "Enter", "open the selected thread"),
        (true, "j/k \u{2191}\u{2193}", "move the selection"),
        (true, "g/G", "jump to the top / the bottom"),
        (false, "transcript", ""),
        (
            true,
            "",
            "from <anchor> = the row's author; * = roster-confirmed",
        ),
        (true, "j/k \u{2191}\u{2193}", "scroll"),
        (true, "PgUp/PgDn", "page by a screen"),
        (true, "n", "new thread (fresh chain on the next send)"),
        (true, "r", "re-stage the draft"),
        (true, "y", "copy the selected row's text to the clipboard"),
        (true, "w", "save the selected row's text to a file"),
        (false, "compose", ""),
        (
            true,
            "i",
            "summon the composer (transcript only; typing mode)",
        ),
        (true, "<Ctrl+Enter>", "send the compose text"),
        (true, "<Alt+Enter>", "send the compose text (either works)"),
        (true, "<Ctrl+W>", "drop the word before the caret"),
        (
            true,
            "Home/End",
            "line start / end (<Ctrl+Home>/<Ctrl+End> = the whole draft)",
        ),
        (true, "<Ctrl+U>", "clear the compose text"),
        (true, "<Ctrl+C>", "copy the compose text to the clipboard"),
        (true, "<Ctrl+V>", "paste the clipboard at the caret"),
        (true, "<Ctrl+R>", "load the compose text from a file"),
        (true, "<Ctrl+S>", "save the compose text to a file"),
        (
            true,
            "Esc",
            "typing \u{2192} standby (the normal UI; Esc again = hide the composer)",
        ),
        (false, "mailboxes", ""),
        (
            true,
            "",
            "+ record = the card's third key (missing = a legacy card)",
        ),
        (
            true,
            "c",
            "copy the selected card (a mailbox or a rostered author)",
        ),
        (true, "s", "save the selected card to a file"),
        (true, "a", "add the counterpart's card"),
        (true, "<Ctrl+V>", "paste the card from the clipboard"),
        (true, "<Ctrl+R>", "load the card from a file"),
        (true, "j/k \u{2191}\u{2193}", "move the selection"),
        (true, "Esc", "back a level / cancel the flow"),
        (false, "authors", ""),
        (
            true,
            "",
            "* = verified authorship (the record signature rostered)",
        ),
        (
            true,
            "",
            "\u{00b7} = observed on an envelope only (unverified)",
        ),
        (true, "r", "set the selected identity's remark"),
        (true, "R", "clear the selected identity's remark"),
        (false, "log", ""),
        (
            true,
            "j/k \u{2191}\u{2193}",
            "scroll the core's stderr ring",
        ),
        (true, "Esc / L / q", "close the popup"),
    ];
    let lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .flat_map(|(i, (row, key, desc))| {
            // Every section break carries a blank line before it
            // (the first header sits at the top — no blank there).
            let blank = (i > 0 && !row).then(|| Line::from(Span::from("")));
            let line = if *row && key.is_empty() {
                // A legend row: a notation explanation, NOT a
                // hotkey — flush left, dark gray.
                Line::from(Span::from(format!("  {desc}")).dark_gray())
            } else if *row {
                // lazygit style: the key column right-aligned, the
                // description tight against it.
                let pad = 12usize.saturating_sub(key.chars().count()) + 1;
                let mut spans = vec![Span::from(" "), Span::from(" ".repeat(pad - 1))];
                spans.extend(key_spans(key));
                spans.push(Span::from(" "));
                spans.push(Span::from(desc.to_owned()));
                Line::from(spans)
            } else {
                // The section break: a full-width rule with the
                // panel's name in the middle.
                Line::from(
                    Span::from(format!("{:-^70}", format!(" {} ", key.to_uppercase()))).dark_gray(),
                )
            };
            blank.into_iter().chain(std::iter::once(line))
        })
        .collect();
    // A short terminal: the card scrolls (j/k) instead of clipping.
    let max_h = f.area().height.saturating_sub(4).max(5);
    let height = (lines.len() as u16 + 2).min(max_h);
    let visible = height.saturating_sub(2) as usize;
    let max_off = lines.len().saturating_sub(visible);
    app.help_scroll = app.help_scroll.min(max_off as u16);
    let area = centered(f.area(), 72, height);
    f.render_widget(Clear, area);
    Paragraph::new(lines)
        .scroll((app.help_scroll, 0))
        .block(
            Block::bordered()
                .title(" help \u{00b7} j/k scroll \u{00b7} Esc/?/q close ")
                .border_style(Color::Magenta),
        )
        .render(area, f.buffer_mut());
}

/// The core's stderr ring — the diagnosis window. The core logs
/// nothing at the default level; a talky core: `--core "zconned -v"`.
fn render_log_popup(app: &App, f: &mut Frame) {
    let area = centered(f.area(), 80, 60);
    f.render_widget(Clear, area);
    let lines: Vec<Line> = app
        .core_stderr
        .iter()
        .map(|l| Line::from(Span::from(l.trim_end().to_owned())).dark_gray())
        .collect();
    // The scroll offset clamps at render time: a popup shorter than
    // the ring shows a window over it.
    let inner = area.height.saturating_sub(2) as usize;
    let offset = app
        .log_scroll
        .min(lines.len().saturating_sub(inner.min(lines.len())));
    Paragraph::new(lines)
        .scroll((offset as u16, 0))
        .block(
            Block::bordered()
                .title(format!(
                    " core stderr (last {}) — Esc/L closes, j/k scrolls ",
                    app.core_stderr.len()
                ))
                .border_style(Color::Yellow),
        )
        .wrap(ratatui::widgets::Wrap { trim: false })
        .render(area, f.buffer_mut());
}

/// The MAILBOXES region content: the core's own card on top (the
/// copy source), the send-target set in the middle, the card/file
/// flow box at the bottom when one is active.
/// One peers-list row: the addressing anchor (the signing key's
/// fingerprint — the same form row authors are shown by), the key
/// set (sign + DH + record — a LEGACY card lacks the third key,
/// its record signatures cannot verify until re-exchanged), the
/// endpoints; `tag` marks the self row.
fn peer_item(p: &zconned_stdio_types::Peer, tag: &str) -> ListItem<'static> {
    let mut line = vec![
        Span::from(tag.to_owned()).yellow(),
        Span::from(short_hex(&anchor_of(p))).cyan(),
        Span::from(format!(" {} + {}", p.sign_algo, p.dh_algo)).dark_gray(),
    ];
    match &p.record_sign_pub {
        Some(k) if !k.is_empty() => {
            line.push(Span::from(" + record").dark_gray());
        }
        _ => line.push(Span::from("  \u{00b7} no record key").red()),
    }
    if !p.endpoints.is_empty() {
        line.push(Span::from(format!("  {}", p.endpoints.join(" "))));
    }
    ListItem::from(Line::from(line))
}

fn render_mailboxes(app: &mut App, f: &mut Frame, i: usize, area: ratatui::prelude::Rect) {
    let flow = matches!(
        app.card_flow,
        CardFlow::InputCard | CardFlow::ReadCard | CardFlow::SaveCard
    );
    let block = Block::bordered()
        .title(format!(" {} mailboxes ", i + 1))
        .border_style(region_border(app, i));
    let inner = block.inner(area);
    block.render(area, f.buffer_mut());
    let fl = if flow { 1 } else { 0 };
    let [card_a, rule_a, list_a, frule_a, input_a, help_a] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(fl),
        Constraint::Length(fl),
        Constraint::Length(fl),
    ])
    .areas(inner);

    // The card text: TWO rows; a longer card is clipped with an
    // explicit ellipsis — the full text travels through c (clipboard)
    // / s (file) on the SELECTED row, never through the screen.
    let card_text = app.self_card.clone().unwrap_or_else(|| "(no card)".into());
    let capacity = card_a.width as usize * 2;
    let shown = if card_text.chars().count() > capacity {
        let kept: String = card_text.chars().take(capacity.saturating_sub(1)).collect();
        format!("{kept}\u{2026}")
    } else {
        card_text
    };
    Paragraph::new(Span::from(shown).cyan())
        .wrap(ratatui::widgets::Wrap { trim: false })
        .render(card_a, f.buffer_mut());

    // The rule names the section below it (the selection spans me +
    // the peer set: row 0 = me).
    Paragraph::new(rule(
        inner.width as usize,
        &format!(" peers ({}) ", app.peers.len()),
    ))
    .render(rule_a, f.buffer_mut());
    let mut items: Vec<ListItem> = Vec::with_capacity(app.peers.len() + 1);
    items.push(
        match app
            .self_card
            .as_deref()
            .and_then(crate::app::peer_from_card)
        {
            Some(me) => peer_item(&me, "(me) "),
            None => ListItem::from(Line::from(Span::from("(me) (no card)").dark_gray())),
        },
    );
    items.extend(app.peers.iter().map(|p| peer_item(p, "    ")));
    let list_w =
        List::new(items).highlight_style(Style::default().bg(Color::DarkGray).fg(Color::White));
    ratatui::widgets::StatefulWidget::render(
        &list_w,
        list_a,
        f.buffer_mut(),
        &mut app.regions[i].mailboxes_sel,
    );

    if flow {
        let (prefix, value, caret_col, help): (_, String, u16, &str) = match app.card_flow {
            // A card is one long base64 line: the visible window
            // follows the caret (horizontal scroll), otherwise the
            // tail is unseeable.
            CardFlow::InputCard => {
                let prefix = "paste card> ";
                let text_width = input_a.width.saturating_sub(prefix.len() as u16) as usize;
                let scroll = app.peer_input.visual_scroll(text_width);
                let visible: String = app
                    .peer_input
                    .value()
                    .chars()
                    .skip(scroll)
                    .take(text_width)
                    .collect();
                let caret = app.peer_input.visual_cursor() - scroll;
                (
                    prefix,
                    visible,
                    caret as u16,
                    "Enter submit · Ctrl-V paste · Ctrl-R load file · Esc cancel",
                )
            }
            CardFlow::ReadCard => {
                let prefix = "card file> ";
                let text_width = input_a.width.saturating_sub(prefix.len() as u16) as usize;
                let scroll = app.file_input.visual_scroll(text_width);
                let visible: String = app
                    .file_input
                    .value()
                    .chars()
                    .skip(scroll)
                    .take(text_width)
                    .collect();
                let caret = app.file_input.visual_cursor() - scroll;
                (prefix, visible, caret as u16, "Enter load · Esc back")
            }
            CardFlow::SaveCard => {
                let prefix = "save to> ";
                let text_width = input_a.width.saturating_sub(prefix.len() as u16) as usize;
                let scroll = app.file_input.visual_scroll(text_width);
                let visible: String = app
                    .file_input
                    .value()
                    .chars()
                    .skip(scroll)
                    .take(text_width)
                    .collect();
                let caret = app.file_input.visual_cursor() - scroll;
                (
                    prefix,
                    visible,
                    caret as u16,
                    "Enter save (~/ expands) · Esc back",
                )
            }
            CardFlow::None | CardFlow::InputRemark => unreachable!("checked above"),
        };
        Paragraph::new(rule(inner.width as usize, "")).render(frule_a, f.buffer_mut());
        Paragraph::new(Line::from(vec![
            Span::from(prefix).yellow(),
            Span::from(value),
        ]))
        .render(input_a, f.buffer_mut());
        f.set_cursor_position((input_a.x + prefix.len() as u16 + caret_col, input_a.y));
        Paragraph::new(Span::from(help).dark_gray()).render(help_a, f.buffer_mut());
    }
}

/// The AUTHORS region content: every sender identity the session
/// observed — ✍ = its record signature was rostered (objective
/// authorship, verified by the core); · = seen on an envelope only.
/// Each row shows the identity's local remark (the key fingerprint
/// only as the fallback), cross-referenced with the book.
fn render_authors(app: &mut App, f: &mut Frame, i: usize, area: ratatui::prelude::Rect) {
    let block = Block::bordered()
        .title(format!(" {} authors ", i + 1))
        .border_style(region_border(app, i));
    let inner = block.inner(area);
    block.render(area, f.buffer_mut());
    let flow = app.card_flow == CardFlow::InputRemark;
    let fl = if flow { 1 } else { 0 };
    let [list_a, frule_a, input_a, help_a] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(fl),
        Constraint::Length(fl),
        Constraint::Length(fl),
    ])
    .areas(inner);
    let items: Vec<ListItem> = app
        .authors
        .iter()
        .map(|(a, entry)| {
            let in_book = app.peers.iter().any(|p| &anchor_of(p) == a);
            let fallback = short_hex(a);
            let shown = entry
                .remark
                .as_deref()
                .filter(|r| !r.is_empty())
                .unwrap_or(fallback.as_str());
            let mut line = vec![
                Span::from(if entry.verified { " * " } else { " \u{00b7} " }).style(
                    if entry.verified {
                        Style::default().fg(Color::Magenta)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    },
                ),
                Span::from(shown.to_owned()).cyan(),
                Span::from(if in_book {
                    "  = book"
                } else {
                    "  not in the book"
                })
                .dark_gray(),
            ];
            if entry.verified && !in_book {
                // Theoretically unreachable (verification rides a
                // mailbox's card) — belt and braces for a drift.
                line[2] = Span::from("  ?? verified w/o card").red();
            }
            ListItem::from(Line::from(line))
        })
        .collect();
    let list_w =
        List::new(items).highlight_style(Style::default().bg(Color::DarkGray).fg(Color::White));
    ratatui::widgets::StatefulWidget::render(
        &list_w,
        list_a,
        f.buffer_mut(),
        &mut app.regions[i].authors_sel,
    );
    if flow {
        let prefix = "remark> ";
        let text_width = input_a.width.saturating_sub(prefix.len() as u16) as usize;
        let scroll = app.remark_input.visual_scroll(text_width);
        let visible: String = app
            .remark_input
            .value()
            .chars()
            .skip(scroll)
            .take(text_width)
            .collect();
        let caret = app.remark_input.visual_cursor() - scroll;
        Paragraph::new(rule(inner.width as usize, "")).render(frule_a, f.buffer_mut());
        Paragraph::new(Line::from(vec![
            Span::from(prefix).yellow(),
            Span::from(visible),
        ]))
        .render(input_a, f.buffer_mut());
        f.set_cursor_position((input_a.x + prefix.len() as u16 + caret as u16, input_a.y));
        Paragraph::new(Span::from("Enter set · Ctrl-V paste · Esc cancel").dark_gray())
            .render(help_a, f.buffer_mut());
    }
}

fn centered(area: Rect, pct_w: u16, pct_h: u16) -> Rect {
    let w = area.width * pct_w / 100;
    let h = area.height * pct_h / 100;
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn render_status(app: &App, f: &mut Frame, area: ratatui::prelude::Rect) {
    let core = match (&app.core_exit, app.core_alive) {
        (_, true) if app.relayed => Span::from("core: alive · relay: on").green(),
        (_, true) => Span::from("core: alive").green(),
        (Some(s), _) if s.success() => Span::from("core: exited clean").dark_gray(),
        (Some(s), _) => Span::from(format!("core: exited {s}")).red(),
        (None, _) => Span::from("core: GONE").red(),
    };
    let cursor = if app.cursor.is_empty() {
        Span::from("cursor: unset").dark_gray()
    } else {
        Span::from(format!("cursor: {} base(s)", app.cursor.len())).cyan()
    };
    let error = app.last_error.as_ref().map(|e| {
        Span::from(format!("error {}: {}", e.code, e.message))
            .red()
            .bold()
    });
    let flash = app.flash.as_ref().map(|m| Span::from(m).yellow());
    let extra = error.or(flash).unwrap_or_else(|| Span::from(""));
    let delivery = {
        let (covered, mailed) = app.delivered();
        if mailed > 0 {
            Span::from(format!("delivered {covered}/{mailed}")).style(if covered == mailed as u64 {
                Style::default().fg(Color::Green)
            } else {
                Style::default().fg(Color::Cyan)
            })
        } else {
            Span::from("")
        }
    };
    Line::from(vec![
        core,
        Span::from("  "),
        cursor,
        Span::from("  "),
        delivery,
        Span::from("  "),
        extra,
    ])
    .centered()
    .render(area, f.buffer_mut());
}

fn render_threads(app: &mut App, f: &mut Frame, i: usize, area: ratatui::prelude::Rect) {
    let items: Vec<ListItem> = app
        .threads
        .iter()
        .map(|t| ListItem::from(Span::from(short_hex(&t.digest))))
        .collect();
    let list = List::new(items)
        .block(
            Block::bordered()
                .title(format!(" {} {} ", i + 1, ContentKind::Threads.title()))
                .border_style(region_border(app, i)),
        )
        .highlight_style(Style::default().bg(Color::DarkGray).fg(Color::White));
    ratatui::widgets::StatefulWidget::render(
        &list,
        area,
        f.buffer_mut(),
        &mut app.regions[i].threads_sel,
    );
}

fn render_transcript(app: &mut App, f: &mut Frame, i: usize, area: ratatui::prelude::Rect) {
    let compose_here = app.show_compose && app.compose_host == i;
    let draft_rows = if compose_here {
        app.draft.lines().len().clamp(1, 8) as u16
    } else {
        0
    };
    let block = Block::bordered()
        .title(format!(" {} {} ", i + 1, ContentKind::Transcript.title()))
        .border_style(region_border(app, i));
    let inner = block.inner(area);
    block.render(area, f.buffer_mut());
    let [log_a, rule_a, draft_a, staged_a] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(if compose_here { 1 } else { 0 }),
        Constraint::Length(draft_rows),
        Constraint::Length(if compose_here { 1 } else { 0 }),
    ])
    .areas(inner);
    if compose_here {
        Paragraph::new(rule(inner.width as usize, " compose ")).render(rule_a, f.buffer_mut());
    }
    let items: Vec<ListItem> = app
        .entries
        .iter()
        .map(|e| {
            let digest = e
                .meta
                .as_ref()
                .map(|m| m.digest.clone())
                .unwrap_or_default();
            // The author observation + the signature conjunction: the
            // core reports the anchor it saw and the verified roster;
            // "confirmed from the author" is the display's judgment.
            // The author's remark (the package's local overlay) is
            // shown INSTEAD of the key fingerprint when stated.
            let from = e.meta.as_ref().map(|m| m.from.clone()).unwrap_or_default();
            let overlay = app.locals.get(&digest);
            let confirmed =
                !from.is_empty() && overlay.is_some_and(|o| o.signers.iter().any(|a| a == &from));
            let fallback = short_hex(&from);
            let author = (!from.is_empty()).then(|| {
                overlay
                    .and_then(|o| o.author_remark.as_deref())
                    .filter(|r| !r.is_empty())
                    .unwrap_or(fallback.as_str())
                    .to_owned()
            });
            entry_item(e, app.row_received(&digest), author.as_deref(), confirmed)
        })
        .collect();
    let list =
        List::new(items).highlight_style(Style::default().bg(Color::DarkGray).fg(Color::White));
    // The viewport height feeds the PgUp/PgDn page (the page is what
    // the terminal actually showed at the last paint) — per region,
    // the LOG rows only (the compose section is not paged).
    app.regions[i].page_rows = log_a.height;
    ratatui::widgets::StatefulWidget::render(
        &list,
        log_a,
        f.buffer_mut(),
        &mut app.regions[i].transcript_sel,
    );
    if compose_here {
        render_compose_body(app, f, draft_a, staged_a);
    }
}

fn entry_item(
    e: &zconned_stdio_types::Entry,
    received: Option<bool>,
    author: Option<&str>,
    confirmed: bool,
) -> ListItem<'static> {
    let ts = e
        .meta
        .as_ref()
        .and_then(|m| m.ts.as_ref())
        .and_then(|ts| jiff::Timestamp::from_second(ts.seconds).ok())
        .map(|t| t.strftime("%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| "????-??".into());
    let digest = e
        .meta
        .as_ref()
        .map(|m| short_hex(&m.digest))
        .unwrap_or_default();
    let content = e.content.as_ref().map(|slot| match &slot.value {
        Some(v) => format!("{} \u{2014} {}", v.kind, preview_bytes(&v.data)),
        None => "(value missing)".into(),
    });
    let claims = e
        .claims
        .iter()
        .map(|c| c.kind.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    // The delivery marker: ✓ = every stated frontier covers the row;
    // a bare dot = sent, receipt pending; nothing = an arrival or a
    // pre-session row (the interface does not know its facts).
    let marker = received.map(|r| {
        if r {
            Span::from(" \u{2713}").green()
        } else {
            Span::from(" \u{00b7}").dark_gray()
        }
    });
    let mut line = vec![
        Span::from(ts).dark_gray(),
        Span::from(" "),
        Span::from(digest).cyan(),
    ];
    if let Some(m) = marker {
        line.push(m);
    }
    // The author's anchor as the core observed it (or the author's
    // local remark in its place); ✍ = confirmed from the author
    // (their signature is in the row's roster).
    if let Some(a) = author {
        line.push(Span::from(format!(" from {a}")).dark_gray());
        if confirmed {
            line.push(Span::from(" *").magenta());
        }
    }
    line.push(Span::from("  "));
    line.push(Span::from(content.unwrap_or_else(|| "(no content)".into())));
    line.push(
        Span::from(if claims.is_empty() {
            String::new()
        } else {
            format!("   [{claims}]")
        })
        .dark_gray(),
    );
    ListItem::from(Line::from(line))
}

/// The composer's body INSIDE its transcript's frame: the draft
/// rows (the caret when typing; dimmed on standby) + the staged
/// line. The section rule is drawn by the transcript renderer.
fn render_compose_body(
    app: &App,
    f: &mut Frame,
    draft_a: ratatui::prelude::Rect,
    staged_a: ratatui::prelude::Rect,
) {
    let staged = match &app.staged {
        Some((content, claims)) => {
            let c = content
                .as_ref()
                .map(|v| format!("{} \u{2014} {}", v.kind, preview_bytes(&v.data)))
                .unwrap_or_else(|| "(empty)".into());
            format!("staged: {c} · {} claim(s)", claims.len())
        }
        None => "staged: (nothing)".into(),
    };
    let rows = app.draft.lines().len().clamp(1, 8) as u16;
    let width = draft_a.width as usize;
    let lines = app.draft.lines();
    if app.focus == Focus::Compose {
        // Typing: the caret rides a following viewport — vertical
        // scroll keeps the caret's row on screen; the caret's row
        // also scrolls horizontally when it runs past the width.
        let (crow, ccol) = app.draft.cursor();
        let top = crow.saturating_sub(rows as usize - 1);
        for (i, line) in lines.iter().skip(top).take(rows as usize).enumerate() {
            let row_no = top + i;
            let off = if row_no == crow {
                ccol.saturating_sub(width.saturating_sub(1))
            } else {
                0
            };
            let visible: String = line.chars().skip(off).take(width).collect();
            Line::from(Span::from(visible)).render(
                ratatui::prelude::Rect {
                    x: draft_a.x,
                    y: draft_a.y + i as u16,
                    width: draft_a.width,
                    height: 1,
                },
                f.buffer_mut(),
            );
        }
        // The caret cell: one cell AFTER the last typed character,
        // never on it.
        let caret_x =
            draft_a.x + (ccol.saturating_sub(ccol.saturating_sub(width.saturating_sub(1))) as u16);
        f.set_cursor_position((caret_x, draft_a.y + (crow - top) as u16));
    } else {
        // Standby: dimmed, and the window follows nothing (the top
        // of the draft shows).
        for (i, line) in lines.iter().take(rows as usize).enumerate() {
            let visible: String = line.chars().take(width).collect();
            Line::from(Span::from(visible).dark_gray()).render(
                ratatui::prelude::Rect {
                    x: draft_a.x,
                    y: draft_a.y + i as u16,
                    width: draft_a.width,
                    height: 1,
                },
                f.buffer_mut(),
            );
        }
    }
    Paragraph::new(Span::from(staged).dark_gray()).render(staged_a, f.buffer_mut());
}

fn keybar_hints(app: &App) -> &'static str {
    // The mode is a modal: its own one-line hint.
    if app.mode_menu {
        return "j/k select · Enter open · 1-4 direct · Esc/m close";
    }
    // Typing: the editor face. (Standby shows the focused content's
    // face below — standby IS the default state.)
    if app.focus == Focus::Compose {
        return "<Ctrl+Enter>/<Alt+Enter> send · <Ctrl+W> drop word · <Ctrl+U> clear · <Ctrl+V> paste · <Ctrl+C> copy · <Ctrl+R>/<Ctrl+S> file · Esc standby";
    }
    match app.regions[app.focus_region()].content {
        ContentKind::Threads => {
            "<Ctrl+h/j/k/l> navigate · t refresh · n new thread · Enter open · j/k move · m mode · L log · ? help · q quit"
        }
        ContentKind::Transcript => {
            "<Ctrl+h/j/k/l> navigate · j/k scroll · PgUp/PgDn page · n new thread · i compose · r staged · y copy · w save row · m mode · L log · ? help · q quit"
        }
        ContentKind::Authors => {
            "r remark · R clear remark · c copy card · s save card · j/k move · m mode · L log · ? help · q quit"
        }
        ContentKind::Mailboxes => {
            "a add card · c copy card · s save card · j/k move · m mode · L log · ? help · q quit"
        }
    }
}

/// One hint segment as styled spans: the KEY stands out (orange,
/// bold — a distinct hue from the borders' yellow), the description
/// reads at the plain dark gray.
/// A key token as styled spans:
/// - a single key: orange + bold;
/// - a combo `<Ctrl+w>`: ONLY the notation prefix `<Ctrl+` is ITALIC
///   (it is never letters to type — "Esc+" must not read as e+s+c);
///   the trailing key keeps the single-key style;
/// - the '/' between keys is a DARK separator (h/j/k/l, Esc/m,
///   PgUp/PgDn — it is never a key of its own).
fn key_spans(key: &str) -> Vec<Span<'static>> {
    const KEY: Style = Style::new().fg(Color::Indexed(208)).bold();
    const NAMED: Style = Style::new().fg(Color::Indexed(208)).bold().italic();
    const SEP: Style = Style::new().dark_gray();
    fn push_body(out: &mut Vec<Span<'static>>, body: &str) {
        let style = match body {
            "Esc" | "Enter" | "Home" | "End" | "Tab" | "PgUp" | "PgDn" => NAMED,
            _ => KEY,
        };
        // letters joined by '/' inside a combo (h/j/k/l)
        for (i, part) in body.split('/').enumerate() {
            if i > 0 {
                out.push(Span::styled("/".to_owned(), SEP));
            }
            out.push(Span::styled(part.to_owned(), style));
        }
    }
    let mut out: Vec<Span> = Vec::new();
    let mut rest = key;
    loop {
        if rest.is_empty() {
            break;
        }
        if rest.starts_with('<') {
            let Some(close) = rest.find('>') else {
                out.push(Span::styled(rest.to_owned(), KEY));
                break;
            };
            let inner = &rest[1..close];
            match inner.split_once('+') {
                // the notation prefix through the '+' is italic; the
                // trailing key is styled on its own
                Some((pfx, body)) => {
                    out.push(Span::styled("<".to_owned(), SEP));
                    out.push(Span::styled(format!("{pfx}+"), NAMED));
                    push_body(&mut out, body);
                }
                None => {
                    out.push(Span::styled("<".to_owned(), SEP));
                    push_body(&mut out, inner);
                }
            }
            out.push(Span::styled(">".to_owned(), SEP));
            rest = &rest[close + 1..];
        } else {
            let end = rest.find('/').unwrap_or(rest.len());
            push_body(&mut out, &rest[..end]);
            rest = &rest[end..];
        }
        if let Some(r) = rest.strip_prefix('/') {
            out.push(Span::styled("/".to_owned(), SEP));
            rest = r;
        }
    }
    out
}

fn hint_spans(seg: &str) -> Vec<Span<'static>> {
    match seg.split_once(' ') {
        Some((k, d)) => key_spans(k)
            .into_iter()
            .chain(std::iter::once(Span::styled(
                format!(" {d}"),
                Style::new().dark_gray(),
            )))
            .collect(),
        None => key_spans(seg),
    }
}

/// The keybar's hint lines, laid out with the right-corner extra
/// info in the budget: a line STARTS reserving the extra's width
/// plus a gap (`a`); the first segment crossing `x-a` is the WRAP
/// DECISION - the extra info leaves this line, and the line now
/// fills to the full `x` (subsequent segments ride it until `x`
/// itself overflows into the next line, which starts reserved
/// again). On the SECOND line a crossing of `x-a` ends the hints:
/// the previous hint becomes an ellipsis and the incoming one is
/// dropped. If the hints ended on a decided line, its last hint
/// shrinks into an ellipsis until the extra info fits - it always
/// rides the last line.
fn keybar_lines(hints: &str, x: u16, a: u16) -> Vec<Vec<Span<'static>>> {
    const ELLIPSIS: &str = "\u{2026}";
    let mut lines: Vec<Vec<Span>> = vec![Vec::new()];
    let mut bounds: Vec<usize> = Vec::new(); // segment starts on the current line
    let mut used: u16 = 0;
    let mut reserved = true;
    for seg in hints.split(" \u{00b7} ") {
        let spans = hint_spans(seg);
        let w: u16 = spans.iter().map(|s| s.width() as u16).sum();
        let sep_w = if used == 0 { 0 } else { 3 }; // " \u{00b7} "
        // The limit follows the line's state: reserved lines budget
        // for the extra info (x-a), decided lines fill to the full
        // width (x). The decision fires ONCE per line — afterwards
        // every segment is measured against x alone.
        let limit = if reserved { x.saturating_sub(a) } else { x };
        let mut overflow = used + sep_w + w > limit;
        if overflow && reserved {
            // The wrap decision: the extra info leaves this line;
            // the line now fills to the full width - retry against
            // x before actually wrapping.
            reserved = false;
            overflow = used + sep_w + w > x;
        }
        if overflow {
            if lines.len() >= 2 {
                // The second line never fills past its reservation:
                // the previous hint becomes the ellipsis and the
                // hints end here.
                let line = lines.last_mut().expect("the second line");
                line.truncate(*bounds.last().expect("a placed segment"));
                line.push(Span::styled(ELLIPSIS.to_owned(), Style::new().dark_gray()));
                break;
            }
            lines.push(Vec::new());
            bounds.clear();
            used = 0;
            reserved = true;
        }
        let line = lines.last_mut().expect("the current line");
        let sep_w = if used == 0 { 0 } else { 3 };
        bounds.push(line.len());
        if used > 0 {
            line.push(Span::styled(
                " \u{00b7} ".to_owned(),
                Style::new().dark_gray(),
            ));
        }
        line.extend(spans);
        used += sep_w + w;
    }
    // The extra info rides the LAST line; if the hints ended on a
    // decided line (filled past the reservation), the last hint
    // shrinks into the ellipsis until the extra fits.
    if !reserved {
        let line = lines.last_mut().expect("at least one line");
        loop {
            let used_now: u16 = line.iter().map(|s| s.width() as u16).sum();
            if used_now + a <= x || bounds.is_empty() {
                break;
            }
            line.truncate(*bounds.last().expect("a placed segment"));
            bounds.pop();
            line.push(Span::styled(ELLIPSIS.to_owned(), Style::new().dark_gray()));
        }
    }
    lines
}
fn render_keybar(
    f: &mut Frame,
    area: ratatui::prelude::Rect,
    lines: &[Vec<Span<'static>>],
    version: &str,
) {
    for (i, line) in lines.iter().enumerate() {
        let row = ratatui::prelude::Rect {
            x: area.x,
            y: area.y + i as u16,
            width: area.width,
            height: 1,
        };
        Paragraph::new(Line::from(line.clone())).render(row, f.buffer_mut());
        if i + 1 == lines.len() {
            // The right-corner extra info (italic, the color it had)
            // rides the LAST hint line — every line reserved its
            // width plus the gap, so it never covers a hint.
            Paragraph::new(Span::styled(
                version.to_owned(),
                Style::new().dark_gray().italic(),
            ))
            .alignment(Alignment::Right)
            .render(row, f.buffer_mut());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_width(line: &[Span]) -> u16 {
        line.iter().map(|s| s.width() as u16).sum()
    }

    #[test]
    fn hints_fit_one_line_with_the_extra_reserved() {
        // Two small segments: one line, and the extra info still fits.
        let lines = keybar_lines("k one \u{00b7} j two", 40, 6);
        assert_eq!(lines.len(), 1);
        assert!(line_width(&lines[0]) + 6 <= 40);
    }

    #[test]
    fn the_wrap_decision_fills_the_line_to_the_full_width() {
        // Two segments land at 15 > x-a=14 but ≤ x=20: the wrap
        // decision frees the reservation and BOTH stay on one line
        // (the old always-reserved logic would have wrapped here).
        let hints = "aaaa 1 \u{00b7} bbbb 2";
        let lines = keybar_lines(hints, 20, 6);
        assert_eq!(lines.len(), 1, "the freed a must be reused on the line");
        assert!(line_width(&lines[0]) + 6 <= 20);
    }

    #[test]
    fn the_second_line_truncates_at_its_reservation() {
        // Enough segments to spill onto line 2: there the first
        // crossing of x-a turns the previous hint into an ellipsis.
        let hints = "aaaa 1 \u{00b7} bbbb 2 \u{00b7} cccc 3 \u{00b7} dddd 4 \u{00b7} eeee 5";
        let lines = keybar_lines(hints, 20, 6);
        assert_eq!(lines.len(), 2);
        let last = lines[1].last().expect("the ellipsis").content.as_ref();
        assert_eq!(last, "\u{2026}");
        assert!(line_width(&lines[1]) + 6 <= 20);
    }
}
