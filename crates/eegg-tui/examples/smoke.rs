//! Smoke drive: the real client mechanism against a fake core —
//! handshake, fresh cursor, staged content, commit; replies asserted
//! by type. No UI. The wire bytes are produced by the REAL encoder
//! (never hardcoded).
//!
//! The core command defaults to `fake-core` on PATH; override with
//! `EEGG_SMOKE_CORE` (e.g. the workspace target dir + `/fake-core`).

use miette::IntoDiagnostic as _;
use zconned_stdio_types::{
    IncomingFrame, MsgDraftCommit, MsgDraftContentSet, incoming_frame::Method,
    outgoing_frame::Frame as Out,
};

use eegg_tui::core::{Core, CoreStdout};

fn main() -> miette::Result<()> {
    let core_cmd = std::env::var("EEGG_SMOKE_CORE").unwrap_or_else(|_| "fake-core".into());
    let (mut core, stdout, stderr) = Core::spawn(&core_cmd)?;
    // A noisy fake exercises the stderr capture lane.
    let _ = stderr;
    let mut stdout = CoreStdout::new(stdout);
    core.handshake(&mut stdout)?;
    println!("handshake ok");

    let mut send = |frame: IncomingFrame| -> miette::Result<()> { Ok(core.write_frame(&frame)?) };
    let recv = |stdout: &mut CoreStdout| -> miette::Result<Out> {
        match stdout.read_frame::<zconned_stdio_types::OutgoingFrame>("OutgoingFrame")? {
            Some(f) => match f.frame {
                Some(out) => Ok(out),
                None => Err(miette::miette!("all-default frame")),
            },
            None => Err(miette::miette!("core stdout EOF")),
        }
    };

    use zconned_stdio_types::MsgCursorSet;
    send(IncomingFrame {
        method: Some(Method::MsgCursorSet(MsgCursorSet {
            at: Vec::new(),
            fresh: Some(zconned_stdio_types::Fresh {}),
        })),
    })?;
    assert!(matches!(recv(&mut stdout)?, Out::MsgCursorPush(_)));
    println!("cursor fresh ok");

    send(IncomingFrame {
        method: Some(Method::MsgDraftContentSet(MsgDraftContentSet {
            kind: "text/plain".into(),
            data: b"hello eegg".to_vec(),
        })),
    })?;
    assert!(matches!(recv(&mut stdout)?, Out::MsgDraftPush(_)));
    println!("staged ok");

    send(IncomingFrame {
        method: Some(Method::MsgDraftCommit(MsgDraftCommit {})),
    })?;
    assert!(matches!(recv(&mut stdout)?, Out::MsgAdded(_)));
    assert!(matches!(recv(&mut stdout)?, Out::MsgDraftPush(_)));
    println!("commit ok");

    let status = core.shutdown().into_diagnostic()?;
    assert!(status.success(), "core must exit 0 on our EOF");
    println!("eof teardown ok ({status})");
    Ok(())
}
