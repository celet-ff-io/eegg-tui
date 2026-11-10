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
    layout::{Constraint, Layout, Rect},
    style::{Color, Style, Stylize as _},
    text::{Line, Span},
    widgets::{Block, Clear, List, ListItem, Paragraph, Widget as _},
};

use crate::app::{App, CardFlow, Pane, preview_bytes, short_hex};

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
        render_help_popup(f);
    }
}

/// The lazygit-style help overlay: the full keymap on one card, above
/// everything (it may sit over the peers/log popups too). Closed with
/// Esc / ? / q.
fn render_help_popup(f: &mut Frame) {
    let rows: [(&str, &str); 17] = [
        ("t", "refresh threads"),
        ("n", "new thread (fresh chain on the next send)"),
        ("Enter", "open selected thread / send the compose text"),
        ("j / k \u{2191}\u{2193}", "move the selection"),
        ("g / G", "jump to the top / the bottom"),
        ("r", "re-stage the draft (transcript pane)"),
        (
            "Tab",
            "next pane (threads \u{2192} transcript \u{2192} compose)",
        ),
        ("p", "peers popup"),
        ("l", "core log popup (the core's stderr ring)"),
        ("Esc", "clear the input / back / close popups"),
        ("p \u{00b7} c", "peers: copy my card to the clipboard"),
        ("p \u{00b7} s", "peers: save my card to a file (~ expands)"),
        ("p \u{00b7} a", "peers: add the counterpart's card"),
        ("Ctrl-V", "peers add: paste the card from the clipboard"),
        ("Ctrl-R", "peers add: load the card from a file"),
        (
            "compose",
            "Enter send \u{00b7} Esc clear \u{00b7} Tab next pane",
        ),
        ("q", "quit"),
    ];
    let height = rows.len() as u16 + 2;
    let area = centered(f.area(), 64, height);
    f.render_widget(Clear, area);
    let lines: Vec<Line> = rows
        .iter()
        .map(|(key, desc)| {
            Line::from(vec![
                Span::from(format!(" {key:<10}")).yellow(),
                Span::from(desc.to_owned()),
            ])
        })
        .collect();
    Paragraph::new(lines)
        .block(
            Block::bordered()
                .title(" help \u{00b7} Esc/?/q close ")
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
    Paragraph::new(lines)
        .block(
            Block::bordered()
                .title(format!(
                    " core stderr (last {}) — l closes ",
                    app.core_stderr.len()
                ))
                .border_style(Color::Yellow),
        )
        .wrap(ratatui::widgets::Wrap { trim: false })
        .render(area, f.buffer_mut());
}

/// The peers popup: the core's own card on top (copy source), the
/// send-target set in the middle, the card/file box at the bottom.
fn render_peers_popup(app: &mut App, f: &mut Frame) {
    let area = centered(f.area(), 70, 60);
    f.render_widget(Clear, area);
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
    // (clipboard) / s (file), never through the screen.
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
        .block(Block::bordered().title(" my card — c copy · s save to file "))
        .render(card, f.buffer_mut());

    let items: Vec<ListItem> = app
        .peers
        .iter()
        .map(|p| {
            let mut line = vec![
                Span::from(short_hex(&p.sign_pub)).cyan(),
                Span::from(format!(" {} + {}", p.sign_algo, p.dh_algo)).dark_gray(),
            ];
            if !p.endpoints.is_empty() {
                line.push(Span::from(format!("  {}", p.endpoints.join(" "))));
            }
            ListItem::from(Line::from(line))
        })
        .collect();
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
                    " save my card to file (~/ expands): Enter save · Esc back ",
                    prefix,
                    visible,
                    caret as u16,
                )
            }
            CardFlow::None => unreachable!("checked above"),
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
            entry_item(e, app.row_received(&digest))
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

fn entry_item(e: &zconned_stdio_types::Entry, received: Option<bool>) -> ListItem<'static> {
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
        // The terminal cursor tracks the input caret.
        f.set_cursor_position((area.x + 2 + caret_col as u16, area.y + 1));
    }
}

fn render_keybar(app: &App, f: &mut Frame, area: ratatui::prelude::Rect) {
    let hints = match app.focus {
        Pane::Threads => {
            "t refresh \u{00b7} n new thread \u{00b7} Enter open \u{00b7} j/k move \u{00b7} p peers \u{00b7} l log \u{00b7} Tab next \u{00b7} ? help \u{00b7} q quit"
        }
        Pane::Transcript => {
            "j/k scroll \u{00b7} n new thread \u{00b7} r staged \u{00b7} p peers \u{00b7} l log \u{00b7} Tab next \u{00b7} ? help \u{00b7} q quit"
        }
        Pane::Compose => "Enter send \u{00b7} Esc clear \u{00b7} Tab next pane",
    };
    Paragraph::new(Span::from(hints).dark_gray())
        .centered()
        .render(area, f.buffer_mut());
}
