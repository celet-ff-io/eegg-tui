# eegg — the zconn terminal interface

A TUI for zconn: it spawns its own core (`zconned`) as a dedicated
child process and speaks protobuf over its stdio. Quitting tears the
core down.

## Licensing

eegg is `MIT OR Apache-2.0` (see `LICENSE-MIT` / `LICENSE-APACHE`);
`eegg --license` prints the third-party license bundle to stdout.

The bundle (`third-party-licenses.txt`, committed) is rendered by
`cargo about` from the template `about.txt.hbs` over the dependency
graph — regenerate it whenever the dependency set changes:

```
cargo about generate about.txt.hbs -o third-party-licenses.txt
```

`about.hbs` is the HTML variant of the same tooling (not embedded;
kept for the web-facing sibling that shares the templates).
