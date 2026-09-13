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
| `renzora login` | Store a marketplace API token |
| `renzora logout` | Forget the stored token |
| `renzora whoami` | Print who the stored token belongs to |
| `renzora publish [dir]` | Publish a plugin or asset to the marketplace |

## Publishing to the marketplace

Mint an API token at [renzora.com/developers](https://renzora.com/developers),
hand it to the CLI once, then publish from the plugin's own directory:

```sh
renzora login          # paste the rz_... token; stored in ~/.renzora/credentials.toml
renzora publish        # from the plugin directory (or: renzora publish path/to/plugin)
```

The directory describes itself in its `Cargo.toml`, in a section cargo ignores:

```toml
[package]
name = "terrain_sculpt"
version = "0.3.0"
description = "A terrain sculpting brush for the editor"
license = "MIT"

[package.metadata.renzora]
marketplace_id = "terrain_sculpt"   # required; your claim on the registry
category = "plugins"                # `renzora publish --list-categories`
tags = ["terrain", "editor"]
price_credits = 0                   # 0 is free
min_engine_version = "r1-alpha7"    # oldest engine this works on
thumbnail = "assets/thumb.png"      # `thumbnail.png` is found without this
screenshots = ["assets/sculpt.png"]
exclude = ["tests", "*.blend1"]
```

`name`, `version` and `description` come from `[package]`, so the version you
bump to release is the version that ships. A directory with no crate — a model
pack, a material library — puts the same keys in a `renzora.toml` beside its
files, with its own `name`, `version` and `description` at the top.

### The marketplace id

`marketplace_id` is the listing's identity: unique across the whole
marketplace, claimed on your first publish, and **never changed**. It is the
only thing a publish matches on, and there are exactly three answers — the id
is free, so a listing is created; it is yours, so this version is added to it;
or it belongs to somebody else, and the publish stops.

Matching on anything softer would guess. A listing is titled by a person and
retitled later — `crt` is listed as "CRT Fx" — so a title says nothing reliable
about which listing a directory belongs to.

### What a publish sends

Packaging skips `target/`, `build/`, `.git/`, `plugin.toml` and anything in
`exclude`. A plugin is published as buildable source, so its zip has the
crate's `Cargo.toml` at the root — the editor extracts it into
`plugins/<crate>/` and the SDK compiles it there.

**A published version is never replaced.** Shipping 0.3.0 leaves 0.2.0
downloadable for everyone who already has it, and publishing a version that
already exists, or one behind the current release, is refused — pass
`--allow-older` for the second if you mean it. Release notes come from
`--notes`, `--notes-file`, or this version's section of `CHANGELOG.md`.

Creating a listing sends everything, defaults included. Updating one sends only
what the manifest actually **states**: an absent `price_credits` is not an
instruction to make an asset free, and a directory name is not an instruction
to retitle a listing somebody named on the website.

| Flag | |
|---|---|
| `--all` | Publish every directory one level down — a whole `plugins/` folder |
| `--dry-run` | Do everything except the upload (see below) |
| `--allow-older` | Publish behind the listing's current version |
| `--out <path>` | Also write the zip out, to inspect what would ship |
| `--list-categories` | Print the marketplace's categories |
| `-y`, `--yes` | Don't ask before publishing |

### Dry runs

`renzora publish --dry-run` makes every check the real thing makes and then
stops, so failures that would otherwise surface *after* a large upload surface
in a second instead:

```
$ renzora publish --dry-run
   Packaging terrain_sculpt v0.2.0 (/home/me/terrain_sculpt)
    Packaged 41 files, 96.2 KiB (284.1 KiB uncompressed)
             Cargo.toml
             src/lib.rs
             ...
       Would publish v0.2.0 to "Terrain Sculpt", which is at v0.1.0
       Would take the description, tags, licence and price from Cargo.toml
       Would leave v0.1.0 downloadable
       Would require engine r1-alpha7 or newer
       Notes - Added a smoothing mode (+3 more lines)
             https://renzora.com/marketplace/asset/terrain-sculpt-1a2b3c4d
     Dry run nothing was uploaded
```

It exits non-zero on anything that would have failed the publish — an unknown
category, a version already published, an id somebody else holds. With no token
stored, or no route to the marketplace, it checks the package and reports which
checks it had to skip rather than failing.

Across a batch it prints one line each and a summary, and refuses the whole run
if any directory is unfit — a broken manifest in the sixtieth plugin should not
be discovered after fifty-nine listings already exist:

```
$ renzora publish plugins --all --dry-run
   Preparing 71 directories
    Packaged 71 directories, 3.6 MiB
             clouds      release v1.0.1 onto "Clouds Plugin" (at v1.0.0)
             ascii       new listing at v0.1.0, claiming `ascii`
             ...
       Would create 58 listings, update 13, and refuse 0
```

## License

MIT OR Apache-2.0.
