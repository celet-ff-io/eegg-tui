# eegg — the zconn terminal interface

A TUI for zconn: it spawns its own core (`zconned`) as a dedicated
child process and speaks protobuf over its stdio. Quitting tears the
core down.

## Licensing

eegg is `MIT OR Apache-2.0` (see `LICENSE-MIT` / `LICENSE-APACHE`);
`eegg --license` prints the third-party license page to stdout —
redirect it to a file and open it in a browser:

```
eegg --license > license.html
```

The committed page (`license.html`) is rendered by `cargo about`
from the template `about.hbs` over the dependency graph — regenerate
it whenever the dependency set changes:

```
cargo about generate about.hbs -o license.html
```
