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

use crate::app::{App, Pane, preview_bytes, short_hex};

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
}

/// The peers popup: the core's own card on top (copy source), the
/// send-target set in the middle, the paste-in input at the bottom.
fn render_peers_popup(app: &mut App, f: &mut Frame) {
    let area = centered(f.area(), 70, 60);
    f.render_widget(Clear, area);
    let [card, list, input] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(if app.peer_input_active { 3 } else { 0 }),
    ])
    .areas(area);

    let card_text = app.self_card.clone().unwrap_or_else(|| "(no card)".into());
    Paragraph::new(Line::from(Span::from(card_text).cyan()))
        .wrap(ratatui::widgets::Wrap { trim: false })
        .block(Block::bordered().title(" my card (copy for your counterpart) "))
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

    if app.peer_input_active {
        Paragraph::new(Line::from(vec![
            Span::from("paste card> ").yellow(),
            Span::from(app.peer_input.value().to_owned()),
        ]))
        .block(Block::bordered().title(" add peer "))
        .render(input, f.buffer_mut());
        let col = input.x
            + 12
            + app
                .peer_input
                .visual_cursor()
                .min(input.width.saturating_sub(13) as usize) as u16;
        f.set_cursor_position((col, input.y + 1));
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
            let digest = e.meta.as_ref().map(|m| m.digest.clone()).unwrap_or_default();
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
    line.push(Span::from(if claims.is_empty() {
        String::new()
    } else {
        format!("   [{claims}]")
    })
    .dark_gray());
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
    let inner = if app.focus == Pane::Compose {
        Line::from(vec![
            Span::from("> ").yellow(),
            Span::from(app.input.value().to_owned()),
        ])
    } else {
        Line::from(Span::from(app.input.value().to_owned()).dark_gray())
    };
    let paragraph = Paragraph::new(vec![inner, Line::from(Span::from(staged).dark_gray())]).block(
        Block::bordered()
            .title(" compose ")
            .border_style(focus_color(app, Pane::Compose)),
    );
    paragraph.render(area, f.buffer_mut());
    if app.focus == Pane::Compose {
        // The terminal cursor tracks the input caret.
        let col = area.x + 2 + app.input.visual_cursor().min(area.width as usize - 3) as u16;
        f.set_cursor_position((col, area.y + 1));
    }
}

fn render_keybar(app: &App, f: &mut Frame, area: ratatui::prelude::Rect) {
    let hints = match app.focus {
        Pane::Threads => {
            "t refresh \u{00b7} n new thread \u{00b7} Enter open \u{00b7} j/k move \u{00b7} p peers \u{00b7} Tab next \u{00b7} q quit"
        }
        Pane::Transcript => {
            "j/k scroll \u{00b7} n new thread \u{00b7} r staged \u{00b7} p peers \u{00b7} Tab next \u{00b7} q quit"
        }
        Pane::Compose => "Enter send \u{00b7} Esc clear \u{00b7} Tab next pane",
    };
    Paragraph::new(Span::from(hints).dark_gray())
        .centered()
        .render(area, f.buffer_mut());
}
