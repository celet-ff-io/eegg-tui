//! eegg — the zconn terminal interface (LPZ107).
//!
//! Spawn the core (`zconned`) as its own dedicated child process over
//! stdio protobuf (the nvim/LSP-client pattern); runtime topology is
//! strictly 1:1; client EOF is the only legal shutdown.
//!
//! M0 milestone: UI-complete shell — real i2c client mechanism against
//! fake cores (S2), features attach when LPZ106 completes.

fn main() -> miette::Result<()> {
    // TODO(M0): cli parse (clap, English-only help), spawn core,
    // handshake v2, event loop (C2), UI (ratatui). Scaffold stub.
    Ok(())
}
