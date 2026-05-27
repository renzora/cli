//! `renzora` — the Renzora engine CLI.
//!
//! Scaffolds projects (`renzora new`) and drives the pinned `renzora/engine`
//! container for everything else, so builds/tests run in one controlled
//! toolchain (the ABI contract the dlopen plugin system depends on). Install
//! with `cargo install renzora`; it finds the engine checkout by walking up
//! from the current directory.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

use clap::{Parser, Subcommand};

const IMAGE: &str = "ghcr.io/renzora/engine";
const DOCKERFILE: &str = "docker/engine-builder/Dockerfile";
const ENGINE_REPO: &str = "https://github.com/renzora/engine";
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
#[command(name = "renzora", about = "Renzora engine CLI — scaffold projects and run everything in the pinned container.", version)]
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
    /// Build the image + create/start the container (idempotent).
    Init,
    /// Cross-build for one or more platforms (no args = all).
    Build { platforms: Vec<String> },
    /// Run the test suite in the container (no args = workspace suite).
    Test {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// `cargo check` in the container.
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
    /// Interactive shell in the container.
    Shell,
    /// Remove target/ in the container.
    Clean,
    /// Remove the container.
    Destroy,
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
    let name = container_name(&root);

    match cli.command {
        Commands::New { .. } => unreachable!("handled above"),
        Commands::Init => {
            ensure_up(&root, &name);
            println!("Container {name} is running.");
        }
        Commands::Build { platforms } => {
            ensure_up(&root, &name);
            dexec(&name, &format!("/app/src/docker/scripts/build-all.sh dist {}", platforms.join(" ")));
        }
        Commands::Test { args } => {
            ensure_up(&root, &name);
            if args.is_empty() {
                dexec(&name, &format!("cargo test --workspace {}", EXCLUDES.join(" ")));
            } else {
                dexec(&name, &format!("cargo test {}", args.join(" ")));
            }
        }
        Commands::Check { args } => {
            ensure_up(&root, &name);
            if args.is_empty() {
                dexec(&name, &format!("cargo check --workspace {}", EXCLUDES.join(" ")));
            } else {
                dexec(&name, &format!("cargo check {}", args.join(" ")));
            }
        }
        Commands::Run { target } => run(&root, &name, target),
        Commands::Add { args } => {
            ensure_up(&root, &name);
            dexec(&name, &format!("bash docker/scripts/add-plugin.sh {}", args.join(" ")));
        }
        Commands::Remove { args } => {
            ensure_up(&root, &name);
            dexec(&name, &format!("bash docker/scripts/remove-plugin.sh {}", args.join(" ")));
        }
        Commands::Upx { args } => {
            ensure_up(&root, &name);
            dexec(&name, &format!("bash docker/scripts/upx-compress.sh {}", args.join(" ")));
        }
        Commands::Shell => {
            ensure_up(&root, &name);
            let st = Command::new("docker")
                .args(["exec", "-it", &name, "bash"])
                .status()
                .unwrap_or_else(|e| fail(format!("docker exec failed: {e}")));
            std::process::exit(st.code().unwrap_or(0));
        }
        Commands::Clean => {
            ensure_up(&root, &name);
            // `target/` is a volume mountpoint, so clear its contents rather
            // than removing the directory itself.
            dexec(&name, "find target -mindepth 1 -maxdepth 1 -exec rm -rf {} + && echo 'target/ cleaned'");
        }
        Commands::Destroy => {
            docker(&["rm", "-f", &name]);
            // Remove the build-cache volume too — destroy is a full teardown.
            docker(&["volume", "rm", "-f", &target_volume(&name)]);
            println!("Removed container {name} and its build-cache volume.");
        }
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
    println!("  renzora init     # build the toolchain image + container (first run is slow)");
    println!("  renzora run      # build the editor and launch it");
}

/// Walk up from the current dir to the engine repo root (the dir holding the
/// builder Dockerfile).
fn find_repo_root() -> PathBuf {
    let mut dir = std::env::current_dir().unwrap_or_else(|e| fail(format!("cannot read cwd: {e}")));
    loop {
        if dir.join(DOCKERFILE).exists() {
            return dir;
        }
        match dir.parent() {
            Some(p) => dir = p.to_path_buf(),
            None => fail(format!(
                "not inside a Renzora engine checkout (no {DOCKERFILE} found). Run `renzora new <name>` first."
            )),
        }
    }
}

/// Per-checkout container name, derived from the repo path (stable per clone).
fn container_name(root: &Path) -> String {
    let mut h = DefaultHasher::new();
    root.to_string_lossy().hash(&mut h);
    format!("renzora-{:08x}", h.finish() as u32)
}

/// Build the image if missing, create the container if missing, start it.
///
/// Reuses an existing container across commands (no per-invocation recreate).
/// After rebuilding the image (e.g. editing the Dockerfile), run
/// `renzora destroy && renzora init` to recreate the container against it.
fn ensure_up(root: &Path, name: &str) {
    if docker_out(&["images", "-q", IMAGE]).trim().is_empty() {
        // Prefer the prebuilt toolchain image from the registry: a ~3 GB pull
        // takes a few minutes, versus 10-25 min to build osxcross/NDK/xwin/etc.
        // from scratch. Fall back to building it locally if the pull fails
        // (offline, or an engine dev who has edited the Dockerfile).
        eprintln!("Fetching toolchain image {IMAGE} (first time)...");
        if !pull_image() {
            eprintln!("Could not pull {IMAGE} — building it locally instead (this takes a while)...");
            build_image(root);
        }
    }
    let by_name = format!("name=^{name}$");
    if docker_out(&["ps", "-aq", "-f", &by_name]).trim().is_empty() {
        eprintln!("Creating container {name}...");
        let mount = format!("{}:/app/src", root.display());
        // `target/` lives on a named volume (inside the Docker VM), not on the
        // bind mount. On Windows/macOS the host bind mount crosses the VM
        // boundary, which is slow for cargo's many-small-file churn; a volume
        // runs at native Linux speed. `dist/` stays on the bind mount so built
        // binaries remain visible on the host, and the volume persists across
        // `renzora destroy` so the build cache survives container recreation.
        let target_mount = format!("{}:/app/src/target", target_volume(name));
        let st = Command::new("docker")
            .args([
                "create", "--name", name, "-v", &mount, "-v", &target_mount, "-w",
                "/app/src", IMAGE, "sleep", "infinity",
            ])
            .status()
            .unwrap_or_else(|e| fail(format!("docker create failed: {e}")));
        if !st.success() {
            std::process::exit(st.code().unwrap_or(1));
        }
    }
    docker(&["start", name]);
}

/// Name of the per-container named volume that backs `/app/src/target`.
fn target_volume(name: &str) -> String {
    format!("{name}-target")
}

/// `docker pull IMAGE`; returns true on success. Failures are quiet so the
/// caller can fall back to building locally.
fn pull_image() -> bool {
    Command::new("docker")
        .args(["pull", IMAGE])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Build the toolchain image from the Dockerfile (the offline / Dockerfile-edit
/// fallback when the registry pull isn't available).
fn build_image(root: &Path) {
    let st = Command::new("docker")
        .current_dir(root)
        .args(["build", "-f", DOCKERFILE, "-t", IMAGE, "."])
        .status()
        .unwrap_or_else(|e| fail(format!("docker build failed to start (is Docker installed/running?): {e}")));
    if !st.success() {
        std::process::exit(st.code().unwrap_or(1));
    }
}

/// Cross-build for the host, then run the produced binary natively (GPU stays
/// on the host; the container can't display).
fn run(root: &Path, name: &str, target: Option<String>) {
    ensure_up(root, name);
    let feature = target.unwrap_or_else(|| "editor".into());
    if feature != "editor" && feature != "runtime" {
        fail("usage: renzora run [editor|runtime]".into());
    }
    let (platform, outdir, ext) = host_platform();
    dexec(name, &format!("/app/src/docker/scripts/build-all.sh dist {platform}"));

    let bin = if feature == "runtime" { "renzora-runtime" } else { "renzora" };
    let dir = root.join("dist").join(outdir).join(&feature);
    let path = dir.join(format!("{bin}{ext}"));
    if !path.exists() {
        fail(format!("built binary not found: {}", path.display()));
    }
    println!("Running {} ...", path.display());
    let st = Command::new(&path)
        .current_dir(&dir)
        .status()
        .unwrap_or_else(|e| fail(format!("failed to launch {}: {e}", path.display())));
    std::process::exit(st.code().unwrap_or(0));
}

/// (platform arg for build-all.sh, dist output dir, executable extension).
fn host_platform() -> (&'static str, &'static str, &'static str) {
    if cfg!(target_os = "windows") {
        ("windows", "windows-x64", ".exe")
    } else if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            ("macos", "macos-arm64", "")
        } else {
            ("macos", "macos-x64", "")
        }
    } else {
        ("linux", "linux-x64", "")
    }
}

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

/// Best-effort, throttled check for a newer published CLI on crates.io.
///
/// Prints a one-line notice to stderr if a newer version exists. Throttled to
/// once per day via a temp marker file so it never slows down normal use, and
/// silently ignores any failure (offline, cargo missing, parse error). Uses
/// `cargo search` so there's no HTTP dependency — cargo is always present since
/// the CLI is installed with it.
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
    // Record the attempt up front so an offline/failed check doesn't retry for
    // a day (writing also refreshes the mtime).
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
    if version_gt(latest, current) {
        eprintln!(
            "renzora: v{latest} is available (you have v{current}) — update with `cargo install renzora`"
        );
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
    use super::version_gt;

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
}
