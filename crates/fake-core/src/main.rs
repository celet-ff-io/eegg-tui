//! fake-core — a minimal stand-in for the `zconned` core process, built
//! to drive the eegg shell (LPZ107 M0). The real wire contract
//! (handshake v2, framed protobuf, the day-one verb set), a toy
//! in-memory state behind it. NOT the core: core logic lives in
//! LPZ106 (B18) — this exists so the interface can run without it.

use std::{
    collections::HashMap,
    io::Read as _,
    process::ExitCode,
    time::{SystemTime, UNIX_EPOCH},
};

use bytes::BytesMut;
use clap::{
    Parser,
    builder::{
        Styles,
        styling::{AnsiColor, Effects},
    },
};
use clap_verbosity_flag::{Verbosity, WarnLevel};
use prost::Message;
use protobuf_varint_deframer::Deframer;
use zconned_material::{CLAIM_KIND_TS, MsgRef, ResourceRef, Value, resource_digest};
use zconned_stdio_types::{
    Handshake, IncomingFrame, OutgoingFrame, PROTOCOL_VERSION, incoming_frame::Method,
    outgoing_frame::Frame as Out,
};

static STYLES: Styles = Styles::styled()
    .header(AnsiColor::Yellow.on_default().effects(Effects::BOLD))
    .usage(AnsiColor::Yellow.on_default().effects(Effects::BOLD))
    .literal(AnsiColor::Green.on_default().effects(Effects::BOLD))
    .placeholder(AnsiColor::Red.on_default());

/// fake-core - a minimal mock of the zconned core for the eegg shell.
#[derive(Debug, Parser)]
#[command(version)]
#[command(styles = STYLES.clone())]
struct Args {
    #[command(flatten)]
    verbose: Verbosity<WarnLevel>,
}

type Result<T> = core::result::Result<T, Framing>;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
enum Framing {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to deframe frame: {0}")]
    Deframe(#[from] protobuf_varint_deframer::Error),
    #[error("eof truncates frame")]
    TruncatedFrame,
    #[error("failed to decode frame: {0}")]
    Decode(#[from] prost::DecodeError),
}

/// Sync framed reader over a std stream (the deframer state machine,
/// the sync core underneath 106's async io-link adapter).
struct Reader {
    stream: Box<dyn std::io::Read>,
    buf: BytesMut,
    deframer: Deframer,
}

impl Reader {
    fn new(stream: Box<dyn std::io::Read>) -> Self {
        Self {
            stream,
            buf: BytesMut::new(),
            deframer: Deframer::builder().build(),
        }
    }

    fn read_frame<T: Message + Default>(&mut self) -> Result<Option<T>> {
        let mut chunk = [0u8; 8192];
        loop {
            if let Some(record) = self.deframer.feed(&mut self.buf)? {
                return T::decode(&record[..]).map(Some).map_err(Into::into);
            }
            let n = self.stream.read(&mut chunk)?;
            if n == 0 {
                return if self.deframer.is_idle() {
                    Ok(None)
                } else {
                    Err(Framing::TruncatedFrame)
                };
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }
}

fn write_frame<T: Message>(mut stream: impl std::io::Write, frame: &T) -> Result<()> {
    stream.write_all(&frame.encode_length_delimited_to_vec())?;
    stream.flush().map_err(Into::into)
}

struct Thread {
    id: MsgRef,
    log: Vec<zconned_stdio_types::Entry>,
}

struct FakeCore {
    threads: Vec<Thread>,
    cursor: Vec<MsgRef>,
    content: Option<Value>,
    claims: Vec<Value>,
    resources: HashMap<Vec<u8>, Value>,
    entropy: u64,
}

impl FakeCore {
    fn new() -> Self {
        Self {
            threads: Vec::new(),
            cursor: Vec::new(),
            content: None,
            claims: Vec::new(),
            resources: HashMap::new(),
            entropy: 0,
        }
    }

    fn now_ts(&self) -> pbjson_types::Timestamp {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        pbjson_types::Timestamp {
            seconds: secs as i64,
            nanos: 0,
        }
    }

    fn now_text(&self) -> String {
        // The ts claim value: an RFC 3339 instant (the real core stamps
        // its own; the fake mirrors the shape only).
        jiff::Timestamp::now().to_string()
    }

    fn fresh_ref(&mut self) -> MsgRef {
        self.entropy += 1;
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64
            ^ (self.entropy << 32);
        let mut state = seed | 1;
        let digest: Vec<u8> = (0..32)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        MsgRef {
            ver_major: 1,
            ver_minor: 0,
            algo: "sha3-256".into(),
            digest,
        }
    }

    fn resource(&mut self, value: &Value) -> ResourceRef {
        let digest = resource_digest(&value.kind, &value.data).to_vec();
        self.resources.insert(digest.clone(), value.clone());
        ResourceRef {
            algo: "sha3-256".into(),
            digest,
        }
    }

    fn draft_push(&self) -> Vec<OutgoingFrame> {
        vec![OutgoingFrame {
            frame: Some(Out::MsgDraftPush(zconned_stdio_types::MsgDraftPush {
                content: self.content.clone(),
                claims: self.claims.clone(),
            })),
        }]
    }

    fn error(&self, code: &str, message: &str) -> Vec<OutgoingFrame> {
        vec![OutgoingFrame {
            frame: Some(Out::Error(zconned_stdio_types::Error {
                code: code.into(),
                message: message.into(),
            })),
        }]
    }

    fn handle(&mut self, f: IncomingFrame) -> Vec<OutgoingFrame> {
        use zconned_stdio_types::{MsgCursorSet, MsgDraftClaimsAdd, MsgDraftContentSet};
        let Some(m) = f.method else {
            return Vec::new(); // all-default frame: acked, no death
        };
        match m {
            Method::MsgCursorSet(MsgCursorSet { at, fresh }) => {
                if fresh.is_none() && at.is_empty() {
                    return self.error("cursor_required", "neither at nor fresh set");
                }
                self.cursor = match fresh {
                    Some(_) => {
                        let id = self.fresh_ref();
                        self.threads.push(Thread {
                            id: id.clone(),
                            log: Vec::new(),
                        });
                        vec![id]
                    }
                    None => at,
                };
                vec![OutgoingFrame {
                    frame: Some(Out::MsgCursorPush(zconned_stdio_types::MsgCursorPush {
                        refs: self.cursor.clone(),
                    })),
                }]
            }
            Method::MsgDraftContentSet(MsgDraftContentSet { kind, data }) => {
                if kind.is_empty() {
                    return self.error("kind_required", "content kind must not be empty");
                }
                self.content = Some(Value { kind, data });
                self.draft_push()
            }
            Method::MsgDraftContentReset(_) => {
                self.content = None;
                self.draft_push()
            }
            Method::MsgDraftClaimsAdd(MsgDraftClaimsAdd { kind, data }) => {
                if kind.is_empty() {
                    return self.error("kind_required", "claim kind must not be empty");
                }
                // Registry kinds (vnd.zconn.*) are core-stamped: the
                // draft carries them with empty data until commit.
                let stamped = kind.starts_with("vnd.zconn.");
                self.claims.push(Value {
                    kind,
                    data: if stamped { Vec::new() } else { data },
                });
                self.draft_push()
            }
            Method::MsgDraftClaimsRemove(s) => {
                self.claims.retain(|c| c.kind != s.kind);
                self.draft_push()
            }
            Method::MsgDraftClaimsClear(_) => {
                self.claims.clear();
                self.draft_push()
            }
            Method::MsgDraftGet(_) => self.draft_push(),
            Method::MsgDraftCommit(_) => self.commit(),
            Method::MsgThreadsGet(_) => vec![OutgoingFrame {
                frame: Some(Out::MsgThreadsPush(zconned_stdio_types::MsgThreadsPush {
                    threads: self
                        .threads
                        .iter()
                        .map(|t| zconned_stdio_types::ThreadEntry {
                            thread: Some(t.id.clone()),
                        })
                        .collect(),
                })),
            }],
            Method::MsgThreadsLogGet(s) => {
                let entries = self
                    .threads
                    .iter()
                    .find(|t| Some(&t.id) == s.thread.as_ref())
                    .map(|t| t.log.clone())
                    .unwrap_or_default();
                vec![OutgoingFrame {
                    frame: Some(Out::MsgThreadsLogPush(
                        zconned_stdio_types::MsgThreadsLogPush { entries },
                    )),
                }]
            }
            Method::ResourceGet(s) => {
                let value = s
                    .r#ref
                    .as_ref()
                    .and_then(|r| self.resources.get(&r.digest).cloned());
                vec![OutgoingFrame {
                    frame: Some(Out::ResourcePush(zconned_stdio_types::ResourcePush {
                        r#ref: s.r#ref,
                        value,
                    })),
                }]
            }
        }
    }

    fn commit(&mut self) -> Vec<OutgoingFrame> {
        use zconned_stdio_types::{Entry, ResolvedClaim, ResolvedSlot, RowMeta};
        let Some(content) = self.content.clone() else {
            return self.error("content_required", "no content staged");
        };
        if self.cursor.is_empty() {
            return self.error("cursor_required", "commit before msg_cursor_set");
        }
        let content_ref = self.resource(&content);

        // Registry claims are stamped here (ts = the commit instant).
        let stamped: Vec<Value> = self
            .claims
            .iter()
            .map(|c| {
                if c.kind == CLAIM_KIND_TS {
                    Value {
                        kind: c.kind.clone(),
                        data: self.now_text().into_bytes(),
                    }
                } else {
                    c.clone()
                }
            })
            .collect();
        let claims: Vec<ResolvedClaim> = stamped
            .iter()
            .map(|s| ResolvedClaim {
                kind: s.kind.clone(),
                r#ref: Some(self.resource(s)),
                value: Some(s.clone()),
            })
            .collect();

        let entry = Entry {
            ver_major: 1,
            ver_minor: 0,
            prev: self.cursor.clone(),
            meta: Some(RowMeta {
                ts: Some(self.now_ts()),
                algo: "sha3-256".into(),
                // Placeholder recipe — the fake is not the digest
                // machine; the shape is what matters for the shell.
                digest: resource_digest("application/vnd.zconn.msg", &content_ref.digest).to_vec(),
            }),
            content: Some(ResolvedSlot {
                r#ref: Some(content_ref),
                value: Some(content),
            }),
            claims,
        };

        // The committed message's ref = its own digest.
        let tip = MsgRef {
            ver_major: 1,
            ver_minor: 0,
            algo: "sha3-256".into(),
            digest: entry.meta.as_ref().unwrap().digest.clone(),
        };
        if let Some(t) = self.threads.last_mut() {
            t.log.push(entry.clone());
        }
        self.cursor = vec![tip];
        self.content = None;
        self.claims.clear();

        let mut out = vec![OutgoingFrame {
            frame: Some(Out::MsgAdded(zconned_stdio_types::MsgAdded {
                entry: Some(entry),
            })),
        }];
        out.extend(self.draft_push());
        out
    }
}

fn serve(mut reader: Reader, mut stdout: impl std::io::Write) -> Result<()> {
    let Handshake { version: theirs } = reader
        .read_frame::<Handshake>()?
        .ok_or(Framing::TruncatedFrame)?; // eof before handshake
    if theirs != PROTOCOL_VERSION {
        eprintln!(
            "handshake version mismatch: core speaks {PROTOCOL_VERSION}, client spoke {theirs}"
        );
        return Ok(());
    }
    write_frame(
        &mut stdout,
        &Handshake {
            version: PROTOCOL_VERSION,
        },
    )?;
    log::info!("handshake ok (version {PROTOCOL_VERSION})");

    let mut core = FakeCore::new();
    while let Some(f) = reader.read_frame::<IncomingFrame>()? {
        for out in core.handle(f) {
            write_frame(&mut stdout, &out)?;
        }
    }
    log::info!("client disconnected (eof)");
    Ok(())
}

fn main() -> ExitCode {
    let args = Args::parse();
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or(args.verbose.log_level_filter().to_string()),
    )
    .init();

    let reader = Reader::new(Box::new(std::io::stdin()));
    match serve(reader, std::io::stdout()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("fake-core: {e}");
            ExitCode::FAILURE
        }
    }
}
