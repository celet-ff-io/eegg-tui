//! eegg — the zconn terminal interface (LPZ107): the client mechanism,
//! the C2 event model, and the ratatui surface.
//!
//! Spawn the core (`zconned`) as its own dedicated child process over
//! stdio protobuf (the nvim/LSP-client pattern); runtime topology is
//! strictly 1:1; client EOF is the only legal shutdown.
//!
//! M0 milestone: UI-complete shell — real i2c client mechanism against
//! fake cores (S2), features attach when LPZ106 completes.

pub mod app;
pub mod cli;
pub mod core;
pub mod events;
pub mod ui;

use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError};
use miette::{Context as _, IntoDiagnostic as _};

use crate::app::App;
use crate::cli::Cli;
use crate::core::Core;

/// Run the interface: spawn, handshake, event loop, teardown.
pub fn run(cli: &Cli) -> miette::Result<()> {
    // Handshake failure = LOUD failure (human intervenes): the terminal
    // is still untouched here, so miette prints to the plain stderr.
    let cmdline = cli
        .core
        .clone()
        .or_else(|| std::env::var("EEGG__CORE").ok())
        .unwrap_or_else(|| "fake-core".into());
    let (mut core, stdout, stderr) =
        Core::spawn(&cmdline).wrap_err_with(|| format!("spawning core `{cmdline}`"))?;
    let mut stdout = core::CoreStdout::new(stdout);
    core.handshake(&mut stdout).wrap_err("handshake with core")?;

    // A panic restores the terminal before the handler prints; the OS
    // closes our fds, so the core sees EOF and exits (1:1 axiom).
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        ui::restore_terminal();
        default_hook(info);
    }));

    let mut term = ui::init_terminal()?;

    let (tx, rx) = crossbeam_channel::unbounded::<events::Evt>();
    let input_thread = events::spawn_input(tx.clone());
    let reader_thread = events::spawn_reader(stdout, tx.clone());
    let stderr_thread = events::spawn_stderr_drain(stderr, tx.clone());
    drop(tx); // the three producers hold the senders now

    let mut core = Some(core);
    let mut app = App::new();
    app.refresh_threads(); // the first hand-typed request
    run_loop(&mut core, &mut app, &rx, &mut term)?;

    // Teardown order (binding): restore terminal → drop stdin →
    // reader join → wait().
    ui::restore_terminal();
    let exit = core
        .take()
        .map(|c| c.shutdown().into_diagnostic())
        .transpose()?;
    for t in [input_thread, reader_thread, stderr_thread] {
        let _ = t.join();
    }

    // Death report, after the screen is back.
    for line in &app.core_stderr {
        eprintln!("core stderr: {line}");
    }
    match exit {
        Some(s) if s.success() => Ok(()),
        Some(s) => {
            eprintln!("core exited: {s}");
            std::process::exit(s.code().unwrap_or(1));
        }
        None => {
            eprintln!("core was not reaped");
            std::process::exit(1);
        }
    }
}

fn run_loop(
    core: &mut Option<Core>,
    app: &mut App,
    rx: &Receiver<events::Evt>,
    term: &mut ui::Term,
) -> miette::Result<()> {
    const TICK: Duration = Duration::from_millis(250);
    loop {
        // The main thread is the SOLE core-stdin writer.
        if app.core_alive && core.is_some() {
            for frame in std::mem::take(&mut app.outbox) {
                if let Some(c) = core.as_mut()
                    && let Err(e) = c.write_frame(&frame) {
                        app.stream_broken(e.to_string());
                        break;
                    }
            }
        } else {
            app.outbox.clear();
        }
        term.draw(|f| ui::render(app, f)).into_diagnostic()?;

        match rx.recv_timeout(TICK) {
            Ok(evt) => {
                app.on_event(evt, core);
                while let Ok(more) = rx.try_recv() {
                    app.on_event(more, core);
                }
            }
            // A dead producer ends in CoreGone, never in a disconnect
            // while the input thread is alive; treat as a tick.
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {}
        }
        if app.quit {
            return Ok(());
        }
    }
}
