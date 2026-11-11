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

use crate::app::{App, CardFlow, Pane, ProfileTab, anchor_of, preview_bytes, short_hex};

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

fn focus_color(app: &App, pane: Pane) -> Color {
    if app.focus == pane {
        Color::Yellow
    } else {
        Color::DarkGray
    }
}

pub fn render(app: &mut App, f: &mut Frame) {
    let [status, main, compose, keybar] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .areas(f.area());
    let [threads, transcript] =
        Layout::horizontal([Constraint::Percentage(30), Constraint::Percentage(70)]).areas(main);

    render_status(app, f, status);
    render_threads(app, f, threads);
    render_transcript(app, f, transcript);
    render_compose(app, f, compose);
    render_keybar(app, f, keybar);
    if app.show_peers {
        render_peers_popup(app, f);
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
    let rows: [(bool, &str, &str); 45] = [
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
        (true, "n", "new thread (fresh chain on the next send)"),
        (true, "r", "re-stage the draft"),
        (true, "y", "copy the selected row's text to the clipboard"),
        (true, "w", "save the selected row's text to a file"),
        (false, "compose", ""),
        (true, "Enter", "send the compose text"),
        (true, "Ctrl-C", "copy the compose text to the clipboard"),
        (true, "Ctrl-V", "paste the clipboard at the caret"),
        (true, "Ctrl-R", "load the compose text from a file"),
        (true, "Ctrl-S", "save the compose text to a file"),
        (true, "Esc", "clear the compose text"),
        (false, "profile", ""),
        (true, "p", "open the profile popup"),
        (true, "Tab", "flip mailboxes \u{2194} authors"),
        (
            true,
            "+ record",
            "the card's third key (record signing); · no record key = a legacy card — re-exchange it",
        ),
        (
            true,
            "c",
            "copy the selected card (a mailbox or a rostered author)",
        ),
        (true, "s", "save the selected card to a file"),
        (true, "a", "add the counterpart's card (mailboxes tab)"),
        (true, "Ctrl-V", "paste the card from the clipboard"),
        (true, "Ctrl-R", "load the card from a file"),
        (true, "j / k \u{2191}\u{2193}", "move the selection"),
        (true, "Esc", "back a level / close the popup"),
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
        (false, "global", ""),
        (
            true,
            "Tab",
            "next pane (threads \u{2192} transcript \u{2192} compose)",
        ),
        (true, "h", "the threads pane (left)"),
        (true, "l", "the transcript pane (right)"),
        (true, "L", "core log popup (the core's stderr ring)"),
        (true, "q", "quit"),
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

/// The profile popup: two domains on two tabs (Tab flips). The
/// MAILBOXES tab: the core's own card on top (copy source), the
/// send-target set in the middle, the card/file box at the bottom.
/// The AUTHORS tab: the sender identities the received messages
/// have shown, verified vs observed.
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
        _ => line.push(Span::from("  · no record key").red()),
    }
    if !p.endpoints.is_empty() {
        line.push(Span::from(format!("  {}", p.endpoints.join(" "))));
    }
    ListItem::from(Line::from(line))
}

fn render_peers_popup(app: &mut App, f: &mut Frame) {
    let area = centered(f.area(), 70, 60);
    f.render_widget(Clear, area);
    if app.profile_tab == ProfileTab::Authors {
        return render_authors_tab(app, f, area);
    }
    let [card, list, input] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(if app.card_flow != CardFlow::None {
            3
        } else {
            0
        }),
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
        .block(Block::bordered().title(" my card — j/k selects (me) or a peer · c copy · s save "))
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
                .border_style(Color::Yellow),
        )
        .highlight_style(Style::default().bg(Color::DarkGray).fg(Color::White));
    ratatui::widgets::StatefulWidget::render(&list_w, list, f.buffer_mut(), &mut app.peers_sel);

    if app.card_flow != CardFlow::None {
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
                    " add peer: Enter submit · Ctrl-V clipboard · Ctrl-R file · Esc cancel ",
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
                    " read card from file: Enter load · Esc back ",
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
                    " save the selected card to file (~/ expands): Enter save · Esc back ",
                    prefix,
                    visible,
                    caret as u16,
                )
            }
            CardFlow::None => unreachable!("checked above"),
            CardFlow::InputRemark => unreachable!("the authors tab renders its own remark box"),
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

fn render_threads(app: &mut App, f: &mut Frame, area: ratatui::prelude::Rect) {
    let items: Vec<ListItem> = app
        .threads
        .iter()
        .map(|t| ListItem::from(Span::from(short_hex(&t.digest))))
        .collect();
    let list = List::new(items)
        .block(
            Block::bordered()
                .title(" threads ")
                .border_style(focus_color(app, Pane::Threads)),
        )
        .highlight_style(Style::default().bg(Color::DarkGray).fg(Color::White));
    ratatui::widgets::StatefulWidget::render(&list, area, f.buffer_mut(), &mut app.thread_sel);
}

fn render_transcript(app: &mut App, f: &mut Frame, area: ratatui::prelude::Rect) {
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
                .title(" transcript ")
                .border_style(focus_color(app, Pane::Transcript)),
        )
        .highlight_style(Style::default().bg(Color::DarkGray).fg(Color::White));
    ratatui::widgets::StatefulWidget::render(&list, area, f.buffer_mut(), &mut app.transcript_sel);
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

/// The AUTHORS tab of the profile popup: every sender identity the
/// session observed — ✍ = its record signature was rostered
/// (objective authorship, verified by the core); · = seen on an
/// envelope only. Each row shows the identity's local remark (the
/// key fingerprint only as the fallback), cross-referenced with
/// the book.
fn render_authors_tab(app: &mut App, f: &mut Frame, area: ratatui::prelude::Rect) {
    // The remark input box shares the popup's bottom-edge pattern
    // with the mailbox tab's card flows.
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
                .title(format!(
                    " authors ({}) — Tab flips to mailboxes ",
                    app.authors.len()
                ))
                .border_style(Color::Yellow),
        )
        .highlight_style(Style::default().bg(Color::DarkGray).fg(Color::White));
    ratatui::widgets::StatefulWidget::render(&list_w, list, f.buffer_mut(), &mut app.authors_sel);
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
                " set the selected identity's remark: Enter set · Ctrl-V paste · Esc cancel ",
            ),
        )
        .render(input, f.buffer_mut());
        f.set_cursor_position((
            input.x + 1 + prefix.len() as u16 + caret as u16,
            input.y + 1,
        ));
    }
}

fn render_compose(app: &App, f: &mut Frame, area: ratatui::prelude::Rect) {
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
    // The visible window follows the caret — long lines scroll
    // horizontally instead of hiding their tail.
    let text_width = area.width.saturating_sub(4) as usize; // borders + "> "
    let (visible, caret_col) = if app.focus == Pane::Compose {
        let scroll = app.input.visual_scroll(text_width);
        let v: String = app
            .input
            .value()
            .chars()
            .skip(scroll)
            .take(text_width)
            .collect();
        (v, app.input.visual_cursor() - scroll)
    } else {
        (app.input.value().to_owned(), 0)
    };
    let inner = if app.focus == Pane::Compose {
        Line::from(vec![Span::from("> ").yellow(), Span::from(visible)])
    } else {
        Line::from(Span::from(visible).dark_gray())
    };
    let paragraph = Paragraph::new(vec![inner, Line::from(Span::from(staged).dark_gray())]).block(
        Block::bordered()
            .title(" compose ")
            .border_style(focus_color(app, Pane::Compose)),
    );
    paragraph.render(area, f.buffer_mut());
    if app.focus == Pane::Compose {
        // The terminal cursor tracks the input caret: the text starts
        // two spans in (the border, then the "> " prompt), so the
        // caret cell is x + 3 + caret — one cell AFTER the last typed
        // character, never on it.
        f.set_cursor_position((area.x + 3 + caret_col as u16, area.y + 1));
    }
}

fn render_keybar(app: &App, f: &mut Frame, area: ratatui::prelude::Rect) {
    let hints = match app.focus {
        Pane::Threads => {
            "t refresh \u{00b7} n new thread \u{00b7} Enter open \u{00b7} j/k move \u{00b7} l right \u{00b7} p profile \u{00b7} L log \u{00b7} Tab next \u{00b7} ? help \u{00b7} q quit"
        }
        Pane::Transcript => {
            "j/k scroll \u{00b7} n new thread \u{00b7} r staged \u{00b7} y copy \u{00b7} w save row \u{00b7} h left \u{00b7} p profile \u{00b7} L log \u{00b7} Tab next \u{00b7} ? help \u{00b7} q quit"
        }
        Pane::Compose => {
            "Enter send \u{00b7} Ctrl-V paste \u{00b7} Ctrl-C copy \u{00b7} Ctrl-R/S file \u{00b7} Esc clear \u{00b7} Tab next"
        }
    };
    Paragraph::new(Span::from(hints).dark_gray())
        .centered()
        .render(area, f.buffer_mut());
    // The lazygit touch: the version lives in the bottom-right corner
    // (compile-time — the crate version, the single version carrier).
    Paragraph::new(Span::from(format!("v{}", env!("CARGO_PKG_VERSION"))).dark_gray())
        .alignment(Alignment::Right)
        .render(area, f.buffer_mut());
}
