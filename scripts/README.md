Scripts
=======

Flamegraph
----------

Run the release version of Alacritty while recording call stacks (Linux only).
This script requires `perf`; it will install `cargo-flamegraph` automatically
if needed.

```sh
./scripts/create-flamegraph.sh
```

Arguments are forwarded to Alacritty:

```sh
./scripts/create-flamegraph.sh --config-file /path/to/alacritty.toml
```

If `cargo-flamegraph` was installed by the script, you'll be prompted to
optionally uninstall it afterwards.

ANSI Color Tests
----------------

We include a few scripts for testing the color of text inside a terminal. The
first shows various foreground and background variants. The second enumerates
all the colors of a standard terminal. The third enumerates the 24-bit colors.

```sh
./scripts/fg-bg.sh
./scripts/colors.sh
./scripts/24-bit-color.sh
```
