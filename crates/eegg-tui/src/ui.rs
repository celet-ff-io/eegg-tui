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
    layout::{Constraint, Layout},
    style::{Color, Style, Stylize as _},
    text::{Line, Span},
    widgets::{Block, List, ListItem, Paragraph, Widget as _},
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
    Line::from(vec![
        core,
        Span::from("  "),
        cursor,
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
    let items: Vec<ListItem> = app.entries.iter().map(entry_item).collect();
    let list = List::new(items)
        .block(
            Block::bordered()
                .title(" transcript ")
                .border_style(focus_color(app, Pane::Transcript)),
        )
        .highlight_style(Style::default().bg(Color::DarkGray).fg(Color::White));
    ratatui::widgets::StatefulWidget::render(&list, area, f.buffer_mut(), &mut app.transcript_sel);
}

fn entry_item(e: &zconned_stdio_types::Entry) -> ListItem<'static> {
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
    ListItem::from(Line::from(vec![
        Span::from(ts).dark_gray(),
        Span::from(" "),
        Span::from(digest).cyan(),
        Span::from("  "),
        Span::from(content.unwrap_or_else(|| "(no content)".into())),
        Span::from(if claims.is_empty() {
            String::new()
        } else {
            format!("   [{claims}]")
        })
        .dark_gray(),
    ]))
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
            "t refresh \u{00b7} Enter open \u{00b7} j/k move \u{00b7} Tab next pane \u{00b7} q quit"
        }
        Pane::Transcript => "j/k scroll \u{00b7} r staged \u{00b7} Tab next pane \u{00b7} q quit",
        Pane::Compose => "type \u{00b7} Enter send \u{00b7} Esc clear \u{00b7} Tab next pane",
    };
    Paragraph::new(Span::from(hints).dark_gray())
        .centered()
        .render(area, f.buffer_mut());
}
