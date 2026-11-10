//! Smoke drive: the real client mechanism against a core — handshake,
//! fresh cursor, staged content, commit; replies asserted by type.
//! No UI. The wire bytes are produced by the REAL encoder (never
//! hardcoded).
//!
//! Phase 1 runs against any core: `EEGG_SMOKE_CORE` (default
//! `fake-core` on PATH) — the real `zconned` passes it equally.
//!
//! Phase 2 exercises the toy-only paths (the peers card round-trip
//! and the synthetic network arrival) against a fresh `fake-core`
//! spawn with `--arrive-after-ms 0`; override its location with
//! `EEGG_SMOKE_FAKE_CORE` when the toy is not on PATH.

use miette::IntoDiagnostic as _;
use zconned_stdio_types::{
    IncomingFrame, MsgDraftCommit, MsgDraftContentSet, incoming_frame::Method,
    outgoing_frame::Frame as Out,
};

use eegg_tui::core::{Process, CoreStdout};

fn main() -> miette::Result<()> {
    phase_flow(&std::env::var("EEGG_SMOKE_CORE").unwrap_or_else(|_| "fake-core".into()))?;
    phase_toy(&std::env::var("EEGG_SMOKE_FAKE_CORE").unwrap_or_else(|_| "fake-core".into()))?;
    Ok(())
}

/// Phase 1: the core-agnostic flow — handshake, fresh cursor, stage,
/// commit, EOF teardown.
fn phase_flow(core_cmd: &str) -> miette::Result<()> {
    let (mut core, stdout, stderr) = Process::spawn(core_cmd, None)?;
    // A noisy fake exercises the stderr capture lane.
    let _ = stderr;
    let mut stdout = CoreStdout::new(stdout);
    core.handshake(&mut stdout)?;
    println!("[phase 1] handshake ok");

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
    println!("[phase 1] cursor fresh ok");

    send(IncomingFrame {
        method: Some(Method::MsgDraftContentSet(MsgDraftContentSet {
            kind: "text/plain".into(),
            data: b"hello eegg".to_vec(),
        })),
    })?;
    assert!(matches!(recv(&mut stdout)?, Out::MsgDraftPush(_)));
    println!("[phase 1] staged ok");

    send(IncomingFrame {
        method: Some(Method::MsgDraftCommit(MsgDraftCommit {})),
    })?;
    assert!(matches!(recv(&mut stdout)?, Out::MsgAdded(_)));
    assert!(matches!(recv(&mut stdout)?, Out::MsgDraftPush(_)));
    println!("[phase 1] commit ok");

    let status = core.shutdown().into_diagnostic()?;
    assert!(status.success(), "core must exit 0 on our EOF");
    println!("[phase 1] eof teardown ok ({status})");
    Ok(())
}

/// Phase 2: the toy's own paths — the identity card, the peers
/// round-trip through the pasted-card form, the synthetic arrival.
fn phase_toy(fake_cmd: &str) -> miette::Result<()> {
    // The arrival fires mid-session (well after the request/response
    // exchanges below complete on local pipes).
    let cmdline = format!("{fake_cmd} --arrive-after-ms 500");
    let (mut core, stdout, _stderr) = Process::spawn(&cmdline, None)?;
    let mut stdout = CoreStdout::new(stdout);
    core.handshake(&mut stdout)?;
    println!("[phase 2] handshake ok");

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

    // The core's own card arrives verbatim.
    send(IncomingFrame {
        method: Some(Method::SelfGet(zconned_stdio_types::SelfGet {})),
    })?;
    let card = match recv(&mut stdout)? {
        Out::SelfPush(s) => s.card,
        other => panic!("expected SelfPush, got {other:?}"),
    };
    assert!(!card.is_empty());
    println!("[phase 2] self card ok");

    // Add the card as a peer (the pasted-card form — the core
    // decodes it), then read the set back.
    send(IncomingFrame {
        method: Some(Method::PeerSet(zconned_stdio_types::PeerSet {
            peer: None,
            card: Some(card),
        })),
    })?;
    assert!(matches!(recv(&mut stdout)?, Out::PeersPush(_)));
    send(IncomingFrame {
        method: Some(Method::PeersGet(zconned_stdio_types::PeersGet {})),
    })?;
    assert!(matches!(recv(&mut stdout)?, Out::PeersPush(_)));
    println!("[phase 2] peers round-trip ok");

    // The synthetic arrival: the notice leads, the thread list follows.
    let first = recv(&mut stdout)?;
    let second = recv(&mut stdout)?;
    assert!(
        matches!(&first, Out::MsgAdded(_)) && matches!(&second, Out::MsgThreadsPush(_)),
        "expected MsgAdded then MsgThreadsPush, got {first:?} / {second:?}"
    );
    println!("[phase 2] arrival ok");

    let status = core.shutdown().into_diagnostic()?;
    assert!(status.success(), "core must exit 0 on our EOF");
    println!("[phase 2] eof teardown ok ({status})");
    Ok(())
}
