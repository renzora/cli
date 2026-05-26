# renzora

CLI for the [Renzora game engine](https://github.com/renzora/engine). Scaffolds
projects and drives the engine's pinned, containerized toolchain so every
build/test runs in one controlled environment.

## Install

```sh
cargo install renzora
```

Requires [Docker](https://docs.docker.com/get-docker/) (the toolchain runs in a
container) and `git` (for `renzora new`).

## Usage

```sh
renzora new my-game     # clone the engine from GitHub into ./my-game
cd my-game
renzora init            # build the toolchain image + container (first run is slow)
renzora run             # build the editor and launch it
```

| Command | What it does |
|---|---|
| `renzora new <dir>` | Clone the engine into a new directory |
| `renzora init` | Build the image + create/start the container |
| `renzora build [platforms]` | Cross-build (no args = all) |
| `renzora run [editor\|runtime]` | Build for this host, then run it |
| `renzora test [args]` | Run the test suite in the container |
| `renzora check [args]` | `cargo check` in the container |
| `renzora add <name> [--editor\|--dylib]` | Scaffold a plugin crate |
| `renzora remove <name>` | Delete a plugin crate |
| `renzora upx [platforms]` | UPX-compress built binaries |
| `renzora shell` | Interactive shell in the container |
| `renzora clean` / `destroy` | Clear `target/` / remove the container |

## License

MIT OR Apache-2.0.
