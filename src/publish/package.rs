//! Turning a directory into the one archive the marketplace stores.
//!
//! The rules are deliberately close to `cargo package`: walk the directory,
//! drop the things nobody wants shipped (`target/`, `.git/`), honour the
//! manifest's `exclude`/`include`, and write what's left into a zip whose paths
//! are relative to the directory root. That last part matters for plugins —
//! the server looks for `Cargo.toml` at the archive root, and the editor
//! extracts the archive straight into `plugins/<crate>/`.

use std::io::Write;
use std::path::{Path, PathBuf};

use zip::write::SimpleFileOptions;

use super::manifest::Manifest;

/// The server's per-file cap. Checked here so a too-large package fails in a
/// second rather than after uploading 200MB of it.
pub const MAX_ARCHIVE_BYTES: usize = 200 * 1024 * 1024;

/// Directories and files that are never part of a published asset: build
/// output, VCS metadata, editor droppings. Matched on a whole path component,
/// so a file named `target.png` is kept.
///
/// Two of these are Renzora-specific. A plugin's compiled library and its
/// `stamp.txt` live in `plugins/<name>/build/`, and that library is rebuilt on
/// the machine that installs the plugin, so shipping it would put one
/// platform's binary inside an archive whose whole point is to be source.
/// `plugin.toml` is the receipt the marketplace installer writes next to an
/// installed plugin — it records *that* machine's asset id and slug, and
/// publishing it would hand a copy of one install's identity to everyone who
/// downloads the plugin afterwards.
const ALWAYS_EXCLUDED: &[&str] = &[
    "target",
    "build",
    "dist",
    "plugin.toml",
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    ".DS_Store",
    "Thumbs.db",
    ".renzora",
];

pub struct Package {
    pub bytes: Vec<u8>,
    /// Archive-relative paths, in the order they were written.
    pub files: Vec<String>,
    /// Total size of the files before compression.
    pub uncompressed: u64,
}

impl Package {
    pub fn len(&self) -> usize {
        self.bytes.len()
    }
}

/// Printed without the archive itself — a few megabytes of zip in a test
/// failure helps nobody.
impl std::fmt::Debug for Package {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Package")
            .field("files", &self.files)
            .field("compressed", &self.bytes.len())
            .field("uncompressed", &self.uncompressed)
            .finish()
    }
}

/// Build the archive for `manifest`'s directory.
pub fn build(manifest: &Manifest) -> Result<Package, String> {
    let files = collect(manifest)?;
    if files.is_empty() {
        return Err(format!(
            "nothing to publish in {} — every file was excluded.",
            manifest.dir.display()
        ));
    }

    // A plugin's archive is compiled by whoever downloads it, so the manifest
    // has to be at the root where the server and the editor both look for it.
    if manifest.is_plugin() && !files.iter().any(|(rel, _)| rel == "Cargo.toml") {
        return Err(format!(
            "Cargo.toml is excluded from the package, but a plugin is published \
             as its source — remove it from `exclude` (or from `include`'s gaps) \
             in {}.",
            manifest.source.display()
        ));
    }

    let mut uncompressed = 0u64;
    let mut names = Vec::with_capacity(files.len());
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    for (rel, abs) in &files {
        let data = std::fs::read(abs).map_err(|e| format!("could not read {}: {e}", abs.display()))?;
        writer
            .start_file(rel, options)
            .map_err(|e| format!("could not add {rel} to the archive: {e}"))?;
        writer
            .write_all(&data)
            .map_err(|e| format!("could not write {rel} into the archive: {e}"))?;
        uncompressed += data.len() as u64;
        names.push(rel.clone());
    }

    let bytes = writer
        .finish()
        .map_err(|e| format!("could not finish the archive: {e}"))?
        .into_inner();

    if bytes.len() > MAX_ARCHIVE_BYTES {
        return Err(format!(
            "the package is {}, over the {} limit. Trim it with `exclude` in {}.",
            human_size(bytes.len() as u64),
            human_size(MAX_ARCHIVE_BYTES as u64),
            manifest.source.display()
        ));
    }

    Ok(Package {
        bytes,
        files: names,
        uncompressed,
    })
}

/// Every file to ship, as (archive-relative path, absolute path), sorted so the
/// same directory always produces the same archive.
fn collect(manifest: &Manifest) -> Result<Vec<(String, PathBuf)>, String> {
    let include = compile(&manifest.include)?;
    let exclude = compile(&manifest.exclude)?;

    let mut out = Vec::new();
    walk(&manifest.dir, &manifest.dir, &include, &exclude, &mut out)?;
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

fn compile(patterns: &[String]) -> Result<Vec<glob::Pattern>, String> {
    patterns
        .iter()
        .map(|p| glob::Pattern::new(p).map_err(|e| format!("invalid glob \"{p}\": {e}")))
        .collect()
}

fn walk(
    root: &Path,
    dir: &Path,
    include: &[glob::Pattern],
    exclude: &[glob::Pattern],
    out: &mut Vec<(String, PathBuf)>,
) -> Result<(), String> {
    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("could not read {}: {e}", dir.display()))?;

    for entry in entries {
        let entry = entry.map_err(|e| format!("could not read {}: {e}", dir.display()))?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();

        if ALWAYS_EXCLUDED.contains(&name.as_str()) {
            continue;
        }

        let Some(rel) = relative(root, &path) else {
            continue;
        };

        let file_type = entry
            .file_type()
            .map_err(|e| format!("could not stat {}: {e}", path.display()))?;

        // Symlinks are skipped rather than followed: a link out of the
        // directory would quietly pull in files the seller never meant to
        // publish, and one pointing back in would loop.
        if file_type.is_symlink() {
            continue;
        }

        if file_type.is_dir() {
            // A directory matching `exclude` prunes the whole subtree. An
            // `include` list can't prune here — `include = ["assets/*.png"]`
            // has to descend into `assets/` to find them.
            if matches_any(exclude, &rel) {
                continue;
            }
            walk(root, &path, include, exclude, out)?;
            continue;
        }

        if !file_type.is_file() {
            continue;
        }
        if matches_any(exclude, &rel) {
            continue;
        }
        if !include.is_empty() && !matches_any(include, &rel) {
            continue;
        }
        out.push((rel, path));
    }
    Ok(())
}

/// `path` relative to `root`, with forward slashes — zip entry names are
/// `/`-separated regardless of the platform that wrote them.
fn relative(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let joined = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join("/");
    (!joined.is_empty()).then_some(joined)
}

/// Does any pattern match this path?
///
/// A bare `tests` also matches everything under it, so `exclude = ["tests"]`
/// means what a person writing it expects without them having to spell
/// `tests/**`.
fn matches_any(patterns: &[glob::Pattern], rel: &str) -> bool {
    patterns.iter().any(|p| {
        p.matches(rel) || rel.starts_with(&format!("{}/", p.as_str().trim_end_matches('/')))
    })
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("renzora-package-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("target/debug")).unwrap();
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"p\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\n[package.metadata.renzora]\ncategory = \"plugins\"\nmarketplace_id = \"test-id\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("src/lib.rs"), "// code").unwrap();
        std::fs::write(dir.join("target/debug/p.rlib"), "binary").unwrap();
        std::fs::write(dir.join("tests/it.rs"), "// test").unwrap();
        dir
    }

    #[test]
    fn build_output_is_never_packaged() {
        let dir = fixture("basic");
        // What `cargo renzora` leaves beside a plugin's source.
        std::fs::create_dir_all(dir.join("build")).unwrap();
        std::fs::write(dir.join("build/p.dll"), "MZ").unwrap();
        std::fs::write(dir.join("build/stamp.txt"), "rustc 1.90.0").unwrap();

        let manifest = crate::publish::manifest::load(&dir).unwrap();
        let pkg = build(&manifest).unwrap();

        assert!(pkg.files.contains(&"Cargo.toml".to_string()));
        assert!(pkg.files.contains(&"src/lib.rs".to_string()));
        assert!(
            !pkg.files.iter().any(|f| f.starts_with("target/")),
            "target/ leaked into {:?}",
            pkg.files
        );
        assert!(
            !pkg.files.iter().any(|f| f.starts_with("build/")),
            "a compiled plugin library must not ship inside its source: {:?}",
            pkg.files
        );
    }

    #[test]
    fn an_install_receipt_is_never_packaged() {
        let dir = fixture("receipt");
        // What the marketplace installer leaves behind, naming this machine's
        // copy of the asset.
        std::fs::write(
            dir.join("plugin.toml"),
            "asset_id = \"c1091f59-6353-4ee4-bd41-ff86d9a1979e\"
slug = \"p-c1091f59\"
",
        )
        .unwrap();

        let manifest = crate::publish::manifest::load(&dir).unwrap();
        let pkg = build(&manifest).unwrap();

        assert!(
            !pkg.files.contains(&"plugin.toml".to_string()),
            "one install's identity must not ship to every other install: {:?}",
            pkg.files
        );
    }

    #[test]
    fn a_bare_directory_name_excludes_its_contents() {
        let dir = fixture("exclude");
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"p\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\n[package.metadata.renzora]\ncategory = \"plugins\"\nmarketplace_id = \"test-id\"\nexclude = [\"tests\"]\n",
        )
        .unwrap();
        let manifest = crate::publish::manifest::load(&dir).unwrap();
        let pkg = build(&manifest).unwrap();
        assert!(!pkg.files.iter().any(|f| f.starts_with("tests/")), "{:?}", pkg.files);
    }

    #[test]
    fn a_plugin_without_its_manifest_is_rejected() {
        let dir = fixture("nomanifest");
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"p\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\n[package.metadata.renzora]\ncategory = \"plugins\"\nmarketplace_id = \"test-id\"\ninclude = [\"src/**\"]\n",
        )
        .unwrap();
        let manifest = crate::publish::manifest::load(&dir).unwrap();
        let err = build(&manifest).unwrap_err();
        assert!(err.contains("Cargo.toml"), "{err}");
    }

    #[test]
    fn the_archive_reads_back_with_its_paths_intact() {
        let dir = fixture("roundtrip");
        let manifest = crate::publish::manifest::load(&dir).unwrap();
        let pkg = build(&manifest).unwrap();
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(pkg.bytes)).unwrap();
        let names: Vec<String> = (0..zip.len())
            .map(|i| zip.by_index(i).unwrap().name().to_string())
            .collect();
        assert!(names.contains(&"src/lib.rs".to_string()), "{names:?}");
    }

    #[test]
    fn sizes_render_readably() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KiB");
    }
}
