//! `renzora` — the Renzora engine CLI.
//!
//! Scaffolds projects (`renzora new`) and drives the pinned `ghcr.io/renzora/*`
//! toolchain containers for everything else, so builds/tests run in one
//! controlled toolchain (the ABI contract the dlopen plugin system depends on).
//! Install with `cargo install renzora`; it finds the engine checkout by walking
//! up from the current directory.
//!
//! ## Split toolchain images
//!
//! The toolchain is one shared BASE image (`base`: rust + Linux deps +
//! LLVM-19) plus one image per platform that builds `FROM` it (`linux`,
//! `windows`, `macos`, `ios`, `android`,
//! `wasm`). So `renzora run` pulls only the host platform image,
//! `renzora build windows` pulls only Windows, and a toolchain change to one
//! platform never re-downloads the others.
//!
//! Tags are content hashes computed identically here and in CI:
//!   baseTag   = sha256(docker/base/Dockerfile)[:12]
//!   <plat>Tag = sha256(baseTag + docker/<plat>/Dockerfile)[:12]
//! Folding baseTag into every platform tag makes a base change cascade (every
//! platform tag re-rolls → we re-pull); a platform-only edit moves just its tag.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

use clap::{Parser, Subcommand};

/// GHCR image prefix; each image is `<IMAGE>/<platform>` (+ `/base`).
const IMAGE: &str = "ghcr.io/renzora";
/// Repo-root sentinel + the file whose hash is the base tag.
const BASE_DOCKERFILE: &str = "docker/base/Dockerfile";
const ENGINE_REPO: &str = "https://github.com/renzora/engine";
/// Every platform image (each `FROM base`). `renzora build` with no args
/// builds all of these; each name is also a valid `build-all.sh` platform arg.
const ALL_PLATFORMS: &[&str] = &["linux", "windows", "macos", "ios", "android", "wasm"];
// Vendored-crate excludes — mirror .github/workflows/test.yml so local and CI agree.
const EXCLUDES: &[&str] = &[
    "--exclude", "renzora_shader",
    "--exclude", "bevy_gauge",
    "--exclude", "bevy_hanabi",
    "--exclude", "bevy_mod_outline",
    "--exclude", "bevy_silk",
    "--exclude", "vleue_navigator",
    "--exclude", "bevy_mod_openxr",
    "--exclude", "bevy_mod_xr",
    "--exclude", "bevy_xr_utils",
];

#[derive(Parser)]
#[command(name = "renzora", about = "Renzora engine CLI — scaffold projects and run everything in the pinned containers.", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Create a new project by cloning the engine from GitHub.
    New {
        /// Directory to create.
        name: String,
    },
    /// Pull/build the host toolchain image + create/start its container.
    Init,
    /// Cross-build for one or more platforms (no args = all platforms).
    Build { platforms: Vec<String> },
    /// Run the test suite in the linux container (no args = workspace suite).
    Test {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// `cargo check` in the linux container.
    Check {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Build for this host, then run it (editor default).
    Run { target: Option<String> },
    /// Scaffold a new plugin crate.
    Add {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Delete a plugin crate.
    Remove { args: Vec<String> },
    /// UPX-compress built binaries under dist/.
    Upx { args: Vec<String> },
    /// Interactive shell in the linux container.
    Shell,
    /// Remove target/ in the linux container.
    Clean,
    /// Remove this checkout's containers + build-cache volumes.
    Destroy,
    /// Remove this checkout's stale (non-current) toolchain images.
    Prune,
}

fn main() {
    let cli = Cli::parse();

    check_for_update();

    // `new` runs from anywhere — there's no engine checkout yet.
    if let Commands::New { name } = &cli.command {
        new_project(name);
        return;
    }

    // Everything else operates on an existing engine checkout.
    let root = find_repo_root();

    match cli.command {
        Commands::New { .. } => unreachable!("handled above"),
        Commands::Init => {
            let host = host_platform();
            ensure_up(&root, host.image);
            println!("Container {} is running.", container_name(&root, host.image));
        }
        Commands::Build { platforms } => build_cmd(&root, platforms),
        Commands::Test { args } => {
            let name = linux_container(&root);
            if args.is_empty() {
                dexec(&name, &format!("cargo test --workspace {}", EXCLUDES.join(" ")));
            } else {
                dexec(&name, &format!("cargo test {}", args.join(" ")));
            }
        }
        Commands::Check { args } => {
            let name = linux_container(&root);
            if args.is_empty() {
                dexec(&name, &format!("cargo check --workspace {}", EXCLUDES.join(" ")));
            } else {
                dexec(&name, &format!("cargo check {}", args.join(" ")));
            }
        }
        Commands::Run { target } => run(&root, target),
        Commands::Add { args } => {
            let name = linux_container(&root);
            dexec(&name, &format!("bash docker/add-plugin.sh {}", args.join(" ")));
        }
        Commands::Remove { args } => {
            let name = linux_container(&root);
            dexec(&name, &format!("bash docker/remove-plugin.sh {}", args.join(" ")));
        }
        Commands::Upx { args } => {
            let name = linux_container(&root);
            dexec(&name, &format!("bash docker/upx-compress.sh {}", args.join(" ")));
        }
        Commands::Shell => {
            let name = linux_container(&root);
            let st = Command::new("docker")
                .args(["exec", "-it", &name, "bash"])
                .status()
                .unwrap_or_else(|e| fail(format!("docker exec failed: {e}")));
            std::process::exit(st.code().unwrap_or(0));
        }
        Commands::Clean => {
            let name = linux_container(&root);
            // `target/` is a volume mountpoint, so clear its contents rather
            // than removing the directory itself.
            dexec(&name, "find target -mindepth 1 -maxdepth 1 -exec rm -rf {} + && echo 'target/ cleaned'");
        }
        Commands::Destroy => destroy(&root),
        Commands::Prune => prune(&root),
    }
}

/// `renzora new <name>` — shallow-clone the engine into a new directory.
fn new_project(name: &str) {
    if Path::new(name).exists() {
        fail(format!("`{name}` already exists"));
    }
    println!("Cloning {ENGINE_REPO} into {name} ...");
    let st = Command::new("git")
        .args(["clone", "--depth", "1", ENGINE_REPO, name])
        .status()
        .unwrap_or_else(|e| fail(format!("git clone failed (is git installed?): {e}")));
    if !st.success() {
        std::process::exit(st.code().unwrap_or(1));
    }
    println!("\nDone. Next:");
    println!("  cd {name}");
    println!("  renzora init     # pull the host toolchain image + container (first run is slow)");
    println!("  renzora run      # build the editor and launch it");
}

/// Walk up from the current dir to the engine repo root (the dir holding the
/// base builder Dockerfile).
fn find_repo_root() -> PathBuf {
    let mut dir = std::env::current_dir().unwrap_or_else(|e| fail(format!("cannot read cwd: {e}")));
    loop {
        if dir.join(BASE_DOCKERFILE).exists() {
            return dir;
        }
        match dir.parent() {
            Some(p) => dir = p.to_path_buf(),
            None => fail(format!(
                "not inside a Renzora engine checkout (no {BASE_DOCKERFILE} found). Run `renzora new <name>` first."
            )),
        }
    }
}

/// Per-checkout, per-platform container name (stable per clone + platform).
fn container_name(root: &Path, plat: &str) -> String {
    let mut h = DefaultHasher::new();
    root.to_string_lossy().hash(&mut h);
    format!("renzora-{:08x}-{plat}", h.finish() as u32)
}

/// Name of the per-container named volume that backs `/app/src/target`.
fn target_volume(name: &str) -> String {
    format!("{name}-target")
}

// ── Tag / image refs ─────────────────────────────────────────────────────────

/// First 12 hex chars of SHA-256 over the concatenated parts.
fn hash12(parts: &[&[u8]]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().iter().take(6).map(|b| format!("{b:02x}")).collect()
}

/// Read a file with `\r` stripped, so the hash is identical on every platform
/// (Windows checkouts may have CRLF) and matches CI's `tr -d '\r' | sha256sum`.
fn read_crlf_stripped(path: PathBuf) -> Option<Vec<u8>> {
    let bytes = std::fs::read(path).ok()?;
    Some(bytes.into_iter().filter(|&b| b != b'\r').collect())
}

/// baseTag = sha256(docker/base/Dockerfile)[:12].
fn base_tag(root: &Path) -> Option<String> {
    let base = read_crlf_stripped(root.join(BASE_DOCKERFILE))?;
    Some(hash12(&[&base]))
}

/// <plat>Tag = sha256(baseTag + docker/<plat>/Dockerfile)[:12] — the exact
/// concatenation CI hashes (`printf '%s' "$baseTag"; tr -d '\r' < Dockerfile`).
fn platform_tag(root: &Path, plat: &str) -> Option<String> {
    let bt = base_tag(root)?;
    let pf = read_crlf_stripped(root.join(format!("docker/{plat}/Dockerfile")))?;
    Some(hash12(&[bt.as_bytes(), &pf]))
}

/// `ghcr.io/renzora/<plat>:<tag>` (falls back to `:latest` if unreadable).
fn image_ref(root: &Path, plat: &str) -> String {
    match platform_tag(root, plat) {
        Some(t) => format!("{IMAGE}/{plat}:{t}"),
        None => format!("{IMAGE}/{plat}:latest"),
    }
}

fn base_image_ref(root: &Path) -> String {
    match base_tag(root) {
        Some(t) => format!("{IMAGE}/base:{t}"),
        None => format!("{IMAGE}/base:latest"),
    }
}

/// Map a `renzora build` token (platform name or specific slice) to its image.
fn image_for_token(tok: &str) -> Option<&'static str> {
    match tok {
        "linux" | "linux-x64" | "linux-arm64" => Some("linux"),
        "windows" | "windows-x64" => Some("windows"),
        "macos" | "macos-x64" | "macos-arm64" => Some("macos"),
        "ios" => Some("ios"),
        "android" | "android-arm64" | "android-x86" => Some("android"),
        "wasm" => Some("wasm"),
        _ => None,
    }
}

// ── Image / container lifecycle ──────────────────────────────────────────────

fn image_missing(image: &str) -> bool {
    docker_out(&["images", "-q", image]).trim().is_empty()
}

/// Ensure the shared base image is present (needed only for the local-build
/// fallback; pulling a platform image already carries its base layers).
fn ensure_base(root: &Path) {
    let base = base_image_ref(root);
    if image_missing(&base) {
        eprintln!("Fetching base image {base} ...");
        if !pull_image(&base) {
            eprintln!("Could not pull {base} — building it locally (this takes a while)...");
            build_image(root, BASE_DOCKERFILE, &base, None);
        }
    }
    // Best-effort: drop superseded base tags (non-forced, so a tag still
    // referenced by another checkout's platform image fails harmlessly).
    remove_other_tags(&format!("{IMAGE}/base"), &base);
}

/// Pull/build a platform image if missing, create/recreate its container, start
/// it. Reuses the container across commands (no per-invocation recreate) unless
/// the image tag changed.
fn ensure_up(root: &Path, plat: &str) {
    let image = image_ref(root, plat);

    if image_missing(&image) {
        // A pull beats a 10-25 min local build of osxcross/NDK/xwin/etc.
        eprintln!("Fetching {plat} toolchain image {image} ...");
        if !pull_image(&image) {
            // Local build needs the base image present (FROM base:<tag>).
            ensure_base(root);
            eprintln!("Could not pull {image} — building it locally (this takes a while)...");
            let bt = base_tag(root).unwrap_or_else(|| "latest".into());
            build_image(root, &format!("docker/{plat}/Dockerfile"), &image, Some(&bt));
        }
    }

    ensure_container(root, plat, &image);

    // Stale-image cleanup: drop other tags of this platform repo so updates
    // don't pile up. Non-forced, so the in-use current tag is never touched.
    remove_other_tags(&format!("{IMAGE}/{plat}"), &image);
}

/// Create the per-platform container if missing; recreate it (and remove the
/// superseded image) if it was created from a different image tag.
fn ensure_container(root: &Path, plat: &str, image: &str) {
    let name = container_name(root, plat);
    let by_name = format!("name=^{name}$");

    if !docker_out(&["ps", "-aq", "-f", &by_name]).trim().is_empty() {
        // The `target/` volume persists across this, so the build cache survives.
        let current = docker_out(&["inspect", &name, "--format", "{{.Config.Image}}"])
            .trim()
            .to_string();
        if current != image {
            eprintln!("Toolchain image changed ({current} -> {image}) — recreating container {name}...");
            docker(&["rm", "-f", &name]);
            // Remove the now-superseded image (best-effort; ignored if another
            // checkout's container still holds it).
            if !current.is_empty() && current != image {
                let _ = Command::new("docker").args(["rmi", &current]).output();
            }
        }
    }

    if docker_out(&["ps", "-aq", "-f", &by_name]).trim().is_empty() {
        eprintln!("Creating container {name}...");
        let mount = format!("{}:/app/src", root.display());
        // `target/` lives on a per-platform named volume (inside the Docker VM),
        // not the bind mount: host bind mounts are slow for cargo's small-file
        // churn, and a per-platform volume stops two platform containers racing
        // on a shared cargo target-dir. `dist/` stays on the bind mount so built
        // binaries are visible on the host, and the volume survives `destroy`'s
        // sibling containers so each platform's build cache persists.
        let target_mount = format!("{}:/app/src/target", target_volume(&name));
        let st = Command::new("docker")
            .args([
                "create", "--name", &name, "-v", &mount, "-v", &target_mount, "-w",
                "/app/src", image, "sleep", "infinity",
            ])
            .status()
            .unwrap_or_else(|e| fail(format!("docker create failed: {e}")));
        if !st.success() {
            std::process::exit(st.code().unwrap_or(1));
        }
    }
    docker(&["start", &name]);
}

/// Remove every tag of `repo` except `keep` (best-effort, non-forced). `repo` is
/// the untagged ref (e.g. `ghcr.io/renzora/linux`); `keep` is the full
/// current ref to preserve.
fn remove_other_tags(repo: &str, keep: &str) {
    let listed = docker_out(&["images", "--format", "{{.Repository}}:{{.Tag}}", repo]);
    for line in listed.lines() {
        let img = line.trim();
        if img.is_empty() || img == keep || img.ends_with(":<none>") {
            continue;
        }
        let _ = Command::new("docker").args(["rmi", img]).output();
    }
}

/// `docker pull <image>`; returns true on success. Failures are quiet so the
/// caller can fall back to building locally.
///
/// On arm64 hosts (Apple Silicon), a tag with only a linux/amd64 manifest fails
/// the native pull with "no matching manifest"; retry pinned to linux/amd64
/// (Docker Desktop runs it under Rosetta) before dropping to a local build.
fn pull_image(image: &str) -> bool {
    let native = Command::new("docker")
        .args(["pull", image])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if native || !cfg!(target_arch = "aarch64") {
        return native;
    }
    eprintln!("No arm64 image for {image} — pulling linux/amd64 (runs emulated; on macOS enable Rosetta in Docker Desktop settings)...");
    Command::new("docker")
        .args(["pull", "--platform", "linux/amd64", image])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Build an image from `dockerfile`, tagged `image` (offline / local-edit
/// fallback). `base_tag_arg`, when set, is passed as `--build-arg BASE_TAG=` so a
/// platform image's `FROM base:${BASE_TAG}` resolves to the right base.
fn build_image(root: &Path, dockerfile: &str, image: &str, base_tag_arg: Option<&str>) {
    let mut args: Vec<String> = vec!["build".into(), "-f".into(), dockerfile.into()];
    if let Some(bt) = base_tag_arg {
        args.push("--build-arg".into());
        args.push(format!("BASE_TAG={bt}"));
    }
    args.extend(["-t".into(), image.into(), ".".into()]);
    let st = Command::new("docker")
        .current_dir(root)
        .args(&args)
        .status()
        .unwrap_or_else(|e| fail(format!("docker build failed to start (is Docker installed/running?): {e}")));
    if !st.success() {
        std::process::exit(st.code().unwrap_or(1));
    }
}

// ── Commands ─────────────────────────────────────────────────────────────────

/// The linux container is where Linux-native ops run (test/check/shell/clean,
/// plugin scaffolding, UPX). Ensures it's up and returns its name.
fn linux_container(root: &Path) -> String {
    ensure_up(root, "linux");
    container_name(root, "linux")
}

/// `renzora build [platforms]` — bare = every platform; else the listed tokens
/// grouped by their image so each container is brought up once.
fn build_cmd(root: &Path, tokens: Vec<String>) {
    if tokens.is_empty() {
        for plat in ALL_PLATFORMS {
            ensure_up(root, plat);
            let name = container_name(root, plat);
            dexec(&name, &format!("bash /app/src/docker/build-all.sh dist {plat}"));
        }
        return;
    }

    // Group tokens by image, preserving first-seen order.
    let mut groups: Vec<(&'static str, Vec<String>)> = Vec::new();
    for tok in tokens {
        match image_for_token(&tok) {
            Some(img) => match groups.iter_mut().find(|(i, _)| *i == img) {
                Some(g) => g.1.push(tok),
                None => groups.push((img, vec![tok])),
            },
            None => eprintln!("renzora: unknown platform '{tok}' (skipping)"),
        }
    }
    for (img, toks) in groups {
        ensure_up(root, img);
        let name = container_name(root, img);
        // Via `bash` so a checkout without exec bits still works.
        dexec(&name, &format!("bash /app/src/docker/build-all.sh dist {}", toks.join(" ")));
    }
}

/// Cross-build for the host platform, then run the produced binary natively (the
/// GPU stays on the host; the container can't display).
fn run(root: &Path, target: Option<String>) {
    let host = host_platform();
    ensure_up(root, host.image);
    let name = container_name(root, host.image);

    let feature = target.unwrap_or_else(|| "editor".into());
    if feature != "editor" && feature != "runtime" {
        fail("usage: renzora run [editor|runtime]".into());
    }
    dexec(&name, &format!("bash /app/src/docker/build-all.sh dist {}", host.build_arg));

    // Operation Merge: one binary, one flat folder. The editor and the game are
    // the SAME exe — the `renzora_editor` bundle dll beside it makes it the
    // editor; `--no-editor` runs that same exe as the game.
    //
    // The build wraps the editor output per platform: macOS into a .app bundle
    // (exe + dylibs/plugins in Contents/MacOS), Linux into an AppImage AppDir
    // whose AppRun sets LD_LIBRARY_PATH before exec'ing — launch through those.
    // Windows stays a flat folder (DLLs resolve from the exe's directory).
    let dist = root.join("dist").join(host.outdir);
    let (dir, bin) = if cfg!(target_os = "macos") {
        (dist.join("Renzora Engine.app").join("Contents").join("MacOS"), "renzora".to_string())
    } else if cfg!(target_os = "windows") {
        (dist, format!("renzora{}", host.ext))
    } else {
        (dist.join("Renzora Engine.AppDir"), "AppRun".to_string())
    };
    let path = dir.join(&bin);
    if !path.exists() {
        fail(format!("built binary not found: {}", path.display()));
    }
    println!("Running {} ({feature}) ...", path.display());
    let mut cmd = Command::new(&path);
    cmd.current_dir(&dir);
    if feature == "runtime" {
        cmd.arg("--no-editor");
    }
    let st = cmd
        .status()
        .unwrap_or_else(|e| fail(format!("failed to launch {}: {e}", path.display())));
    std::process::exit(st.code().unwrap_or(0));
}

/// Remove this checkout's per-platform containers and their build-cache volumes.
fn destroy(root: &Path) {
    for plat in ALL_PLATFORMS {
        let name = container_name(root, plat);
        docker(&["rm", "-f", &name]);
        docker(&["volume", "rm", "-f", &target_volume(&name)]);
    }
    println!("Removed this checkout's containers and build-cache volumes.");
}

/// Remove this checkout's stale (non-current) toolchain images — keeps the
/// tags the current Dockerfiles hash to, drops the rest.
fn prune(root: &Path) {
    for plat in ALL_PLATFORMS {
        remove_other_tags(&format!("{IMAGE}/{plat}"), &image_ref(root, plat));
    }
    remove_other_tags(&format!("{IMAGE}/base"), &base_image_ref(root));
    println!("Pruned stale toolchain images.");
}

/// Host platform → its `build-all.sh` arg (the host arch only, so `run` doesn't
/// also cross-build the other slice), its toolchain image, the `dist/` output
/// subdir, and the executable extension.
struct HostPlatform {
    build_arg: &'static str,
    image: &'static str,
    outdir: &'static str,
    ext: &'static str,
}

fn host_platform() -> HostPlatform {
    if cfg!(target_os = "windows") {
        HostPlatform { build_arg: "windows", image: "windows", outdir: "windows-x64", ext: ".exe" }
    } else if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            HostPlatform { build_arg: "macos-arm64", image: "macos", outdir: "macos-arm64", ext: "" }
        } else {
            HostPlatform { build_arg: "macos-x64", image: "macos", outdir: "macos-x64", ext: "" }
        }
    } else if cfg!(target_arch = "aarch64") {
        HostPlatform { build_arg: "linux-arm64", image: "linux", outdir: "linux-arm64", ext: "" }
    } else {
        HostPlatform { build_arg: "linux-x64", image: "linux", outdir: "linux-x64", ext: "" }
    }
}

// ── Docker helpers ───────────────────────────────────────────────────────────

fn dexec(name: &str, cmd: &str) {
    let st = Command::new("docker")
        .args(["exec", name, "bash", "-c", cmd])
        .status()
        .unwrap_or_else(|e| fail(format!("docker exec failed: {e}")));
    if !st.success() {
        std::process::exit(st.code().unwrap_or(1));
    }
}

fn docker(args: &[&str]) -> ExitStatus {
    Command::new("docker")
        .args(args)
        .status()
        .unwrap_or_else(|e| fail(format!("failed to run docker (is it installed/running?): {e}")))
}

fn docker_out(args: &[&str]) -> String {
    Command::new("docker")
        .args(args)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

fn fail(msg: String) -> ! {
    eprintln!("renzora: {msg}");
    std::process::exit(1);
}

/// Best-effort, throttled self-update from crates.io.
///
/// When a newer published CLI exists and stdin is a TTY, prompt to update and,
/// on yes, run `cargo install renzora` (the new version takes effect next run).
/// Non-interactive (CI, piped) → just print a one-line notice and continue.
/// Throttled to once per day via a temp marker so it never slows normal use, and
/// silent on any failure (offline, cargo missing, parse error). Uses
/// `cargo search` so there's no HTTP dependency.
fn check_for_update() {
    use std::time::Duration;

    let marker = std::env::temp_dir().join("renzora-cli-update-check");
    // Skip if we already checked within the last 24h.
    if let Ok(modified) = std::fs::metadata(&marker).and_then(|m| m.modified()) {
        if modified
            .elapsed()
            .map(|d| d < Duration::from_secs(86_400))
            .unwrap_or(false)
        {
            return;
        }
    }
    // Record the attempt up front so an offline/failed check doesn't retry for a
    // day (writing also refreshes the mtime).
    let _ = std::fs::write(&marker, b"");

    let Ok(out) = Command::new("cargo")
        .args(["search", "renzora", "--limit", "1"])
        .output()
    else {
        return;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    // The matching line looks like: renzora = "0.1.4"    # description
    let Some(latest) = text.lines().find_map(|l| {
        l.trim()
            .strip_prefix("renzora = \"")
            .and_then(|rest| rest.split('"').next())
    }) else {
        return;
    };

    let current = env!("CARGO_PKG_VERSION");
    if !version_gt(latest, current) {
        return;
    }

    // Non-interactive: nag and move on (never block a script on a prompt).
    if !io::stdin().is_terminal() {
        eprintln!("renzora: v{latest} is available (you have v{current}) — update with `cargo install renzora`");
        return;
    }

    eprint!("renzora: v{latest} is available (you have v{current}). Update now? [Y/n] ");
    let _ = io::stderr().flush();
    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_err() {
        return;
    }
    let ans = input.trim().to_lowercase();
    if !(ans.is_empty() || ans == "y" || ans == "yes") {
        return;
    }

    eprintln!("Updating via `cargo install renzora` ...");
    let ok = Command::new("cargo")
        .args(["install", "renzora"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        // The running process is still the old binary; the update applies on the
        // next invocation. Continue the current command with what's loaded.
        eprintln!("renzora: updated to v{latest} (effective next run). Continuing...");
    } else {
        eprintln!("renzora: update failed — run `cargo install renzora` manually (on Windows, close any running renzora first).");
    }
}

/// True if dotted version `a` is strictly greater than `b` (numeric, by
/// component). Pre-release suffixes are ignored. Good enough for a nag.
fn version_gt(a: &str, b: &str) -> bool {
    fn parts(v: &str) -> Vec<u64> {
        v.split('.')
            .map(|p| p.split('-').next().unwrap_or("").parse().unwrap_or(0))
            .collect()
    }
    parts(a) > parts(b)
}

#[cfg(test)]
mod tests {
    use super::{hash12, version_gt};

    #[test]
    fn version_comparison() {
        assert!(version_gt("0.1.4", "0.1.3"));
        assert!(version_gt("0.2.0", "0.1.9"));
        assert!(version_gt("0.1.10", "0.1.3")); // numeric, not lexical
        assert!(version_gt("1.0.0", "0.9.9"));
        assert!(!version_gt("0.1.3", "0.1.3")); // equal -> no nag
        assert!(!version_gt("0.1.3", "0.1.4")); // local newer than published
        assert!(!version_gt("0.1.4-beta", "0.1.4")); // pre-release suffix ignored
    }

    #[test]
    fn platform_tag_folds_in_base() {
        // 12 hex chars, and the platform hash depends on the base tag (so a base
        // change cascades). Mirrors CI: sha256(baseTag + platform Dockerfile).
        let base = hash12(&[b"BASE\n"]);
        assert_eq!(base.len(), 12);
        let plat_a = hash12(&[base.as_bytes(), b"PLAT\n"]);
        let plat_b = hash12(&[hash12(&[b"DIFFERENT\n"]).as_bytes(), b"PLAT\n"]);
        assert_eq!(plat_a.len(), 12);
        assert_ne!(plat_a, plat_b); // same platform file, different base -> different tag
    }
}
