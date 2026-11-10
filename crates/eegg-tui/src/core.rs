//! The client mechanism: spawn the core child, handshake v2, framed
//! IO on the protocol pipes (the sync deframer state machine — no
//! async runtime), and the EOF teardown. The core-stdin writer is
//! exactly one thread (the main loop).

use std::{
    io::{Read, Write as _},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};

use bytes::BytesMut;
use miette::Diagnostic as _Diagnostic;
use prost::Message;
use protobuf_varint_deframer::Deframer;
use thiserror::Error as ThisError;
use zconned_stdio_types::{Handshake, PROTOCOL_VERSION};

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, ThisError, _Diagnostic)]
#[non_exhaustive]
pub enum Error {
    #[error("failed to spawn core `{cmd}`: {source}")]
    Spawn { cmd: String, source: std::io::Error },

    #[error("core closed the pipe before the handshake")]
    HandshakeEof,

    #[error("handshake version mismatch: ours {ours}, theirs {theirs}")]
    HandshakeVersion { ours: u32, theirs: u32 },

    #[error("framing error: {0}")]
    Framing(#[from] protobuf_varint_deframer::Error),

    #[error("eof truncates frame")]
    TruncatedFrame,

    #[error("failed to decode {name}: {source}")]
    Decode {
        name: &'static str,
        source: prost::DecodeError,
    },

    #[error("failed to write to core stdin: {source}")]
    Write { source: std::io::Error },

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// The child process plus the one writable pipe (core stdin).
#[derive(Debug)]
pub struct Core {
    child: Child,
    stdin: ChildStdin,
}

/// The readable pipe (core stdout) with its framing state. Owned by
/// the reader thread.
#[derive(Debug)]
pub struct CoreStdout {
    stream: ChildStdout,
    buf: BytesMut,
    deframer: Deframer,
}

impl Core {
    /// Spawn the core child (piped stdio). stderr is piped too — the
    /// full-screen UI must never share the screen with core logs.
    pub fn spawn(cmdline: &str) -> Result<(Core, ChildStdout, ChildStderrStream)> {
        let parts = shlex::split(cmdline)
            .filter(|p| !p.is_empty())
            .ok_or_else(|| Error::Spawn {
                cmd: cmdline.into(),
                source: std::io::Error::other("empty core command"),
            })?;
        let mut child = Command::new(&parts[0])
            .args(&parts[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| Error::Spawn {
                cmd: cmdline.into(),
                source,
            })?;
        let stdin = child.stdin.take().expect("stdin was piped");
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");
        Ok((Self { child, stdin }, stdout, ChildStderrStream(stderr)))
    }

    /// Handshake v2: we speak first; the core answers with its version.
    pub fn handshake(&mut self, stdout: &mut CoreStdout) -> Result<()> {
        self.write_frame(&Handshake {
            version: PROTOCOL_VERSION,
        })?;
        match stdout.read_frame::<Handshake>("Handshake")? {
            Some(h) if h.version == PROTOCOL_VERSION => {
                log::info!("core handshake ok (version {PROTOCOL_VERSION})");
                Ok(())
            }
            Some(h) => Err(Error::HandshakeVersion {
                ours: PROTOCOL_VERSION,
                theirs: h.version,
            }),
            None => Err(Error::HandshakeEof),
        }
    }

    pub fn write_frame<T: Message>(&mut self, frame: &T) -> Result<()> {
        self.stdin
            .write_all(&frame.encode_length_delimited_to_vec())
            .and_then(|()| self.stdin.flush())
            .map_err(|source| Error::Write { source })
    }

    /// The ONLY legal shutdown: client EOF closes core stdin, then the
    /// reap. `wait()` is strictly after the stdin drop.
    pub fn shutdown(mut self) -> std::io::Result<std::process::ExitStatus> {
        log::info!("core stdin dropped (EOF to core)");
        drop(self.stdin);
        self.child.wait()
    }
}

impl CoreStdout {
    pub fn new(stream: ChildStdout) -> Self {
        Self {
            stream,
            buf: BytesMut::new(),
            deframer: Deframer::builder().build(),
        }
    }

    /// Read one framed message; `None` = clean EOF on an idle deframer
    /// (the core closed the pipe — contract exit or death).
    pub fn read_frame<T: Message + Default>(&mut self, name: &'static str) -> Result<Option<T>> {
        let mut chunk = [0u8; 8192];
        loop {
            if let Some(record) = self.deframer.feed(&mut self.buf)? {
                return T::decode(&record[..])
                    .map(Some)
                    .map_err(|source| Error::Decode { name, source });
            }
            let n = self.stream.read(&mut chunk)?;
            if n == 0 {
                return if self.deframer.is_idle() {
                    Ok(None)
                } else {
                    Err(Error::TruncatedFrame)
                };
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }
}

/// The core's stderr pipe — never rendered into the full-screen UI;
/// buffered and surfaced only on the death report.
#[derive(Debug)]
pub struct ChildStderrStream(std::process::ChildStderr);

impl Read for ChildStderrStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}
