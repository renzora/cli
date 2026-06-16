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
renzora init            # pull the host toolchain image + container (first run is slow)
renzora run             # build the editor and launch it
```

The toolchain is split into a shared `base` image plus one image per
platform (`linux`, `windows`, `macos`, `ios`,
`android`, `wasm`), so each command pulls only what it needs:
`renzora run` pulls the host platform image, `renzora build` (no args) pulls all,
`renzora build windows` pulls only Windows. Stale images are pruned on update.

| Command | What it does |
|---|---|
| `renzora new <dir>` | Clone the engine into a new directory |
| `renzora init` | Pull/build the host toolchain image + create/start its container |
| `renzora build [platforms]` | Cross-build (no args = all platforms) |
| `renzora run [editor\|runtime]` | Build for this host, then run it |
| `renzora test [args]` | Run the test suite in the linux container |
| `renzora check [args]` | `cargo check` in the linux container |
| `renzora add <name> [--editor\|--dylib]` | Scaffold a plugin crate |
| `renzora remove <name>` | Delete a plugin crate |
| `renzora upx [platforms]` | UPX-compress built binaries |
| `renzora shell` | Interactive shell in the linux container |
| `renzora clean` | Clear `target/` |
| `renzora destroy` | Remove this checkout's containers + cache volumes |
| `renzora prune` | Remove this checkout's stale toolchain images |

## License

MIT OR Apache-2.0.
