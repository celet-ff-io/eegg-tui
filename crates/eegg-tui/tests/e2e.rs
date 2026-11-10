//! The end-to-end acceptance: a REAL relay and TWO REAL cores, driven
//! through the client mechanism this crate ships — spawn `zconnor`,
//! spawn two `zconned` cores against it, exchange identity cards,
//! commit on one side, and watch the message land plus the receipt
//! frontier come back. No UI.
//!
//! The binaries build from the linked zconn checkout (the local-deps
//! symlink); without it the test skips (a clone that never created
//! the link has no protocol stack to test against).

use std::{
    io::Write as _,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, RecvTimeoutError, bounded};
use zconned_stdio_types::{
    IncomingFrame, OutgoingFrame, incoming_frame::Method, outgoing_frame::Frame as Out,
};

use eegg_tui::core::{Process, CoreStdout};

/// Build the real binaries from the linked zconn tree and return
/// their paths. `None` = no linked tree (skip).
fn build_real_bins() -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../local-deps/zconn")
        .canonicalize()
        .ok()?;
    let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(["build", "-q", "--bin", "zconned", "--bin", "zconnor"])
        .current_dir(&root)
        .status()
        .expect("cargo must run for the E2E build");
    assert!(status.success(), "building the real bins failed");
    let target = match std::env::var("CARGO_TARGET_DIR") {
        Ok(dir) if std::path::Path::new(&dir).is_absolute() => std::path::PathBuf::from(dir),
        Ok(dir) => root.join(dir),
        Err(_) => root.join("target"),
    };
    Some((target.join("debug/zconned"), target.join("debug/zconnor")))
}

/// A free port, probed the honest way (bind-and-drop; a tiny race
/// window remains, the relay spawn failure would show it loudly).
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("a loopback port")
        .local_addr()
        .expect("a local addr")
        .port()
}

/// A real core process plus its frame stream, pushed by a reader
/// thread (arrivals and statements come anytime — the test polls).
struct Client {
    _core: Process,
    frames: Receiver<Out>,
}

impl Client {
    fn spawn(zconned: &std::path::Path, store: &std::path::Path, relay: &str) -> Self {
        // A distinct --store per core is LOAD-BEARING: the store root
        // holds the identity, and two cores sharing it claim the SAME
        // mailbox — the relay's one-session-per-anchor rule kills one
        // of them and every send defers.
        let cmdline = format!("{} --store {}", zconned.to_string_lossy(), store.to_string_lossy());
        let (mut core, stdout, stderr) =
            Process::spawn(&cmdline, Some(relay)).expect("core spawn");
        // The relayed core writes operational logs to stderr; park
        // them on the test's stderr for diagnosis.
        let mut stderr = stderr;
        std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            while let Ok(n) = std::io::Read::read(&mut stderr, &mut buf) {
                if n == 0 || std::io::stderr().write_all(&buf[..n]).is_err() {
                    break;
                }
            }
        });
        let mut stdout = CoreStdout::new(stdout);
        core.handshake(&mut stdout).expect("handshake");
        let (tx, rx) = bounded(64);
        std::thread::spawn(move || {
            while let Ok(Some(f)) = stdout.read_frame::<OutgoingFrame>("OutgoingFrame") {
                if let Some(out) = f.frame
                    && tx.send(out).is_err()
                {
                    break;
                }
            }
        });
        Self { _core: core, frames: rx }
    }

    fn send(&mut self, method: Method) {
        self._core
            .write_frame(&IncomingFrame { method: Some(method) })
            .expect("core stdin write");
    }

    /// Wait for the first frame matching the predicate (draining
    /// non-matching frames into the void — the streams interleave).
    fn wait_for(&self, what: &str, deadline: Duration, f: impl Fn(&Out) -> bool) -> Out {
        let start = Instant::now();
        loop {
            match self.frames.recv_timeout(Duration::from_millis(200)) {
                Ok(frame) if f(&frame) => return frame,
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) if start.elapsed() < deadline => {}
                Err(_) => panic!("timed out waiting for {what}"),
            }
        }
    }
}

#[test]
fn two_cores_one_relay_commit_arrives_and_receipt_returns() {
    // The cores' operational logs (info) reach this test's stderr —
    // the diagnosis lane when an assertion fails. (Edition-2024:
    // set_var is unsafe; single-threaded at this point.)
    unsafe { std::env::set_var("RUST_LOG", "debug") };
    let Some((zconned, zconnor)) = build_real_bins() else {
        eprintln!("skipping: no local-deps/zconn link");
        return;
    };

    // The relay, on a probe-chosen port.
    let port = free_port();
    let mut relay_proc = Command::new(&zconnor)
        .args(["--host", "127.0.0.1", "--port", &port.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("relay spawn");
    std::thread::sleep(Duration::from_millis(300));

    let dir = std::env::temp_dir().join(format!("eegg-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("a")).unwrap();
    std::fs::create_dir_all(dir.join("b")).unwrap();
    let relay_url = format!("http://127.0.0.1:{port}");

    let mut a = Client::spawn(&zconned, &dir.join("a"), &relay_url);
    let mut b = Client::spawn(&zconned, &dir.join("b"), &relay_url);

    // Identity exchange, out of band by design: each side states its
    // own card, the other side pastes it in.
    a.send(Method::SelfGet(zconned_stdio_types::SelfGet {}));
    let card_a = match a.wait_for("self card (a)", Duration::from_secs(10), |f| {
        matches!(f, Out::SelfPush(_))
    }) {
        Out::SelfPush(s) => s.card,
        _ => unreachable!(),
    };
    b.send(Method::SelfGet(zconned_stdio_types::SelfGet {}));
    let card_b = match b.wait_for("self card (b)", Duration::from_secs(10), |f| {
        matches!(f, Out::SelfPush(_))
    }) {
        Out::SelfPush(s) => s.card,
        _ => unreachable!(),
    };

    a.send(Method::PeerSet(zconned_stdio_types::PeerSet {
        peer: None,
        card: Some(card_b),
    }));
    a.wait_for("peer set receipt (a)", Duration::from_secs(10), |f| {
        matches!(f, Out::PeersPush(_))
    });
    b.send(Method::PeerSet(zconned_stdio_types::PeerSet {
        peer: None,
        card: Some(card_a),
    }));
    b.wait_for("peer set receipt (b)", Duration::from_secs(10), |f| {
        matches!(f, Out::PeersPush(_))
    });

    // A commits: fresh cursor, content, commit.
    a.send(Method::MsgCursorSet(zconned_stdio_types::MsgCursorSet {
        at: Vec::new(),
        fresh: Some(zconned_stdio_types::Fresh {}),
    }));
    a.wait_for("cursor push (a)", Duration::from_secs(10), |f| {
        matches!(f, Out::MsgCursorPush(_))
    });
    a.send(Method::MsgDraftContentSet(zconned_stdio_types::MsgDraftContentSet {
        kind: "text/plain".into(),
        data: b"e2e: over the blind relay".to_vec(),
    }));
    a.wait_for("draft push (a)", Duration::from_secs(10), |f| {
        matches!(f, Out::MsgDraftPush(_))
    });
    a.send(Method::MsgDraftCommit(zconned_stdio_types::MsgDraftCommit {}));
    a.wait_for("commit receipt (a)", Duration::from_secs(10), |f| {
        matches!(f, Out::MsgAdded(_))
    });

    // The message lands on B (the arrival is msg_added itself).
    b.wait_for("the arrival on b", Duration::from_secs(30), |f| {
        matches!(f, Out::MsgAdded(_))
    });
    println!("e2e: arrival landed on b");

    // The receipt frontier comes back to A (ack = received, never
    // read). Zero-frontier statements exist (the counterpart's
    // position pairing writes a 0 ack at connect) — wait for the one
    // that covers the commit.
    let ack = a.wait_for("the receipt on a", Duration::from_secs(30), |f| {
        matches!(f, Out::MailAcked(m) if m.up_to >= 1)
    });
    match ack {
        Out::MailAcked(m) => {
            assert_eq!(m.to_mailbox.len(), 32, "the anchor is 32 bytes");
            assert!(m.up_to >= 1, "the frontier must cover the commit");
            println!("e2e: receipt frontier up_to={}", m.up_to);
        }
        _ => unreachable!(),
    }

    // Teardown: EOF each core (the standing relay session dies with
    // the core — no half-close ceremony).
    let sa = a._core.shutdown().unwrap();
    let sb = b._core.shutdown().unwrap();
    let _ = relay_proc.kill();
    let _ = relay_proc.wait();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(sa.success() && sb.success(), "cores must exit 0 on EOF");
    println!("e2e: teardown ok");
}
