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

use crate::app::{
    App, CardFlow, ComposeMode, ContentKind, Focus, anchor_of, preview_bytes, short_hex,
};

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
    if app.wheel {
        let focused = match app.focus {
            Focus::Region(i) => i.min(app.regions.len() - 1),
            Focus::Compose => app.compose_host.min(app.regions.len() - 1),
        };
        let area = if focused == 0 { r0 } else { r1 };
        render_wheel(app, f, area);
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

/// One region: its content, or the composer's 日 split when THIS
/// region hosts an open composer (the composer is the transcript's
/// sub-window — the log above, the compose below).
fn render_region(app: &mut App, f: &mut Frame, i: usize, area: ratatui::prelude::Rect) {
    if app.show_compose
        && app.compose_host == i
        && app.regions[i].content == ContentKind::Transcript
    {
        let rows = app.draft.lines().len().clamp(1, 8) as u16;
        let [t_area, c_area] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(rows + 3)]).areas(area);
        render_transcript(app, f, i, t_area);
        render_compose(app, f, c_area);
        return;
    }
    match app.regions[i].content {
        ContentKind::Threads => render_threads(app, f, i, area),
        ContentKind::Transcript => render_transcript(app, f, i, area),
        ContentKind::Authors => render_authors(app, f, i, area),
        ContentKind::Mailboxes => render_mailboxes(app, f, i, area),
    }
}

/// A region border's color: yellow when the focus is here, dark
/// gray otherwise.
fn region_border(app: &App, i: usize) -> Color {
    if app.focus == Focus::Region(i) {
        Color::Yellow
    } else {
        Color::DarkGray
    }
}

/// The wheel: the in-place content menu, drawn over the focused
/// region's own area (the rest of the screen stays visible). The
/// current content is marked — selecting it just closes.
fn render_wheel(app: &App, f: &mut Frame, region_area: ratatui::prelude::Rect) {
    let current = app.regions[match app.focus {
        Focus::Region(i) => i.min(app.regions.len() - 1),
        Focus::Compose => app.compose_host.min(app.regions.len() - 1),
    }]
    .content;
    let lines: Vec<Line> = (0..4)
        .map(|n| {
            let kind = ContentKind::nth(n);
            let mark = if kind == current { "\u{25cf} " } else { "  " };
            Line::from(vec![
                Span::from(format!(" {mark}{}", n + 1)).yellow().bold(),
                Span::from(format!(" {}", kind.title())).cyan(),
            ])
        })
        .collect();
    let area = centered(region_area, 40, 8);
    f.render_widget(Clear, area);
    Paragraph::new(lines)
        .block(
            Block::bordered()
                .title(" wheel \u{00b7} 1-4 content \u{00b7} Esc/o close ")
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
    let rows: [(bool, &str, &str); 57] = [
        (false, "global", ""),
        (
            true,
            "<ctrl+hjkl>",
            "navigate from anywhere: h = region 1 · l = region 2 · j = next · k = previous",
        ),
        (
            true,
            "h / l",
            "the same left/right moves (plain keys — the lists and the composer keep what they use)",
        ),
        (true, "Tab", "cycle focus (regions, then the composer)"),
        (true, "1 / 2", "jump straight to region 1 / 2"),
        (true, "o", "the wheel: switch this region's content"),
        (true, "p", "show the mailboxes here"),
        (true, "L", "core log popup (the core's stderr ring)"),
        (true, "q", "quit"),
        (false, "wheel", ""),
        (
            true,
            "1..4",
            "switch this region's content (the current one just closes)",
        ),
        (true, "Esc / o", "close, no change"),
        (false, "threads", ""),
        (true, "t", "refresh the thread list"),
        (true, "n", "new thread (fresh chain on the next send)"),
        (true, "Enter", "open the selected thread"),
        (true, "j / k \u{2191}\u{2193}", "move the selection"),
        (true, "g / G", "jump to the top / the bottom"),
        (false, "transcript", ""),
        (
            true,
            "from <anchor>",
            "the row's author; \u{270d} = roster-confirmed",
        ),
        (true, "j / k \u{2191}\u{2193}", "scroll"),
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
        (true, "<ctrl+enter>", "send the compose text"),
        (true, "<alt+enter>", "send the compose text (either works)"),
        (true, "<ctrl+w>", "drop the word before the caret"),
        (
            true,
            "Home / End",
            "line start / end (<ctrl+home>/<ctrl+end> = the whole draft)",
        ),
        (true, "<ctrl+u>", "clear the compose text"),
        (true, "<ctrl+c>", "copy the compose text to the clipboard"),
        (true, "<ctrl+v>", "paste the clipboard at the caret"),
        (true, "<ctrl+r>", "load the compose text from a file"),
        (true, "<ctrl+s>", "save the compose text to a file"),
        (
            true,
            "Esc",
            "typing \u{2192} moving; again = hide the composer",
        ),
        (
            true,
            "hjkl (moving)",
            "navigate while the composer is in the moving mode",
        ),
        (true, "i (moving)", "back to typing"),
        (false, "mailboxes", ""),
        (
            true,
            "+ record",
            "the card's third key (record signing); \u{00b7} no record key = a legacy card — re-exchange it",
        ),
        (
            true,
            "c",
            "copy the selected card (a mailbox or a rostered author)",
        ),
        (true, "s", "save the selected card to a file"),
        (true, "a", "add the counterpart's card"),
        (true, "<ctrl+v>", "paste the card from the clipboard"),
        (true, "<ctrl+r>", "load the card from a file"),
        (true, "j / k \u{2191}\u{2193}", "move the selection"),
        (true, "Esc", "back a level / cancel the flow"),
        (false, "authors", ""),
        (
            true,
            "\u{270d}",
            "verified authorship (the record signature rostered)",
        ),
        (
            true,
            "\u{00b7}",
            "observed on an envelope only (unverified)",
        ),
        (true, "r", "set the selected identity's remark"),
        (true, "R", "clear the selected identity's remark"),
        (false, "log", ""),
        (
            true,
            "j / k \u{2191}\u{2193}",
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
            let line = if *row {
                // lazygit style: the key column right-aligned, the
                // description tight against it.
                Line::from(vec![
                    Span::from(" "),
                    Span::from(format!("{key:>12} ")).yellow(),
                    Span::from(desc.to_owned()),
                ])
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
    let [card, list, input] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(if flow { 3 } else { 0 }),
    ])
    .areas(area);

    // The card box shows TWO text lines: a longer card is clipped
    // with an explicit ellipsis — the full text travels through c
    // (clipboard) / s (file) on the SELECTED row, never through the
    // screen.
    let card_text = app.self_card.clone().unwrap_or_else(|| "(no card)".into());
    let capacity = card.width.saturating_sub(2) as usize * 2;
    let shown = if card_text.chars().count() > capacity {
        let kept: String = card_text.chars().take(capacity.saturating_sub(1)).collect();
        format!("{kept}\u{2026}")
    } else {
        card_text
    };
    Paragraph::new(Line::from(Span::from(shown).cyan()))
        .wrap(ratatui::widgets::Wrap { trim: false })
        .block(
            Block::bordered()
                .title(format!(
                    " {} my card — j/k selects (me) or a peer \u{00b7} c copy \u{00b7} s save ",
                    i + 1
                ))
                .border_style(region_border(app, i)),
        )
        .render(card, f.buffer_mut());

    // The selection spans me + the peer set: row 0 = me (decoded
    // from my card text for the same record display).
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
    let list_w = List::new(items)
        .block(
            Block::bordered()
                .title(format!(" peers ({}) ", app.peers.len()))
                .border_style(region_border(app, i)),
        )
        .highlight_style(Style::default().bg(Color::DarkGray).fg(Color::White));
    ratatui::widgets::StatefulWidget::render(
        &list_w,
        list,
        f.buffer_mut(),
        &mut app.regions[i].mailboxes_sel,
    );

    if flow {
        let (title, prefix, value, caret_col) = match app.card_flow {
            // A card is one long base64 line: the visible window
            // follows the caret (horizontal scroll), otherwise the
            // tail is unseeable.
            CardFlow::InputCard => {
                let prefix = "paste card> ";
                let text_width = input.width.saturating_sub(2 + prefix.len() as u16) as usize;
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
                    " add peer: Enter submit \u{00b7} Ctrl-V clipboard \u{00b7} Ctrl-R file \u{00b7} Esc cancel ",
                    prefix,
                    visible,
                    caret as u16,
                )
            }
            CardFlow::ReadCard => {
                let prefix = "card file> ";
                let text_width = input.width.saturating_sub(2 + prefix.len() as u16) as usize;
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
                    " read card from file: Enter load \u{00b7} Esc back ",
                    prefix,
                    visible,
                    caret as u16,
                )
            }
            CardFlow::SaveCard => {
                let prefix = "save to> ";
                let text_width = input.width.saturating_sub(2 + prefix.len() as u16) as usize;
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
                    " save the selected card to file (~/ expands): Enter save \u{00b7} Esc back ",
                    prefix,
                    visible,
                    caret as u16,
                )
            }
            CardFlow::None | CardFlow::InputRemark => unreachable!("checked above"),
        };
        Paragraph::new(Line::from(vec![
            Span::from(prefix).yellow(),
            Span::from(value),
        ]))
        .block(Block::bordered().title(title))
        .render(input, f.buffer_mut());
        f.set_cursor_position((input.x + 1 + prefix.len() as u16 + caret_col, input.y + 1));
    }
}

/// The AUTHORS region content: every sender identity the session
/// observed — ✍ = its record signature was rostered (objective
/// authorship, verified by the core); · = seen on an envelope only.
/// Each row shows the identity's local remark (the key fingerprint
/// only as the fallback), cross-referenced with the book.
fn render_authors(app: &mut App, f: &mut Frame, i: usize, area: ratatui::prelude::Rect) {
    // The remark input box sits at the region's bottom edge.
    let [list, input] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(if app.card_flow == CardFlow::InputRemark {
            3
        } else {
            0
        }),
    ])
    .areas(area);
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
                Span::from(if entry.verified {
                    " \u{270d} "
                } else {
                    " \u{00b7} "
                })
                .style(if entry.verified {
                    Style::default().fg(Color::Magenta)
                } else {
                    Style::default().fg(Color::DarkGray)
                }),
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
    let list_w = List::new(items)
        .block(
            Block::bordered()
                .title(format!(" {} authors ({}) ", i + 1, app.authors.len()))
                .border_style(region_border(app, i)),
        )
        .highlight_style(Style::default().bg(Color::DarkGray).fg(Color::White));
    ratatui::widgets::StatefulWidget::render(
        &list_w,
        list,
        f.buffer_mut(),
        &mut app.regions[i].authors_sel,
    );
    if app.card_flow == CardFlow::InputRemark {
        let prefix = "remark> ";
        let text_width = input.width.saturating_sub(2 + prefix.len() as u16) as usize;
        let scroll = app.remark_input.visual_scroll(text_width);
        let visible: String = app
            .remark_input
            .value()
            .chars()
            .skip(scroll)
            .take(text_width)
            .collect();
        let caret = app.remark_input.visual_cursor() - scroll;
        Paragraph::new(Line::from(vec![
            Span::from(prefix).yellow(),
            Span::from(visible),
        ]))
        .block(
            Block::bordered().title(
                " set the selected identity's remark: Enter set \u{00b7} Ctrl-V paste \u{00b7} Esc cancel ",
            ),
        )
        .render(input, f.buffer_mut());
        f.set_cursor_position((
            input.x + 1 + prefix.len() as u16 + caret as u16,
            input.y + 1,
        ));
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
    let list = List::new(items)
        .block(
            Block::bordered()
                .title(format!(" {} {} ", i + 1, ContentKind::Transcript.title()))
                .border_style(region_border(app, i)),
        )
        .highlight_style(Style::default().bg(Color::DarkGray).fg(Color::White));
    // The viewport height feeds the PgUp/PgDn page (the page is what
    // the terminal actually showed at the last paint) — per region.
    app.regions[i].page_rows = area.height.saturating_sub(2);
    ratatui::widgets::StatefulWidget::render(
        &list,
        area,
        f.buffer_mut(),
        &mut app.regions[i].transcript_sel,
    );
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
            line.push(Span::from(" \u{270d}").magenta());
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

fn render_compose(app: &App, f: &mut Frame, area: ratatui::prelude::Rect) {
    if area.height == 0 {
        return;
    }
    let staged = match &app.staged {
        Some((content, claims)) => {
            let c = content
                .as_ref()
                .map(|v| format!("{} \u{2014} {}", v.kind, preview_bytes(&v.data)))
                .unwrap_or_else(|| "(empty)".into());
            format!("staged: {c} \u{00b7} {} claim(s)", claims.len())
        }
        None => "staged: (nothing)".into(),
    };
    // The moving mode is visibly distinct: a cyan border + the ·nav·
    // title, so the double-Esc (leave → hide) is never a surprise.
    let (title, border) = if app.compose_mode == ComposeMode::Nav {
        (" compose ·nav· ", Color::Cyan)
    } else if app.focus == Focus::Compose {
        (" compose ", Color::Yellow)
    } else {
        (" compose ", Color::DarkGray)
    };
    let block = Block::bordered().title(title).border_style(border);
    let inner = block.inner(area);
    block.render(area, f.buffer_mut());
    let rows = app.draft.lines().len().clamp(1, 8) as u16;
    let [text_a, staged_a] =
        Layout::vertical([Constraint::Length(rows), Constraint::Min(0)]).areas(inner);
    let width = text_a.width as usize;
    let lines = app.draft.lines();
    if app.focus == Focus::Compose && app.compose_mode == ComposeMode::Input {
        // The caret rides a following viewport: vertical scroll keeps
        // the caret's row on screen; the caret's row also scrolls
        // horizontally when it runs past the width.
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
                    x: text_a.x,
                    y: text_a.y + i as u16,
                    width: text_a.width,
                    height: 1,
                },
                f.buffer_mut(),
            );
        }
        // The caret cell: one cell AFTER the last typed character,
        // never on it.
        let caret_x =
            text_a.x + (ccol.saturating_sub(ccol.saturating_sub(width.saturating_sub(1))) as u16);
        f.set_cursor_position((caret_x, text_a.y + (crow - top) as u16));
    } else {
        // Unfocused = dimmed; focused-but-moving = normal color (the
        // face swap is already visible on the border/title). The
        // window still follows the caret row when focused.
        let dim = app.focus != Focus::Compose;
        let top = if dim {
            0
        } else {
            app.draft.cursor().0.saturating_sub(rows as usize - 1)
        };
        for (i, line) in lines.iter().skip(top).take(rows as usize).enumerate() {
            let visible: String = line.chars().take(width).collect();
            let span = Span::from(visible);
            Line::from(if dim { span.dark_gray() } else { span }).render(
                ratatui::prelude::Rect {
                    x: text_a.x,
                    y: text_a.y + i as u16,
                    width: text_a.width,
                    height: 1,
                },
                f.buffer_mut(),
            );
        }
    }
    Paragraph::new(Span::from(staged).dark_gray()).render(staged_a, f.buffer_mut());
}

fn keybar_hints(app: &App) -> &'static str {
    // The wheel is a modal: its own one-line hint.
    if app.wheel {
        return "1..4 content · Esc/o close";
    }
    if app.focus == Focus::Compose {
        return match app.compose_mode {
            // Typing mode: the editor face.
            ComposeMode::Input => {
                "<ctrl+enter>/<alt+enter> send · <ctrl+w> drop word · <ctrl+u> clear · <ctrl+v> paste · <ctrl+c> copy · <ctrl+r>/<ctrl+s> file · Esc leave"
            }
            // Moving mode: plain hjkl navigates (the content layer is
            // free here); i returns to typing; Esc hides the composer.
            ComposeMode::Nav => "hjkl navigate · i type · Esc hide",
        };
    }
    match app.regions[app.focus_region()].content {
        ContentKind::Threads => {
            "<ctrl+hjkl> navigate · Tab cycle · 1/2 region · t refresh · n new thread · Enter open · j/k move · o wheel · L log · ? help · q quit"
        }
        ContentKind::Transcript => {
            "<ctrl+hjkl> navigate · Tab cycle · 1/2 region · j/k scroll · PgUp/PgDn page · n new thread · i compose · r staged · y copy · w save row · o wheel · L log · ? help · q quit"
        }
        ContentKind::Authors => {
            "r remark · R clear remark · c copy card · s save card · j/k move · Tab cycle · o wheel · L log · ? help · q quit"
        }
        ContentKind::Mailboxes => {
            "a add card · c copy card · s save card · j/k move · Tab cycle · o wheel · L log · ? help · q quit"
        }
    }
}

/// One hint segment as styled spans: the KEY stands out (orange,
/// bold — a distinct hue from the borders' yellow), the description
/// reads at the plain dark gray.
fn hint_spans(seg: &str) -> Vec<Span<'static>> {
    const KEY: Style = Style::new().fg(Color::Indexed(208)).bold();
    match seg.split_once(' ') {
        Some((k, d)) => vec![
            Span::styled(k.to_owned(), KEY),
            Span::styled(format!(" {d}"), Style::new().dark_gray()),
        ],
        None => vec![Span::styled(seg.to_owned(), KEY)],
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
