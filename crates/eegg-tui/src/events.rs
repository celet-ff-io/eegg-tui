//! Event plumbing: producer threads feed one channel; the main loop
//! is the sole consumer (and the sole core-stdin writer).

use std::thread::JoinHandle;

use crossbeam_channel::Sender;
use zconned_stdio_types::OutgoingFrame;

use crate::core::{ChildStderrStream, CoreStdout, Error as CoreError};

/// Everything the UI reacts to, from three producer threads.
#[derive(Debug)]
pub enum Evt {
    Key(ratatui::crossterm::event::KeyEvent),
    /// Each draw queries the real size; the variant only wakes the loop.
    Resize,
    /// A delivery/answer frame from the core (arrives ANYTIME).
    Frame(OutgoingFrame),
    /// One line of the core's stderr (buffered for the death report).
    CoreStderr(String),
    /// The core's stdout pipe closed: `None` = clean EOF, `Some` = a
    /// broken stream (framing/decode failure).
    CoreGone(Option<CoreError>),
}

/// Terminal input → events. Ends when the channel closes (main exit).
pub fn spawn_input(tx: Sender<Evt>) -> JoinHandle<()> {
    use ratatui::crossterm::event::{Event, read};
    std::thread::Builder::new()
        .name("input".into())
        .spawn(move || {
            loop {
                match read() {
                    Ok(Event::Key(k)) => {
                        if tx.send(Evt::Key(k)).is_err() {
                            break;
                        }
                    }
                    Ok(Event::Resize(..)) => {
                        if tx.send(Evt::Resize).is_err() {
                            break;
                        }
                    }
                    Ok(_) => {}      // mouse / focus / paste — ignored for now
                    Err(_) => break, // terminal gone — nothing left to read
                }
            }
        })
        .expect("thread spawn cannot fail at startup")
}

/// Core stdout → frames. Ends on EOF or a broken stream.
pub fn spawn_reader(stdout: CoreStdout, tx: Sender<Evt>) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("reader".into())
        .spawn(move || {
            let mut stdout = stdout;
            loop {
                match stdout.read_frame::<OutgoingFrame>("OutgoingFrame") {
                    Ok(Some(frame)) => {
                        if tx.send(Evt::Frame(frame)).is_err() {
                            break;
                        }
                    }
                    Ok(None) => {
                        let _ = tx.send(Evt::CoreGone(None));
                        break;
                    }
                    Err(e) => {
                        let _ = tx.send(Evt::CoreGone(Some(e)));
                        break;
                    }
                }
            }
        })
        .expect("thread spawn cannot fail at startup")
}

/// Core stderr → buffered lines. Never touches the UI surface.
pub fn spawn_stderr_drain(stderr: ChildStderrStream, tx: Sender<Evt>) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("stderr".into())
        .spawn(move || {
            use std::io::BufRead as _;
            let mut stderr = std::io::BufReader::new(stderr);
            let mut line = Vec::new();
            loop {
                line.clear();
                match stderr.read_until(b'\n', &mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        if tx
                            .send(Evt::CoreStderr(String::from_utf8_lossy(&line).into_owned()))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        })
        .expect("thread spawn cannot fail at startup")
}
