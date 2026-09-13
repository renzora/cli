//! The publish manifest: what a directory says about itself.
//!
//! For a Rust crate that is `Cargo.toml` — name, version and description come
//! from `[package]` exactly as `cargo publish` reads them, and the
//! marketplace-only fields live under `[package.metadata.renzora]`, a section
//! cargo ignores. Keeping one manifest means the version you bump to release is
//! the version that ships; there is no second file to forget.
//!
//! A directory with no `Cargo.toml` — a pack of models, a material library —
//! uses `renzora.toml` instead, carrying the same keys plus the `name`,
//! `version` and `description` that `[package]` would otherwise have supplied.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Licences the marketplace accepts, mirroring `VALID_LICENCES` server-side.
pub const VALID_LICENCES: &[&str] = &["standard", "extended", "cc0", "mit", "apache2", "gpl3"];

/// Categories whose upload is buildable plugin source. The server applies the
/// same rule (`is_plugin_category`); knowing it here is what lets us reject a
/// bad package before spending an upload on it.
pub fn is_plugin_category(slug: &str) -> bool {
    matches!(slug, "plugins" | "plugin")
}

/// A manifest resolved down to what the marketplace actually needs.
#[derive(Debug, Clone)]
pub struct Manifest {
    /// Directory being published — every relative path below resolves against it.
    pub dir: PathBuf,
    /// Which file it came from, for error messages.
    pub source: PathBuf,
    /// The listing's display name: `title` when stated, else the crate name.
    /// Used when *creating* a listing, which has to be called something.
    pub name: String,
    /// The title the manifest actually states, if it states one.
    ///
    /// Kept apart from `name` because a listing already on the marketplace was
    /// titled by a person — "CRT Fx" for the `crt` crate — and a directory name
    /// is not an instruction to rename it. Only a stated title overwrites one.
    pub title: Option<String>,
    pub version: String,
    pub description: String,
    pub category: String,
    pub subcategory: String,
    pub tags: Vec<String>,
    /// `None` when the manifest says nothing about the licence — as opposed to
    /// stating "standard", which it may also do.
    pub licence: Option<String>,
    pub price_credits: Option<i64>,
    pub ai_generated: Option<bool>,
    pub credit_name: String,
    pub credit_url: String,
    /// `"keep"` stores the archive whole, `"extract"` unpacks it into a browsable
    /// tree. Plugins are always kept — the editor builds those bytes.
    pub zip_action: String,
    /// Oldest engine release this asset supports, as a release tag. The editor
    /// reads it back (`engine_satisfies`) and will not offer the update to
    /// anyone running something older.
    pub min_engine_version: Option<String>,
    /// This directory's identity on the marketplace: unique across the whole
    /// registry, claimed on first publish, never changed.
    ///
    /// Required, and the only thing a publish matches on. A title can be
    /// anything and be edited later — "CRT Fx" for the `crt` plugin — so
    /// matching on one would mean guessing. Here the answer is exact: the id
    /// is free, or it is yours, or it is somebody else's.
    pub marketplace_id: String,
    pub thumbnail: Option<PathBuf>,
    pub screenshots: Vec<PathBuf>,
    pub video: Option<PathBuf>,
    pub audio: Option<PathBuf>,
    /// Extra free-form asset metadata (`render_pipeline`, `poly_count`, ...).
    pub metadata: serde_json::Value,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

impl Manifest {
    /// The filename the archive is uploaded under, and what buyers download.
    pub fn archive_name(&self) -> String {
        format!("{}-{}.zip", slug_ish(&self.name), self.version)
    }

    pub fn is_plugin(&self) -> bool {
        is_plugin_category(&self.category)
    }
}

/// Does this directory claim a marketplace id?
///
/// The id is what makes a directory a listing, so its absence is how a sweep
/// tells a plugin meant for the marketplace from one that merely lives beside
/// it. A repository of plugins holds both — the engine's own `unsupported-targets`
/// gate lives in the same section — and `--all` refusing the whole run because
/// of them would make it unusable on exactly the repository it is for.
///
/// Deliberately a text scan: a directory that is not being published should not
/// have to parse cleanly to be skipped.
pub fn declares_listing(dir: &Path) -> bool {
    for name in ["Cargo.toml", "renzora.toml"] {
        let Ok(text) = std::fs::read_to_string(dir.join(name)) else {
            continue;
        };
        if text.lines().any(|l| l.trim_start().starts_with("marketplace_id")) {
            return true;
        }
    }
    false
}

/// Load the manifest for `dir`, preferring `Cargo.toml`.
pub fn load(dir: &Path) -> Result<Manifest, String> {
    let cargo = dir.join("Cargo.toml");
    let standalone = dir.join("renzora.toml");

    if cargo.is_file() {
        return from_cargo(dir, &cargo);
    }
    if standalone.is_file() {
        return from_standalone(dir, &standalone);
    }
    Err(format!(
        "no manifest in {}\n\
         A Rust crate publishes from its Cargo.toml; anything else needs a \
         renzora.toml beside its files. Both take the same marketplace keys — \
         see `renzora publish --help`.",
        dir.display()
    ))
}

// ── Cargo.toml ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct CargoFile {
    package: Option<CargoPackage>,
}

#[derive(Debug, Deserialize)]
struct CargoPackage {
    name: String,
    #[serde(default)]
    version: Option<Inheritable>,
    #[serde(default)]
    description: Option<Inheritable>,
    #[serde(default)]
    license: Option<Inheritable>,
    #[serde(default)]
    metadata: Option<CargoMetadata>,
}

#[derive(Debug, Deserialize)]
struct CargoMetadata {
    renzora: Option<Section>,
}

/// A `[package]` field that may be `{ workspace = true }` instead of a value.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Inheritable {
    Value(String),
    Inherited { workspace: bool },
}

fn from_cargo(dir: &Path, path: &Path) -> Result<Manifest, String> {
    let file: CargoFile = read_toml(path)?;
    let package = file.package.ok_or_else(|| {
        format!(
            "{} has no [package] section — a workspace root is not something to \
             publish; point at the crate itself.",
            path.display()
        )
    })?;

    // Workspace-inherited fields are resolved here rather than rejected,
    // because a plugin scaffolded inside the engine checkout inherits its
    // version from the workspace and would otherwise never be publishable.
    let inherited = WorkspacePackage::find(dir);
    let version = resolve(package.version, "version", &inherited, path)?
        .ok_or_else(|| format!("{} has no [package] version", path.display()))?;
    let description = resolve(package.description, "description", &inherited, path)?;
    let license = resolve(package.license, "license", &inherited, path)?;

    let section = package
        .metadata
        .and_then(|m| m.renzora)
        .ok_or_else(|| missing_section(path, &package.name, &version))?;

    build(
        dir,
        path,
        section,
        Defaults {
            name: package.name,
            name_is_title: false,
            version,
            description,
            license,
        },
    )
}

/// `[workspace.package]` from the nearest ancestor that defines one.
struct WorkspacePackage {
    path: PathBuf,
    table: toml::Table,
}

impl WorkspacePackage {
    fn find(start: &Path) -> Option<Self> {
        for dir in start.ancestors().skip(1) {
            let candidate = dir.join("Cargo.toml");
            let Ok(text) = std::fs::read_to_string(&candidate) else {
                continue;
            };
            let Ok(parsed) = text.parse::<toml::Table>() else {
                continue;
            };
            if let Some(table) = parsed
                .get("workspace")
                .and_then(|w| w.get("package"))
                .and_then(|p| p.as_table())
            {
                return Some(Self {
                    path: candidate,
                    table: table.clone(),
                });
            }
        }
        None
    }
}

fn resolve(
    field: Option<Inheritable>,
    key: &str,
    workspace: &Option<WorkspacePackage>,
    path: &Path,
) -> Result<Option<String>, String> {
    match field {
        None => Ok(None),
        Some(Inheritable::Value(v)) => Ok(Some(v)),
        Some(Inheritable::Inherited { workspace: false }) => Ok(None),
        Some(Inheritable::Inherited { workspace: true }) => {
            let ws = workspace.as_ref().ok_or_else(|| {
                format!(
                    "{} inherits `{key}` from the workspace, but no ancestor \
                     Cargo.toml has a [workspace.package] section",
                    path.display()
                )
            })?;
            ws.table
                .get(key)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .ok_or_else(|| {
                    format!(
                        "{} inherits `{key}` from the workspace, but {} does not \
                         set [workspace.package] {key}",
                        path.display(),
                        ws.path.display()
                    )
                })
                .map(Some)
        }
    }
}

fn missing_section(path: &Path, name: &str, version: &str) -> String {
    format!(
        "{} has no [package.metadata.renzora] section.\n\
         Add one so the marketplace knows where the listing goes:\n\
         \n\
         \x20   [package.metadata.renzora]\n\
         \x20   category = \"plugins\"\n\
         \x20   tags = [\"editor\", \"tools\"]\n\
         \x20   price_credits = 0\n\
         \n\
         The listing's name ({name}) and version ({version}) come from \
         [package]; cargo ignores the section above.",
        path.display()
    )
}

// ── renzora.toml ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct Standalone {
    name: Option<String>,
    version: Option<String>,
    description: Option<String>,
    license: Option<String>,
    #[serde(flatten)]
    section: Section,
}

fn from_standalone(dir: &Path, path: &Path) -> Result<Manifest, String> {
    let file: Standalone = read_toml(path)?;
    let name = file
        .name
        .clone()
        .ok_or_else(|| format!("{} has no `name`", path.display()))?;
    let version = file
        .version
        .clone()
        .ok_or_else(|| format!("{} has no `version`", path.display()))?;

    build(
        dir,
        path,
        file.section,
        Defaults {
            name,
            name_is_title: true,
            version,
            description: file.description,
            license: file.license,
        },
    )
}

// ── Shared shape ───────────────────────────────────────────────────────────

/// The `[package.metadata.renzora]` table, and the whole of a `renzora.toml`.
///
/// Unknown keys are collected rather than rejected, because this section is not
/// only ours: the engine's exporter reads `unsupported-targets` from it to
/// decide which plugins a given target can build. Refusing what we do not
/// recognise made those plugins unpublishable.
#[derive(Debug, Default, Deserialize)]
struct Section {
    name: Option<String>,
    description: Option<String>,
    category: Option<String>,
    subcategory: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    licence: Option<String>,
    /// American spelling, accepted because half the ecosystem writes it.
    license_id: Option<String>,
    price_credits: Option<i64>,
    ai_generated: Option<bool>,
    #[serde(default)]
    credit_name: String,
    #[serde(default)]
    credit_url: String,
    zip_action: Option<String>,
    /// The listing's id. Required.
    marketplace_id: Option<String>,
    /// Oldest engine release this asset works on (`r1-alpha8`). Omitted, or
    /// empty, means it declares no floor.
    min_engine_version: Option<String>,
    thumbnail: Option<String>,
    #[serde(default)]
    screenshots: Vec<String>,
    video: Option<String>,
    audio: Option<String>,
    #[serde(default)]
    metadata: toml::Table,
    #[serde(default)]
    include: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
    /// Everything else in the table. Kept so a typo can still be pointed at
    /// without a key belonging to someone else being an error.
    #[serde(flatten)]
    other: toml::Table,
}

/// Keys in this section that belong to something other than publishing, so
/// finding one is not a mistake worth mentioning.
const FOREIGN_KEYS: &[&str] = &["unsupported-targets"];

/// What `[package]` (or the top of a `renzora.toml`) contributes.
struct Defaults {
    name: String,
    /// Whether that name is a *stated* title.
    ///
    /// A crate name is not: `crt` is what cargo calls the package, and the
    /// listing is called "CRT Fx". A `renzora.toml`'s `name` is, because there
    /// is no crate for it to be anything else.
    name_is_title: bool,
    version: String,
    description: Option<String>,
    license: Option<String>,
}

fn build(dir: &Path, source: &Path, s: Section, d: Defaults) -> Result<Manifest, String> {
    // A misspelled key would otherwise do nothing at all, silently: an asset
    // meant to cost credits published free, or a title never applied.
    let unknown: Vec<&str> = s
        .other
        .keys()
        .map(String::as_str)
        .filter(|k| !FOREIGN_KEYS.contains(k))
        .collect();
    if !unknown.is_empty() {
        eprintln!(
            "renzora: {} sets {} under [package.metadata.renzora], which              publishing does not read. A misspelling here is silent, so check              it is meant for something else.",
            source.display(),
            unknown.join(", ")
        );
    }

    let category = s
        .category
        .map(|c| c.trim().to_lowercase())
        .filter(|c| !c.is_empty())
        .ok_or_else(|| {
            format!(
                "{} does not set `category`. Every listing belongs to one — \
                 `renzora publish --list-categories` prints the current set.",
                source.display()
            )
        })?;

    let description = s
        .description
        .or(d.description)
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .ok_or_else(|| {
            format!(
                "{} has no description. Set `description` in [package], or \
                 override it under the renzora section.",
                source.display()
            )
        })?;

    // `None` here means "unstated", not "standard". An update sends only what
    // the manifest states, so the difference decides whether a licence chosen
    // on the website survives the next release.
    let licence = s
        .licence
        .or(s.license_id)
        .map(|l| l.trim().to_lowercase())
        .or_else(|| d.license.as_deref().and_then(licence_from_spdx));

    // Plugins are stored whole whatever the manifest says (the server enforces
    // it too) — the editor extracts the archive and builds it, so unpacking it
    // into loose files server-side would leave nothing to compile.
    let zip_action = match s.zip_action.as_deref().map(str::trim) {
        _ if is_plugin_category(&category) => "keep".to_string(),
        Some("keep") => "keep".to_string(),
        Some("extract") => "extract".to_string(),
        Some(other) => {
            return Err(format!(
                "{}: zip_action must be \"keep\" or \"extract\", not \"{other}\"",
                source.display()
            ))
        }
        // Everything else is content rather than source, so unpacking it is
        // what gives the listing a file tree and a rendered README.
        None => "extract".to_string(),
    };

    let min_engine_version = s
        .min_engine_version
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    if let Some(tag) = &min_engine_version {
        validate_engine_tag(tag, source)?;
    }

    // The engine and the website both read this out of the asset's free-form
    // metadata, so that is where it has to end up; the manifest key is just a
    // name for it that does not require knowing the storage.
    let mut metadata = toml_to_json(toml::Value::Table(s.metadata));
    if let Some(tag) = &min_engine_version {
        match metadata.as_object_mut() {
            Some(obj) => {
                obj.insert("min_engine_version".into(), tag.clone().into());
            }
            None => metadata = serde_json::json!({ "min_engine_version": tag }),
        }
    }

    let marketplace_id = s
        .marketplace_id
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
        .ok_or_else(|| missing_id(source, dir))?;
    validate_marketplace_id(&marketplace_id, source)?;

    let manifest = Manifest {
        dir: dir.to_path_buf(),
        source: source.to_path_buf(),
        title: s
            .name
            .as_deref()
            .map(|t| t.trim().to_string())
            .or_else(|| d.name_is_title.then(|| d.name.trim().to_string())),
        name: s.name.clone().unwrap_or(d.name).trim().to_string(),
        version: d.version.trim().to_string(),
        description,
        category,
        subcategory: s.subcategory.unwrap_or_default().trim().to_lowercase(),
        tags: s
            .tags
            .iter()
            .map(|t| t.trim().to_lowercase())
            .filter(|t| !t.is_empty())
            .collect(),
        licence,
        price_credits: s.price_credits,
        ai_generated: s.ai_generated,
        credit_name: s.credit_name.trim().to_string(),
        credit_url: s.credit_url.trim().to_string(),
        zip_action,
        min_engine_version,
        marketplace_id,
        thumbnail: s
            .thumbnail
            .map(|p| dir.join(p))
            .or_else(|| find_thumbnail(dir)),
        screenshots: s.screenshots.iter().map(|p| dir.join(p)).collect(),
        video: s.video.map(|p| dir.join(p)),
        audio: s.audio.map(|p| dir.join(p)),
        metadata,
        include: s.include,
        exclude: s.exclude,
    };

    validate(&manifest)?;
    Ok(manifest)
}

/// Everything the server would reject, checked before packaging so a mistake
/// costs a second rather than a 100MB upload.
fn validate(m: &Manifest) -> Result<(), String> {
    let where_ = m.source.display();

    if m.name.is_empty() || m.name.chars().count() > 128 {
        return Err(format!("{where_}: name must be 1-128 characters"));
    }
    if m.version.is_empty() || m.version.len() > 32 {
        return Err(format!("{where_}: version must be 1-32 characters"));
    }
    if m.description.chars().count() > 5000 {
        return Err(format!("{where_}: description must be at most 5000 characters"));
    }
    if let Some(licence) = &m.licence {
        if !VALID_LICENCES.contains(&licence.as_str()) {
            return Err(format!(
                "{where_}: unknown licence \"{licence}\". Valid values: {}",
                VALID_LICENCES.join(", ")
            ));
        }
    }
    if m.price_credits.is_some_and(|p| p < 0) {
        return Err(format!("{where_}: price_credits cannot be negative"));
    }
    if m.tags.len() > 5 {
        return Err(format!(
            "{where_}: at most 5 tags (found {})",
            m.tags.len()
        ));
    }
    if let Some(tag) = m.tags.iter().find(|t| t.len() > 32) {
        return Err(format!("{where_}: tag \"{tag}\" is longer than 32 characters"));
    }
    if m.screenshots.len() > 10 {
        return Err(format!("{where_}: at most 10 screenshots"));
    }
    if !m.credit_name.is_empty() && m.price_credits.unwrap_or(0) > 0 {
        return Err(format!(
            "{where_}: an asset credited to {} is published free — remove \
             price_credits or credit_name.",
            m.credit_name
        ));
    }

    for (field, path) in media_paths(m) {
        if !path.is_file() {
            return Err(format!(
                "{where_}: {field} points at {}, which does not exist",
                path.display()
            ));
        }
    }

    for pattern in m.include.iter().chain(&m.exclude) {
        glob::Pattern::new(pattern)
            .map_err(|e| format!("{where_}: invalid glob \"{pattern}\": {e}"))?;
    }

    if m.is_plugin() && !m.dir.join("Cargo.toml").is_file() {
        return Err(format!(
            "{where_}: a plugin is published from its crate, but there is no \
             Cargo.toml in {}",
            m.dir.display()
        ));
    }

    Ok(())
}

/// Every media file the manifest references, paired with the key that named it.
fn media_paths(m: &Manifest) -> Vec<(&'static str, &Path)> {
    let mut out: Vec<(&'static str, &Path)> = Vec::new();
    if let Some(p) = &m.thumbnail {
        out.push(("thumbnail", p));
    }
    for p in &m.screenshots {
        out.push(("screenshots", p));
    }
    if let Some(p) = &m.video {
        out.push(("video", p));
    }
    if let Some(p) = &m.audio {
        out.push(("audio", p));
    }
    out
}

/// What to say when a directory has not claimed an id.
///
/// The directory's own name is nearly always the right answer — it is what the
/// editor installs into and what a plugin is called in conversation — so the
/// message writes the line out rather than describing it.
fn missing_id(source: &Path, dir: &Path) -> String {
    let suggestion = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_else(|| "my-plugin".into());
    format!(
        "{} does not set `marketplace_id`. Every listing needs one: it is \
         claimed on your first publish, is unique across the marketplace, and \
         cannot be changed afterwards.\n\
         \x20   marketplace_id = \"{suggestion}\"",
        source.display()
    )
}

/// The same rule the marketplace applies to a new claim, checked here so a bad
/// handle costs nothing instead of a rejected upload.
///
/// Handles that predate this are grandfathered server-side and can break these
/// rules; only something being claimed now has to satisfy them.
fn validate_marketplace_id(id: &str, source: &Path) -> Result<(), String> {
    let where_ = source.display();
    if id.len() > 64 {
        return Err(format!(
            "{where_}: marketplace_id \"{id}\" is longer than 64 characters"
        ));
    }
    if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Err(format!(
            "{where_}: marketplace_id \"{id}\" may only contain letters, numbers, \
             '_' and '-'"
        ));
    }
    if !id.chars().next().is_some_and(|c| c.is_ascii_alphanumeric()) {
        return Err(format!(
            "{where_}: marketplace_id \"{id}\" must start with a letter or number"
        ));
    }
    Ok(())
}

/// Check an engine tag is one the editor can actually order.
///
/// `engine_satisfies` splits a tag at its trailing digits (`r1-alpha8` ->
/// `("r1-alpha", 8)`) and compares within a matching prefix; a tag it cannot
/// parse is treated as "never block". So a typo does not fail loudly at
/// install time — it silently stops constraining anything, which is the
/// opposite of what someone setting a floor wants. Hence the check here.
///
/// A nightly is rejected outright: it is one night's build, never a floor
/// somebody else's engine can be expected to meet, and the website's own
/// version dropdown filters them out for the same reason.
fn validate_engine_tag(tag: &str, source: &Path) -> Result<(), String> {
    let where_ = source.display();
    if tag.len() > 32 || tag.chars().any(char::is_whitespace) {
        return Err(format!(
            "{where_}: min_engine_version \"{tag}\" is not a release tag — \
             it should look like \"r1-alpha8\"."
        ));
    }
    if tag.to_ascii_lowercase().contains("nightly") {
        return Err(format!(
            "{where_}: min_engine_version \"{tag}\" is a nightly. A floor has to \
             be a release other people can be on — use the release it belongs \
             to, like \"r1-alpha8\"."
        ));
    }
    if !tag.chars().next_back().is_some_and(|c| c.is_ascii_digit()) {
        return Err(format!(
            "{where_}: min_engine_version \"{tag}\" does not end in a number, so \
             the editor cannot order it against the engine it is running and \
             would ignore the floor entirely. Use a tag like \"r1-alpha8\"."
        ));
    }
    Ok(())
}

/// A `thumbnail.<ext>` sitting in the directory, when the manifest names none.
///
/// Every engine plugin that has cover art keeps it at this path, so requiring
/// the key would mean writing down something already obvious from the files.
/// An explicit `thumbnail` still wins — `vignette` keeps its under `src/`, and
/// only the manifest can say so.
fn find_thumbnail(dir: &Path) -> Option<PathBuf> {
    // The extensions the marketplace accepts for an image, in the order a
    // plugin is most likely to use.
    ["png", "jpg", "jpeg", "webp", "gif"]
        .iter()
        .map(|ext| dir.join(format!("thumbnail.{ext}")))
        .find(|p| p.is_file())
}

/// Map an SPDX expression from `[package] license` onto a marketplace licence.
///
/// Only the unambiguous single-licence cases map; `MIT OR Apache-2.0` and other
/// multi-licence expressions fall through to the default rather than guessing
/// which half the seller meant.
fn licence_from_spdx(spdx: &str) -> Option<String> {
    let id = spdx.trim().to_ascii_lowercase();
    let mapped = match id.as_str() {
        "mit" => "mit",
        "apache-2.0" | "apache2.0" | "apache-2" => "apache2",
        "gpl-3.0" | "gpl-3.0-only" | "gpl-3.0-or-later" | "gpl3" => "gpl3",
        "cc0-1.0" | "cc0" => "cc0",
        _ => return None,
    };
    Some(mapped.to_string())
}

fn read_toml<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("could not read {}: {e}", path.display()))?;
    toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

fn toml_to_json(value: toml::Value) -> serde_json::Value {
    match value {
        toml::Value::String(s) => serde_json::Value::String(s),
        toml::Value::Integer(i) => serde_json::Value::Number(i.into()),
        toml::Value::Float(f) => serde_json::Number::from_f64(f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        toml::Value::Boolean(b) => serde_json::Value::Bool(b),
        toml::Value::Datetime(d) => serde_json::Value::String(d.to_string()),
        toml::Value::Array(a) => serde_json::Value::Array(a.into_iter().map(toml_to_json).collect()),
        toml::Value::Table(t) => {
            serde_json::Value::Object(t.into_iter().map(|(k, v)| (k, toml_to_json(v))).collect())
        }
    }
}

/// A filename-safe version of the listing name, for the archive we upload.
fn slug_ish(name: &str) -> String {
    let cleaned: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let joined = cleaned
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if joined.is_empty() {
        "asset".to_string()
    } else {
        joined
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("renzora-manifest-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn reads_cargo_package_and_renzora_section() {
        let dir = tmp("cargo");
        write(
            &dir,
            "Cargo.toml",
            r#"
[package]
name = "my_plugin"
version = "0.3.0"
description = "Adds a terrain sculpt tool"
license = "MIT"

[package.metadata.renzora]
category = "plugins"
marketplace_id = "test-id"
tags = ["Terrain", "editor"]
price_credits = 250
"#,
        );
        let m = load(&dir).unwrap();
        assert_eq!(m.name, "my_plugin");
        assert_eq!(m.version, "0.3.0");
        assert_eq!(m.description, "Adds a terrain sculpt tool");
        assert_eq!(m.licence.as_deref(), Some("mit"), "SPDX MIT maps onto the marketplace licence");
        assert_eq!(m.tags, vec!["terrain", "editor"], "tags are normalised");
        assert_eq!(m.zip_action, "keep", "a plugin's archive is never unpacked");
        assert_eq!(m.archive_name(), "my-plugin-0.3.0.zip");
    }

    #[test]
    fn a_thumbnail_beside_the_manifest_is_found_without_being_named() {
        let dir = tmp("thumb");
        std::fs::write(dir.join("thumbnail.png"), "x").unwrap();
        write(
            &dir,
            "renzora.toml",
            "name = \"x\"\nversion = \"1.0.0\"\ndescription = \"d\"\ncategory = \"3d-models\"\nmarketplace_id = \"test-id\"\n",
        );
        let m = load(&dir).unwrap();
        assert_eq!(m.thumbnail, Some(dir.join("thumbnail.png")));
    }

    #[test]
    fn a_named_thumbnail_wins_over_the_one_lying_around() {
        let dir = tmp("thumb-named");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("thumbnail.png"), "x").unwrap();
        std::fs::write(dir.join("src/cover.png"), "x").unwrap();
        write(
            &dir,
            "renzora.toml",
            "name = \"x\"\nversion = \"1.0.0\"\ndescription = \"d\"\ncategory = \"3d-models\"\nmarketplace_id = \"test-id\"\n\
             thumbnail = \"src/cover.png\"\n",
        );
        let m = load(&dir).unwrap();
        assert_eq!(m.thumbnail, Some(dir.join("src/cover.png")));
    }

    #[test]
    fn standalone_manifest_needs_no_crate() {
        let dir = tmp("standalone");
        write(
            &dir,
            "renzora.toml",
            r#"
name = "Forest Pack"
version = "1.2.0"
description = "Twelve trees"
category = "3d-models"
marketplace_id = "test-id"
"#,
        );
        let m = load(&dir).unwrap();
        assert_eq!(m.name, "Forest Pack");
        assert_eq!(
            m.licence, None,
            "an unstated licence stays unstated, so an update cannot overwrite one"
        );
        assert_eq!(m.zip_action, "extract", "content is unpacked so it can be browsed");
    }

    #[test]
    fn version_is_inherited_from_the_workspace() {
        let root = tmp("workspace");
        write(
            &root,
            "Cargo.toml",
            "[workspace]\nmembers = [\"p\"]\n\n[workspace.package]\nversion = \"2.1.0\"\n",
        );
        let crate_dir = root.join("p");
        std::fs::create_dir_all(&crate_dir).unwrap();
        write(
            &crate_dir,
            "Cargo.toml",
            r#"
[package]
name = "p"
version.workspace = true
description = "d"

[package.metadata.renzora]
category = "plugins"
marketplace_id = "test-id"
"#,
        );
        let m = load(&crate_dir).unwrap();
        assert_eq!(m.version, "2.1.0");
    }

    #[test]
    fn a_listing_without_an_id_is_refused() {
        let dir = tmp("claim");
        write(
            &dir,
            "Cargo.toml",
            "[package]\nname = \"system_monitor\"\nversion = \"1.0.1\"\n\
             description = \"d\"\n\n[package.metadata.renzora]\ncategory = \"plugins\"\n",
        );
        let err = load(&dir).unwrap_err();
        assert!(err.contains("does not set `marketplace_id`"), "{err}");
        // The message writes the line out rather than describing it.
        assert!(err.contains("marketplace_id = "), "{err}");
    }

    #[test]
    fn a_claim_can_be_stated_outright() {
        let dir = tmp("claim-explicit");
        write(
            &dir,
            "Cargo.toml",
            "[package]\nname = \"sm\"\nversion = \"1.0.1\"\ndescription = \"d\"\n\n\
             [package.metadata.renzora]\ncategory = \"plugins\"\n\
             marketplace_id = \"system-monitor\"\n",
        );
        assert_eq!(load(&dir).unwrap().marketplace_id, "system-monitor");
    }

    #[test]
    fn a_claim_the_marketplace_would_reject_fails_here_first() {
        let dir = tmp("claim-bad");
        write(
            &dir,
            "Cargo.toml",
            "[package]\nname = \"sm\"\nversion = \"1.0.1\"\ndescription = \"d\"\n\n\
             [package.metadata.renzora]\ncategory = \"plugins\"\n\
             marketplace_id = \"-nope!\"\n",
        );
        let err = load(&dir).unwrap_err();
        assert!(err.contains("may only contain"), "{err}");
    }

    #[test]
    fn an_engine_floor_lands_where_the_engine_reads_it() {
        let dir = tmp("engine");
        // Not the plugins category: a floor is not plugin-only, and a plugin
        // would need a crate here this test has no reason to write.
        write(
            &dir,
            "renzora.toml",
            "name = \"x\"\nversion = \"1.0.0\"\ndescription = \"d\"\n\
             category = \"3d-models\"\nmarketplace_id = \"test-id\"\nmin_engine_version = \"r1-alpha8\"\n",
        );
        let m = load(&dir).unwrap();
        assert_eq!(m.min_engine_version.as_deref(), Some("r1-alpha8"));
        assert_eq!(
            m.metadata["min_engine_version"], "r1-alpha8",
            "the editor and the website both read it out of metadata"
        );
    }

    #[test]
    fn an_unorderable_engine_tag_is_refused() {
        let dir = tmp("engine-bad");
        // `engine_satisfies` would parse no number out of this and quietly stop
        // constraining anything.
        write(
            &dir,
            "renzora.toml",
            "name = \"x\"\nversion = \"1.0.0\"\ndescription = \"d\"\n\
             category = \"plugins\"\nmarketplace_id = \"test-id\"\nmin_engine_version = \"latest\"\n",
        );
        let err = load(&dir).unwrap_err();
        assert!(err.contains("does not end in a number"), "{err}");
    }

    #[test]
    fn a_nightly_is_not_a_floor() {
        let dir = tmp("engine-nightly");
        write(
            &dir,
            "renzora.toml",
            "name = \"x\"\nversion = \"1.0.0\"\ndescription = \"d\"\n\
             category = \"plugins\"\nmarketplace_id = \"test-id\"\nmin_engine_version = \"r1-alpha8-nightly-16aug26\"\n",
        );
        let err = load(&dir).unwrap_err();
        assert!(err.contains("nightly"), "{err}");
    }

    #[test]
    fn rejects_what_the_server_would_reject() {
        let dir = tmp("invalid");
        write(
            &dir,
            "renzora.toml",
            r#"
name = "x"
version = "1.0.0"
description = "d"
category = "3d-models"
marketplace_id = "test-id"
licence = "wtfpl"
"#,
        );
        let err = load(&dir).unwrap_err();
        assert!(err.contains("unknown licence"), "{err}");
    }

    #[test]
    fn missing_category_explains_itself() {
        let dir = tmp("nocat");
        write(
            &dir,
            "renzora.toml",
            "name = \"x\"\nversion = \"1.0.0\"\ndescription = \"d\"\n",
        );
        let err = load(&dir).unwrap_err();
        assert!(err.contains("category"), "{err}");
    }
}
