//! `renzora publish` — push a directory to the Renzora marketplace.
//!
//! The model is crates.io's: the directory carries its own manifest, you mint a
//! token once, and `publish` decides by itself whether this is a new listing or
//! a new version of one you already own. A version is never overwritten —
//! publishing 0.3.0 leaves 0.2.0 downloadable for everyone who already has it,
//! because the server keeps releases rather than replacing an asset's files.
//!
//! A directory is matched to a listing by the `marketplace_id` its manifest
//! claims — unique marketplace-wide, claimed on first publish, never changed.
//! One identifier, three answers: free, yours, or somebody else's. Matching on
//! a title instead would be guessing, since a listing is named by a person and
//! renamed later (`crt` is listed as "CRT Fx").

pub mod checkout;
pub mod credentials;
pub mod manifest;
pub mod package;
pub mod registry;

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use checkout::Checkout;
use credentials::Credentials;
use manifest::Manifest;
use registry::{Multipart, Registry};

/// Width cargo uses for its right-aligned status verbs; matching it makes the
/// two tools' output sit together in one terminal without looking foreign.
const VERB: usize = 12;

fn status(verb: &str, message: impl std::fmt::Display) {
    println!("{verb:>VERB$} {message}");
}

// ── renzora login ──────────────────────────────────────────────────────────

pub fn login(url: Option<String>) -> Result<(), String> {
    let site = url
        .unwrap_or_else(|| credentials::DEFAULT_SITE.to_string())
        .trim_end_matches('/')
        .to_string();

    println!("Create an API token at {site}/developers, then paste it here.");
    println!("(The token is stored in {}.)", credentials::path()?.display());
    print!("token: ");
    let _ = std::io::stdout().flush();

    let mut token = String::new();
    std::io::stdin()
        .read_line(&mut token)
        .map_err(|e| format!("could not read the token: {e}"))?;
    let token = token.trim().to_string();

    if token.is_empty() {
        return Err("no token entered".into());
    }
    if !token.starts_with("rz_") {
        return Err(format!(
            "that does not look like an API token — they start with `rz_`.\n\
             A `rza_` token belongs to a developer app and cannot publish; the \
             one you want is under \"API Tokens\" at {site}/developers."
        ));
    }

    let creds = Credentials { url: site, token };
    // Verified before it is written, so a mistyped token fails here rather
    // than the next time someone tries to ship something.
    let me = Registry::new(&creds).me()?;
    let path = credentials::save(&creds)?;

    status("Logged in", format!("as {} ({})", me.username, creds.url));
    if !cfg!(unix) {
        // Nothing restricts the file beyond the user profile's own ACL here,
        // and a publish token is worth saying that out loud.
        println!("{:>VERB$} {} holds your token in plain text.", "Note", path.display());
    }
    Ok(())
}

pub fn logout() -> Result<(), String> {
    match credentials::delete()? {
        Some(path) => {
            status("Logged out", format!("removed {}", path.display()));
            println!(
                "{:>VERB$} the token itself stays valid until you revoke it at {}/developers.",
                "Note",
                credentials::DEFAULT_SITE
            );
        }
        None => status("Logged out", "there were no stored credentials"),
    }
    Ok(())
}

pub fn whoami() -> Result<(), String> {
    let creds = credentials::load()?;
    let me = Registry::new(&creds).me()?;
    println!("{} ({}) at {}", me.username, me.role, creds.url);
    Ok(())
}

// ── renzora publish ────────────────────────────────────────────────────────

pub struct PublishArgs {
    pub paths: Vec<String>,
    /// Publish from this git repository instead of from the working directory.
    pub repo: Option<String>,
    /// Which branch, tag or commit of it. `None` is the default branch.
    pub git_ref: Option<String>,
    pub all: bool,
    pub thumbnails: bool,
    pub dry_run: bool,
    pub allow_older: bool,
    pub list_categories: bool,
    pub notes: Option<String>,
    pub notes_file: Option<String>,
    pub out: Option<String>,
    pub yes: bool,
}

pub fn publish(args: PublishArgs) -> Result<(), String> {
    if args.list_categories {
        return list_categories();
    }

    // Held for the whole run: dropping it deletes the checkout, so it has to
    // outlive the packaging that reads from it.
    let checkout = match &args.repo {
        Some(url) => {
            status("Fetching", url);
            let checkout = Checkout::fetch(url, args.git_ref.as_deref())?;
            status("Fetched", &checkout.commit[..12.min(checkout.commit.len())]);
            Some(checkout)
        }
        None => None,
    };

    // With a repository, a path on the command line names a directory *inside*
    // it — `--repo <url> clouds` is the repository's `clouds`, not a local one.
    let paths: Vec<String> = match &checkout {
        Some(c) if args.paths.is_empty() => vec![c.dir.to_string_lossy().to_string()],
        Some(c) => args.paths.iter().map(|p| c.resolve(p)).collect(),
        None => args.paths.clone(),
    };

    let targets = resolve_targets(&paths, args.all)?;

    if args.thumbnails {
        return sync_thumbnails(&targets, args.yes);
    }

    match targets.as_slice() {
        [] => Err("nothing to publish".into()),
        [one] => publish_one(one.clone(), &args),
        many => publish_many(many, &args),
    }
}

/// Replace listings' cover images and nothing else.
///
/// A cover is not a new version of a plugin, and a published version can never
/// be replaced — so pushing one as a release would mean bumping the version of
/// everything that gained a picture, and leaving a release behind whose only
/// change was the picture. `PUT /:id/files` takes a thumbnail on its own and
/// only touches an asset's files when the request carries some.
fn sync_thumbnails(dirs: &[PathBuf], yes: bool) -> Result<(), String> {
    let creds = credentials::load()?;
    let registry = Registry::new(&creds);
    let mine = registry.my_assets()?;

    // Everything is resolved before anything is sent, so a directory with no
    // listing or no image is reported rather than discovered halfway through.
    let mut ready: Vec<(Manifest, registry::Asset, PathBuf)> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();

    for dir in dirs {
        let manifest = manifest::load(dir)?;
        let Some(path) = manifest.thumbnail.clone() else {
            skipped.push(format!("{} has no thumbnail", manifest.marketplace_id));
            continue;
        };
        match mine
            .iter()
            .find(|a| a.marketplace_id.eq_ignore_ascii_case(&manifest.marketplace_id))
        {
            Some(asset) => ready.push((manifest, asset.clone(), path)),
            None => skipped.push(format!(
                "{} is not one of your listings yet — publish it first",
                manifest.marketplace_id
            )),
        }
    }

    for reason in &skipped {
        println!("{:>VERB$} {reason}", "Skipped");
    }
    if ready.is_empty() {
        return Err("nothing to upload".into());
    }

    let total: u64 = ready
        .iter()
        .filter_map(|(_, _, p)| std::fs::metadata(p).ok().map(|m| m.len()))
        .sum();
    status(
        "Uploading",
        format!("{} covers, {}", ready.len(), package::human_size(total)),
    );
    if !confirm(yes, &format!("Replace {} cover images?", ready.len()))? {
        return Err("cancelled".into());
    }

    let mut failed = Vec::new();
    for (manifest, asset, path) in &ready {
        let name = filename(path);
        match read(path).and_then(|bytes| registry.update_thumbnail(&asset.id, &name, &bytes)) {
            Ok(()) => println!("{:>VERB$} {:<24} {name}", "Replaced", asset.name),
            Err(e) => {
                println!("{:>VERB$} {:<24} {e}", "Failed", asset.name);
                failed.push(manifest.marketplace_id.clone());
            }
        }
    }

    println!();
    status(
        "Finished",
        format!(
            "{} replaced, {} failed",
            ready.len() - failed.len(),
            failed.len()
        ),
    );
    if failed.is_empty() {
        return Ok(());
    }
    Err(format!("these did not upload: {}", failed.join(", ")))
}

/// Every directory to publish, from the paths given.
///
/// `--all` turns each path into its manifest-bearing children rather than the
/// path itself, which is how one command covers an editor's whole `plugins/`
/// directory. It is a flag rather than the default because `renzora publish`
/// inside a directory that happens to contain other crates should publish that
/// directory, not everything beneath it.
fn resolve_targets(paths: &[String], all: bool) -> Result<Vec<PathBuf>, String> {
    let roots: Vec<PathBuf> = if paths.is_empty() {
        vec![resolve_dir(None)?]
    } else {
        paths
            .iter()
            .map(|p| resolve_dir(Some(p)))
            .collect::<Result<_, _>>()?
    };

    if !all {
        return Ok(roots);
    }

    let mut found = Vec::new();
    for root in &roots {
        let entries = std::fs::read_dir(root)
            .map_err(|e| format!("could not read {}: {e}", root.display()))?;
        let all_dirs: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir() && has_manifest(p))
            .collect();

        // A sweep takes only what has claimed an id. Naming a directory
        // outright still publishes it or explains why it cannot — saying so is
        // an instruction, where sweeping a folder is not.
        let mut children: Vec<PathBuf> = all_dirs
            .iter()
            .filter(|p| manifest::declares_listing(p))
            .cloned()
            .collect();
        let passed_over = all_dirs.len() - children.len();
        if passed_over > 0 {
            println!(
                "{:>VERB$} {passed_over} of {} directories claim no marketplace id",
                "Passed over",
                all_dirs.len()
            );
        }
        if children.is_empty() {
            return Err(format!(
                "nothing to publish under {} — --all looks one level \
                 down for a directory whose manifest sets `marketplace_id`.",
                root.display()
            ));
        }
        children.sort();
        found.append(&mut children);
    }
    Ok(found)
}

fn has_manifest(dir: &Path) -> bool {
    dir.join("Cargo.toml").is_file() || dir.join("renzora.toml").is_file()
}

/// Publish one directory — the whole of what `renzora publish <dir>` does.
fn publish_one(dir: PathBuf, args: &PublishArgs) -> Result<(), String> {
    let manifest = manifest::load(&dir)?;

    status(
        "Packaging",
        format!("{} v{} ({})", manifest.name, manifest.version, dir.display()),
    );
    let package = package::build(&manifest)?;
    status(
        "Packaged",
        format!(
            "{} files, {} ({} uncompressed)",
            package.files.len(),
            package::human_size(package.len() as u64),
            package::human_size(package.uncompressed)
        ),
    );

    if let Some(out) = &args.out {
        std::fs::write(out, &package.bytes)
            .map_err(|e| format!("could not write {out}: {e}"))?;
        status("Wrote", out);
    }

    if args.dry_run {
        for file in &package.files {
            println!("{:>VERB$} {file}", "");
        }
        return dry_run(
            &manifest,
            args.notes.as_deref(),
            args.notes_file.as_deref(),
            args.allow_older,
        );
    }

    let creds = credentials::load()?;
    let registry = Registry::new(&creds);
    let catalog = Catalog::fetch(&registry)?;
    ship(&registry, &catalog, &manifest, &package, args, args.yes)
}

/// Publish one already-packaged directory against a catalog someone else
/// fetched. The tail shared by a single publish and every step of a batch.
fn ship(
    registry: &Registry,
    catalog: &Catalog,
    manifest: &Manifest,
    package: &package::Package,
    args: &PublishArgs,
    yes: bool,
) -> Result<(), String> {
    if let Some(problem) = category_error(&catalog.categories, manifest) {
        return Err(problem);
    }
    if let Some(problem) = engine_version_error(&catalog.engine_versions, manifest) {
        return Err(problem);
    }

    match catalog.find_listing(registry, manifest)? {
        None => {
            create_listing(registry, manifest, package, yes)
        }
        Some(asset) => {
            check_version(registry, manifest, &asset, args.allow_older)?;
            let notes =
                release_notes(manifest, args.notes.as_deref(), args.notes_file.as_deref())?;
            publish_release(registry, manifest, package, &asset, &notes, yes)
        }
    }
}

// ── Batch ──────────────────────────────────────────────────────────────────

/// Publish several directories in one run.
///
/// Everything is packaged and checked *before* anything is uploaded. A broken
/// manifest in the sixtieth plugin should not be discovered after fifty-nine
/// listings already exist, and unlike a single publish there is no way to
/// simply run it again — the ones that went up are up.
fn publish_many(dirs: &[PathBuf], args: &PublishArgs) -> Result<(), String> {
    if args.out.is_some() {
        return Err("--out writes one archive, so it cannot be used with several \
                    directories."
            .into());
    }

    status("Preparing", format!("{} directories", dirs.len()));
    let prepared = prepare(dirs)?;

    if args.dry_run {
        return batch_dry_run(&prepared, args);
    }

    let creds = credentials::load()?;
    let registry = Registry::new(&creds);
    let catalog = Catalog::fetch(&registry)?;

    if !confirm(
        args.yes,
        &format!(
            "Publish {} directories to {}?",
            prepared.len(),
            registry.site()
        ),
    )? {
        return Err("cancelled".into());
    }

    // Past the confirmation each one is independent: a failure is reported and
    // the rest still go. Stopping at the first would leave the set half
    // published with no record of where it got to.
    let mut failed = Vec::new();
    for (manifest, package) in &prepared {
        println!();
        status("Publishing", format!("{} v{}", manifest.name, manifest.version));
        // `yes` is true here: the batch was confirmed once, as a batch.
        if let Err(e) = ship(&registry, &catalog, manifest, package, args, true) {
            println!("{:>VERB$} {e}", "Failed");
            failed.push(manifest.name.clone());
        }
    }

    println!();
    status(
        "Finished",
        format!(
            "{} published, {} failed",
            prepared.len() - failed.len(),
            failed.len()
        ),
    );
    if failed.is_empty() {
        return Ok(());
    }
    Err(format!("these did not publish: {}", failed.join(", ")))
}

/// Load and package every directory, refusing the whole run if any is unfit.
fn prepare(dirs: &[PathBuf]) -> Result<Vec<(Manifest, package::Package)>, String> {
    let mut prepared: Vec<(Manifest, package::Package)> = Vec::new();
    let mut problems = Vec::new();

    for dir in dirs {
        match manifest::load(dir).and_then(|m| package::build(&m).map(|p| (m, p))) {
            Ok(pair) => prepared.push(pair),
            Err(e) => problems.push(e),
        }
    }

    // Two directories publishing under one name would not collide on the
    // server — it would simply make two listings — which is exactly why it has
    // to be caught here.
    for (i, (a, _)) in prepared.iter().enumerate() {
        for (b, _) in prepared.iter().skip(i + 1) {
            if a.marketplace_id.eq_ignore_ascii_case(&b.marketplace_id) {
                problems.push(format!(
                    "{} and {} both claim the marketplace id `{}`. An id \
                     belongs to one listing, so one of them has to change.",
                    a.source.display(),
                    b.source.display(),
                    a.marketplace_id
                ));
            }
        }
    }

    if problems.is_empty() {
        return Ok(prepared);
    }
    Err(format!(
        "{} of {} directories cannot be published, so none were:{}{}",
        problems.len(),
        dirs.len(),
        NEWLINE,
        problems.join(NEWLINE)
    ))
}

const NEWLINE: &str = "\n";

/// What a batch would do, one line each.
fn batch_dry_run(prepared: &[(Manifest, package::Package)], args: &PublishArgs) -> Result<(), String> {
    let total: u64 = prepared.iter().map(|(_, p)| p.len() as u64).sum();
    status(
        "Packaged",
        format!("{} directories, {}", prepared.len(), package::human_size(total)),
    );

    let Ok(creds) = credentials::load() else {
        for (manifest, _) in prepared {
            println!("{:>VERB$} {} v{}", "", manifest.name, manifest.version);
        }
        println!(
            "{:>VERB$} not logged in, so no listing was checked — run \
             `renzora login` to see what publishing would do.",
            "Skipped"
        );
        return finish_dry_run(None);
    };

    let registry = Registry::new(&creds);
    let Ok(catalog) = Catalog::fetch(&registry) else {
        return finish_dry_run(Some("the listings were not checked — the marketplace \
                                    could not be reached."));
    };

    let mut create = 0;
    let mut update = 0;
    let mut problems = Vec::new();

    for (manifest, _) in prepared {
        let verdict = batch_verdict(&registry, &catalog, manifest, args);
        match verdict {
            Ok(line) => {
                if line.starts_with("new") {
                    create += 1;
                } else {
                    update += 1;
                }
                println!("{:>VERB$} {:<28} {line}", "", manifest.name);
            }
            Err(e) => {
                problems.push(manifest.name.clone());
                println!("{:>VERB$} {:<28} {}", "Problem", manifest.name, first_line(&e));
            }
        }
    }

    println!();
    status(
        "Would",
        format!("create {create} listings, update {update}, and refuse {}", problems.len()),
    );
    finish_dry_run(None)?;

    // A dry run reporting refusals has found real problems, and a script
    // gating a publish on it should see that in the exit code rather than
    // having to read the summary.
    if problems.is_empty() {
        return Ok(());
    }
    Err(format!(
        "{} of {} would not publish: {}",
        problems.len(),
        prepared.len(),
        problems.join(", ")
    ))
}

/// One plugin's outcome in a batch dry run, as a short phrase.
fn batch_verdict(
    registry: &Registry,
    catalog: &Catalog,
    manifest: &Manifest,
    args: &PublishArgs,
) -> Result<String, String> {
    if let Some(problem) = category_error(&catalog.categories, manifest) {
        return Err(problem);
    }
    if let Some(problem) = engine_version_error(&catalog.engine_versions, manifest) {
        return Err(problem);
    }
    match catalog.find_listing(registry, manifest)? {
        None => Ok(format!(
            "new listing at v{}, claiming `{}`",
            manifest.version, manifest.marketplace_id
        )),
        Some(asset) => {
            check_version(registry, manifest, &asset, args.allow_older)?;
            Ok(format!(
                "release v{} onto \"{}\" (at v{})",
                manifest.version, asset.name, asset.version
            ))
        }
    }
}

fn first_line(message: &str) -> &str {
    message.lines().next().unwrap_or(message)
}

/// The marketplace facts a publish checks against, fetched once.
///
/// A batch of seventy asks the same three questions seventy times otherwise,
/// which is both slow and a real amount of an account's 500-a-day budget.
struct Catalog {
    categories: Vec<registry::Category>,
    /// Empty when the marketplace does not serve the list — the engine-floor
    /// check treats that as "cannot tell" rather than as a failure.
    engine_versions: Vec<registry::EngineVersion>,
    mine: Vec<registry::Asset>,
}

impl Catalog {
    fn fetch(registry: &Registry) -> Result<Self, String> {
        Ok(Self {
            categories: registry.categories()?,
            engine_versions: registry.engine_versions().unwrap_or_default(),
            mine: registry.my_assets()?,
        })
    }
}


/// Where an engine tag sits in the ordering, by its trailing digits.
///
/// `r1-alpha8` -> `("r1-alpha", 8)`, so `r1-alpha10` sorts above `r1-alpha7`,
/// which a string compare gets backwards. `None` for a tag with no trailing
/// number, which callers read as "cannot tell" and never as a failure.
///
/// The same split the editor makes in `installed::release_order`. It has to be,
/// or the tool and the thing it publishes for would disagree about which of two
/// engines is newer.
fn engine_order(tag: &str) -> Option<(String, u32)> {
    let tag = tag.trim();
    if tag.is_empty() {
        return None;
    }
    let digits_start = tag
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_ascii_digit())
        .map(|(i, _)| i)
        .last()?;
    let (prefix, num) = tag.split_at(digits_start);
    num.parse::<u32>().ok().map(|n| (prefix.to_string(), n))
}

/// Is engine `a` strictly newer than engine `b`?
///
/// An empty tag means "any engine", which is the OLDEST possible line: a
/// release with no floor is offered to everybody, including the oldest editor
/// anybody is running.
fn engine_newer(a: &str, b: &str) -> bool {
    match (engine_order(a), engine_order(b)) {
        (Some((ap, an)), Some((bp, bn))) if ap == bp => an > bn,
        // Any real tag is newer than "no floor at all", which is the oldest
        // line there is. A tag that simply will not parse is left alone rather
        // than guessed at, so an unfamiliar scheme never blocks a publish.
        (Some(_), None) => b.trim().is_empty(),
        _ => false,
    }
}

/// Refuse a version the listing cannot sensibly take.
///
/// Two separate problems, and the server only catches one of them. It rejects a
/// version that already exists — but only once the archive has been uploaded,
/// so the same check here saves the upload. What it does *not* check is
/// ordering, and ordering is what decides which release an engine resolves.
///
/// The rule that matters is not "newer than the newest", it is **newer than the
/// newest on your own line, and older than everything on any newer line**.
///
/// Resolution picks the highest version among releases an engine can run, so a
/// release built for r1-alpha7 with a version ABOVE an r1-alpha8 release wins
/// for r1-alpha8 users too: they can run both, and yours sorts higher. That
/// hands the newer engine the older code, silently, and it is the one mistake
/// this arrangement makes easy. Publishing 1.0.11 on the r1-alpha7 line while
/// 2.0.0 sits on r1-alpha8 is fine; publishing 3.0.0 there is not.
///
/// Publishing below the newest version overall stopped being suspicious the day
/// compatibility moved onto the release, so `--allow-older` is no longer needed
/// for the ordinary "fix an old line" case. It still covers going behind on
/// your OWN line, which remains a real mistake.
fn check_version(
    registry: &Registry,
    manifest: &Manifest,
    asset: &registry::Asset,
    allow_older: bool,
) -> Result<(), String> {
    let releases = registry.releases(&asset.id)?;

    if releases.iter().any(|r| r.version == manifest.version) {
        return Err(format!(
            "v{} has already been published for \"{}\".\n\
             Bump the version in {} — a published version is never replaced, so \
             that everyone who already downloaded it keeps what they have.",
            manifest.version,
            asset.name,
            manifest.source.display()
        ));
    }

    let mine = manifest.min_engine_version.as_deref().unwrap_or("");

    // A marketplace that predates per-release compatibility reports no engine on
    // any release, so every release is on one line and the old whole-listing
    // comparison is the only correct one.
    let per_release = releases.iter().any(|r| !r.min_engine_version.is_empty());
    if !per_release {
        let current = releases
            .iter()
            .find(|r| r.is_current)
            .map(|r| r.version.as_str())
            .unwrap_or(asset.version.as_str());
        if allow_older || crate::version_gt(&manifest.version, current) {
            return Ok(());
        }
        return Err(format!(
            "v{} is behind \"{}\"'s current v{current}, and publishing it would make \
             it the version buyers get.\n\
             Bump the version in {}, or pass --allow-older if you mean to publish \
             behind the current release.",
            manifest.version,
            asset.name,
            manifest.source.display()
        ));
    }

    // Nothing on a newer line may be at or below this version, or that line's
    // users start resolving this release instead of their own.
    if let Some(clash) = releases
        .iter()
        .filter(|r| engine_newer(&r.min_engine_version, mine))
        .find(|r| !crate::version_gt(&r.version, &manifest.version))
    {
        return Err(format!(
            "v{} is at or above v{} on the {} line, which is newer than the {} \
             line you are publishing to.\n\
             Editors on {} can run both releases and take whichever has the \
             higher version, so publishing this would hand them the {} code.\n\
             Use a version below v{} in {}, or publish this to {} instead.",
            manifest.version,
            clash.version,
            clash.min_engine_version,
            engine_label(mine),
            clash.min_engine_version,
            engine_label(mine),
            clash.version,
            manifest.source.display(),
            clash.min_engine_version,
        ));
    }

    // Within this line, and the older ones it inherits, the usual rule holds:
    // an editor here resolves the highest version it can run, so anything not
    // above that is a release nobody would ever be given.
    let highest_here = releases
        .iter()
        .filter(|r| !engine_newer(&r.min_engine_version, mine))
        .map(|r| r.version.as_str())
        .fold(None::<&str>, |best, v| match best {
            Some(b) if !crate::version_gt(v, b) => Some(b),
            _ => Some(v),
        });

    match highest_here {
        Some(newest) if !allow_older && !crate::version_gt(&manifest.version, newest) => {
            Err(format!(
                "v{} is behind v{newest} on the {} line, so editors there would go \
                 on getting v{newest} and never see this.\n\
                 Bump the version in {}, or pass --allow-older if you mean to \
                 publish behind it.",
                manifest.version,
                engine_label(mine),
                manifest.source.display()
            ))
        }
        _ => Ok(()),
    }
}

/// How to name an engine line in a message, including the one with no floor.
fn engine_label(tag: &str) -> String {
    if tag.trim().is_empty() {
        "any engine".to_string()
    } else {
        tag.to_string()
    }
}

/// Everything `publish` would do, stopping short of the upload.
///
/// The package has already been built and listed by the time this runs, so what
/// is left is the half that needs the marketplace: whether the category exists,
/// whether this directory is a new listing or another version of one you own,
/// and whether that version is still free. Those are precisely the checks that
/// otherwise fail *after* the archive has gone up the wire, which is what makes
/// them worth making here.
///
/// It still works with no marketplace to ask. Without a token, or without a
/// route to the server, it reports which checks it could not make and succeeds —
/// a dry run that failed because you happen to be on a train would be a worse
/// tool than one that tells you the package itself is sound.
fn dry_run(
    manifest: &Manifest,
    notes: Option<&str>,
    notes_file: Option<&str>,
    allow_older: bool,
) -> Result<(), String> {
    let Ok(creds) = credentials::load() else {
        return finish_dry_run(Some(
            "not logged in, so the listing itself was not checked — run \
             `renzora login` to see what publishing would do.",
        ));
    };

    let registry = Registry::new(&creds);
    // The first request doubles as the reachability test: an unreachable
    // marketplace or a stale token is a fact about the network rather than
    // about the package, and the run should say so instead of failing.
    let categories = match registry.categories() {
        Ok(categories) => categories,
        Err(e) => return finish_dry_run(Some(&format!("the listing was not checked — {e}"))),
    };

    // From here the marketplace is answering, so whatever it rejects is a real
    // problem with the manifest and fails the run.
    if let Some(problem) = category_error(&categories, manifest) {
        return Err(problem);
    }
    let engine_versions = registry.engine_versions().unwrap_or_default();
    if let Some(problem) = engine_version_error(&engine_versions, manifest) {
        return Err(problem);
    }

    let catalog = Catalog {
        categories,
        engine_versions,
        mine: registry.my_assets()?,
    };
    let existing = catalog.find_listing(&registry, manifest)?;
    if let Some(asset) = &existing {
        check_version(&registry, manifest, asset, allow_older)?;
    }

    for line in plan(manifest, existing.as_ref()) {
        println!("{:>VERB$} {line}", "Would");
    }
    if let Some(asset) = &existing {
        let notes = release_notes(manifest, notes, notes_file)?;
        println!("{:>VERB$} {}", "Notes", describe_notes(&notes));
        // Printed bare, the way a finished publish prints it.
        println!("{:>VERB$} {}", "", registry.asset_url(&asset.slug));
    }

    finish_dry_run(None)
}

fn finish_dry_run(skipped: Option<&str>) -> Result<(), String> {
    if let Some(reason) = skipped {
        println!("{:>VERB$} {reason}", "Skipped");
    }
    status("Dry run", "nothing was uploaded");
    Ok(())
}

/// What publishing would do, in the order it would do it.
fn plan(manifest: &Manifest, existing: Option<&registry::Asset>) -> Vec<String> {
    let mut lines = Vec::new();
    match existing {
        None => {
            lines.push(format!(
                "create a new listing \"{}\" v{} in {}",
                manifest.name, manifest.version, manifest.category
            ));
            lines.push(format!(
                "claim the marketplace id `{}`",
                manifest.marketplace_id
            ));
            if let Some(media) = describe_media(manifest) {
                lines.push(format!("upload {media} alongside it"));
            }
            lines.extend(describe_engine_floor(manifest));
        }
        Some(asset) => {
            lines.push(format!(
                "publish v{} to \"{}\", which is at v{}",
                manifest.version, asset.name, asset.version
            ));
            lines.push(format!(
                "take the description, tags, licence and price from {}",
                manifest.source.display()
            ));
            lines.push(format!("leave v{} downloadable", asset.version));
            if let Some(path) = &manifest.thumbnail {
                lines.push(format!("replace the cover with {}", filename(path)));
            }
            lines.extend(describe_engine_floor(manifest));
        }
    }
    lines
}

/// The engine floor, which decides who is *offered* the plugin at all — the
/// editor hides an update from anyone running older than this. Worth saying
/// out loud in a dry run, because the consequence of getting it wrong is
/// silent: the plugin simply stops appearing for people.
fn describe_engine_floor(manifest: &Manifest) -> Option<String> {
    manifest
        .min_engine_version
        .as_ref()
        .map(|tag| format!("require engine {tag} or newer"))
}

/// The gallery that only a *new* listing carries up with it — a release
/// uploads files and nothing else.
fn describe_media(manifest: &Manifest) -> Option<String> {
    let mut parts = Vec::new();
    if manifest.thumbnail.is_some() {
        parts.push("a thumbnail".to_string());
    }
    match manifest.screenshots.len() {
        0 => {}
        1 => parts.push("1 screenshot".to_string()),
        n => parts.push(format!("{n} screenshots")),
    }
    if manifest.video.is_some() {
        parts.push("a video".to_string());
    }
    if manifest.audio.is_some() {
        parts.push("an audio preview".to_string());
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// Release notes are easy to get silently wrong — an empty CHANGELOG section, or
/// the whole file where one entry was meant — so the run shows what it found
/// rather than only that it found something.
fn describe_notes(notes: &str) -> String {
    let trimmed = notes.trim();
    if trimmed.is_empty() {
        return "none — pass --notes, or add this version to CHANGELOG.md".to_string();
    }
    let mut lines = trimmed.lines();
    let first = lines.next().unwrap_or_default();
    let first = if first.chars().count() > 60 {
        format!("{}...", first.chars().take(60).collect::<String>())
    } else {
        first.to_string()
    };
    match trimmed.lines().count() {
        1 => first,
        n => format!("{first} (+{} more lines)", n - 1),
    }
}

/// Create the listing, its first release and its gallery in one request.
fn create_listing(
    registry: &Registry,
    manifest: &Manifest,
    package: &package::Package,
    yes: bool,
) -> Result<(), String> {
    if !confirm(
        yes,
        &format!(
            "Create a new listing \"{}\" v{} in {} at {}?",
            manifest.name,
            manifest.version,
            manifest.category,
            registry.site()
        ),
    )? {
        return Err("cancelled".into());
    }

    let mut body = Multipart::new();
    body.text("metadata", &upload_metadata(manifest)?);
    body.file("file", &manifest.archive_name(), &package.bytes);
    attach_media(&mut body, manifest)?;

    status("Uploading", format!("{} v{}", manifest.name, manifest.version));
    let asset = registry.upload(body)?;

    status("Published", format!("{} v{}", asset.name, asset.version));
    println!("{:>VERB$} {}", "", registry.asset_url(&asset.slug));
    Ok(())
}

/// Add a version to a listing that already exists, then bring its metadata
/// back in line with the manifest.
fn publish_release(
    registry: &Registry,
    manifest: &Manifest,
    package: &package::Package,
    asset: &registry::Asset,
    notes: &str,
    yes: bool,
) -> Result<(), String> {
    if asset.category != manifest.category {
        println!(
            "{:>VERB$} the listing is in `{}` but the manifest says `{}`; a \
             release cannot move it, so it stays in `{}`.",
            "Warning", asset.category, manifest.category, asset.category
        );
    }

    if !confirm(
        yes,
        &format!(
            "Publish {} v{} (the listing is at v{})?",
            asset.name, manifest.version, asset.version
        ),
    )? {
        return Err("cancelled".into());
    }

    let mut body = Multipart::new();
    body.text(
        "metadata",
        &serde_json::to_string(&serde_json::json!({
            "version": manifest.version,
            "notes": notes,
            "zip_action": manifest.zip_action,
            // Stamped on the release, not just the listing. The listing holds
            // one value and can therefore only ever describe the newest
            // release, which is what used to cut older-engine users off from
            // the release that still worked for them. A marketplace that does
            // not know the field falls back to the listing's, so sending it
            // costs nothing against an older one.
            "min_engine_version": manifest.min_engine_version.clone().unwrap_or_default(),
        }))
        .map_err(|e| e.to_string())?,
    );
    body.file("file", &manifest.archive_name(), &package.bytes);

    status(
        "Uploading",
        format!("{} v{} to an existing listing", manifest.name, manifest.version),
    );
    let release = registry.create_release(&asset.id, body)?;

    // The manifest is the source of truth for everything but the files, so a
    // description or price edited there lands with the release rather than
    // waiting for someone to also change it in the web UI.
    registry.update_asset(&asset.id, listing_fields(manifest, Some(&asset.metadata)))?;

    // The release endpoint takes files and nothing else, so the cover is
    // refreshed separately — otherwise a plugin that gained a thumbnail after
    // its first publish could never show one without visiting the website.
    if let Some(path) = &manifest.thumbnail {
        registry.update_thumbnail(&asset.id, &filename(path), &read(path)?)?;
        status("Thumbnail", filename(path));
    }
    if !manifest.screenshots.is_empty() || manifest.video.is_some() || manifest.audio.is_some() {
        println!(
            "{:>VERB$} the gallery is left alone on a release — re-sending it \
             would add copies rather than replace it. Edit it on the asset's page.",
            "Note"
        );
    }

    status("Published", format!("{} v{}", asset.name, release.version));
    println!("{:>VERB$} {}", "", registry.asset_url(&asset.slug));
    Ok(())
}

// ── Matching this directory to a listing ───────────────────────────────────

/// Find the listing this directory publishes to, if it already exists.
impl Catalog {
/// Which listing this directory publishes to, if any.
///
/// One question, asked of one identifier. The id is either unclaimed — a new
/// listing — or held by exactly one listing, which is either yours or somebody
/// else's. Nothing to disambiguate and nothing to guess at, which is the whole
/// reason the id exists.
fn find_listing(
    &self,
    registry: &Registry,
    manifest: &Manifest,
) -> Result<Option<registry::Asset>, String> {
    let id = &manifest.marketplace_id;

    if let Some(asset) = self
        .mine
        .iter()
        .find(|a| a.marketplace_id.eq_ignore_ascii_case(id))
    {
        return Ok(Some(asset.clone()));
    }

    // Not among the caller's listings, so the id is either free or somebody
    // else's — and `my-assets` cannot tell those apart.
    match registry.lookup(id) {
        // A marketplace that cannot answer is not a reason to stop here; the
        // server rejects a taken id at upload either way.
        Err(_) => Ok(None),
        Ok(None) => Ok(None),
        Ok(Some(holder)) if holder.yours => Err(format!(
            "the marketplace id `{id}` belongs to your listing \"{}\", which is \
             not among your listings — refusing rather than publishing a second \
             one beside it.",
            holder.name
        )),
        Ok(Some(holder)) => Err(format!(
            "the marketplace id `{id}` is taken by \"{}\", which is not yours. \
             Ids are first come, first served and cannot be changed — choose \
             another in {}.",
            holder.name,
            manifest.source.display()
        )),
    }
}
}


// ── Request bodies ─────────────────────────────────────────────────────────

fn upload_metadata(manifest: &Manifest) -> Result<String, String> {
    // Nothing to merge onto — the listing does not exist yet, and the server
    // adds `crate_name` to whatever we send.
    let mut fields = listing_fields(manifest, None);
    let object = fields.as_object_mut().expect("listing_fields builds an object");
    object.insert("category".into(), manifest.category.clone().into());
    object.insert(
        "marketplace_id".into(),
        manifest.marketplace_id.clone().into(),
    );
    object.insert("version".into(), manifest.version.clone().into());
    object.insert("zip_action".into(), manifest.zip_action.clone().into());
    object.insert("download_filename".into(), manifest.archive_name().into());
    serde_json::to_string(&fields).map_err(|e| e.to_string())
}

/// The fields that describe the listing rather than one version of it. Sent on
/// create, and re-sent on every release so the manifest stays authoritative.
///
/// `existing` is the metadata already on the listing, which the manifest's own
/// metadata is merged *onto* rather than replacing: an update overwrites that
/// column wholesale, and the server stores things there we must not drop —
/// `crate_name` names the directory the editor installs a plugin into, and
/// losing it breaks the install for everyone who has already bought it.
fn listing_fields(manifest: &Manifest, existing: Option<&serde_json::Value>) -> serde_json::Value {
    let mut fields = serde_json::Map::new();
    let mut put = |key: &str, value: serde_json::Value| {
        fields.insert(key.into(), value);
    };

    // A new listing has to be called something and starts from nothing, so
    // every default applies. Updating is the opposite: a silent default is not
    // an instruction, and sending one would overwrite whatever a person set on
    // the website — a title, a price, a licence — with a value the manifest
    // never actually stated.
    let creating = existing.is_none();

    put("description", manifest.description.clone().into());
    put("metadata", merge_metadata(existing, &manifest.metadata));

    match (&manifest.title, creating) {
        (Some(title), _) => put("name", title.clone().into()),
        (None, true) => put("name", manifest.name.clone().into()),
        (None, false) => {}
    }
    match (manifest.price_credits, creating) {
        (Some(price), _) => put("price_credits", price.into()),
        (None, true) => put("price_credits", 0.into()),
        (None, false) => {}
    }
    match (&manifest.licence, creating) {
        (Some(licence), _) => put("licence", licence.clone().into()),
        (None, true) => put("licence", "standard".into()),
        (None, false) => {}
    }
    match (manifest.ai_generated, creating) {
        (Some(flag), _) => put("ai_generated", flag.into()),
        (None, true) => put("ai_generated", false.into()),
        (None, false) => {}
    }
    if !manifest.tags.is_empty() || creating {
        put("tags", manifest.tags.clone().into());
    }
    for (key, value) in [
        ("subcategory", &manifest.subcategory),
        ("credit_name", &manifest.credit_name),
        ("credit_url", &manifest.credit_url),
    ] {
        if !value.is_empty() || creating {
            put(key, value.clone().into());
        }
    }

    serde_json::Value::Object(fields)
}

/// `existing` with the manifest's keys written over it. Shallow on purpose:
/// these are flat key/value hints (`poly_count`, `render_pipeline`), and a
/// deep merge would make it impossible to replace a nested value.
fn merge_metadata(
    existing: Option<&serde_json::Value>,
    manifest: &serde_json::Value,
) -> serde_json::Value {
    let mut merged = existing
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    if let Some(extra) = manifest.as_object() {
        merged.extend(extra.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    serde_json::Value::Object(merged)
}

fn attach_media(body: &mut Multipart, manifest: &Manifest) -> Result<(), String> {
    if let Some(path) = &manifest.thumbnail {
        body.file("thumbnail", &filename(path), &read(path)?);
    }
    for (i, path) in manifest.screenshots.iter().enumerate() {
        body.file(&format!("screenshot_{i}"), &filename(path), &read(path)?);
    }
    if let Some(path) = &manifest.video {
        body.file("video", &filename(path), &read(path)?);
    }
    if let Some(path) = &manifest.audio {
        body.file("audio", &filename(path), &read(path)?);
    }
    Ok(())
}

// ── Release notes ──────────────────────────────────────────────────────────

/// What to write in the release notes: the flag, the file, or the CHANGELOG.
///
/// The server seeds notes from a CHANGELOG inside an *unpacked* archive, which
/// a plugin's never is — its zip is stored whole for the editor to build. So
/// the CHANGELOG is read here instead, and the section for this version is
/// preferred over the whole file.
fn release_notes(
    manifest: &Manifest,
    notes: Option<&str>,
    notes_file: Option<&str>,
) -> Result<String, String> {
    if let Some(text) = notes {
        return Ok(text.to_string());
    }
    if let Some(path) = notes_file {
        return std::fs::read_to_string(path)
            .map(|s| s.trim().to_string())
            .map_err(|e| format!("could not read {path}: {e}"));
    }

    for name in ["CHANGELOG.md", "CHANGELOG", "CHANGES.md"] {
        let path = manifest.dir.join(name);
        if let Ok(text) = std::fs::read_to_string(&path) {
            return Ok(changelog_section(&text, &manifest.version)
                .unwrap_or_else(|| text.trim().to_string()));
        }
    }
    Ok(String::new())
}

/// Pull one version's entry out of a changelog.
///
/// Matches the Keep a Changelog shape — a heading that mentions the version,
/// everything up to the next heading of the same level. Anything else returns
/// None and the caller falls back to the whole file.
fn changelog_section(text: &str, version: &str) -> Option<String> {
    let mut lines = text.lines().enumerate();
    let (start, level) = lines.find_map(|(i, line)| {
        let trimmed = line.trim_start();
        let level = trimmed.chars().take_while(|c| *c == '#').count();
        (level > 0 && mentions_version(trimmed, version)).then_some((i, level))
    })?;

    let body: Vec<&str> = text
        .lines()
        .skip(start + 1)
        .take_while(|line| {
            let hashes = line.trim_start().chars().take_while(|c| *c == '#').count();
            hashes == 0 || hashes > level
        })
        .collect();

    let section = body.join("\n").trim().to_string();
    (!section.is_empty()).then_some(section)
}

/// Does this heading name `version`? Bounded on both sides so `1.2.0` does not
/// match the `11.2.0` heading three releases later.
fn mentions_version(heading: &str, version: &str) -> bool {
    let Some(at) = heading.find(version) else {
        return false;
    };
    let before = heading[..at].chars().next_back();
    let after = heading[at + version.len()..].chars().next();
    let boundary = |c: Option<char>| !c.is_some_and(|c| c.is_ascii_alphanumeric() || c == '.');
    boundary(before) && boundary(after)
}

// ── Odds and ends ──────────────────────────────────────────────────────────

fn list_categories() -> Result<(), String> {
    let creds = credentials::load().unwrap_or(Credentials {
        url: credentials::DEFAULT_SITE.to_string(),
        token: String::new(),
    });
    for category in Registry::new(&creds).categories()? {
        println!("{:<24} {}", category.slug, category.name);
    }
    Ok(())
}

/// Reject an unknown category here, where we can list the real ones, rather
/// than letting the server bounce the whole upload with a bare name.
/// Check the engine floor names a release that exists.
///
/// The shape check in the manifest catches a tag the editor could not order at
/// all, but not one it orders against a different prefix — `alpha8` parses
/// fine and then compares against nothing, so `engine_satisfies` returns true
/// for everyone and the floor quietly does nothing. Only the real list can
/// tell those apart, so this asks for it.
///
/// Best effort: a marketplace that does not serve the list is not a reason to
/// refuse to publish.
fn engine_version_error(
    versions: &[registry::EngineVersion],
    manifest: &Manifest,
) -> Option<String> {
    let tag = manifest.min_engine_version.as_ref()?;
    if versions.is_empty() || versions.iter().any(|v| &v.id == tag) {
        return None;
    }
    Some(format!(
        "min_engine_version \"{tag}\" in {} is not an engine release. The editor \
         would not recognise it and would apply no floor at all. Known releases:\n{}",
        manifest.source.display(),
        versions
            .iter()
            .map(|v| {
                let note = if v.status == "current" { "  (current)" } else { "" };
                format!("    {}{note}", v.id)
            })
            .collect::<Vec<_>>()
            .join("\n")
    ))
}

/// Why this manifest's category is not one the marketplace has, if it isn't.
fn category_error(categories: &[registry::Category], manifest: &Manifest) -> Option<String> {
    if categories.iter().any(|c| c.slug == manifest.category) {
        return None;
    }
    Some(format!(
        "unknown category \"{}\" in {}. Valid categories:\n{}",
        manifest.category,
        manifest.source.display(),
        categories
            .iter()
            .map(|c| format!("    {:<24} {}", c.slug, c.name))
            .collect::<Vec<_>>()
            .join("\n")
    ))
}

fn resolve_dir(path: Option<&str>) -> Result<PathBuf, String> {
    let dir = PathBuf::from(path.unwrap_or("."));
    if !dir.is_dir() {
        return Err(format!("{} is not a directory", dir.display()));
    }
    // Canonicalised so messages name a path the user can paste back, and so
    // the walk below cannot escape via `..`.
    let dir = dir
        .canonicalize()
        .map_err(|e| format!("could not resolve {}: {e}", dir.display()))?;
    // Windows canonicalisation yields a \\?\ prefix that nothing else here
    // wants to print.
    let printable = dir.to_string_lossy().trim_start_matches(r"\\?\").to_string();
    Ok(PathBuf::from(printable))
}

/// Publishing is public and a version cannot be taken back, so it asks first —
/// unless there is no one to ask (a pipe, CI) or `--yes` says not to.
fn confirm(yes: bool, question: &str) -> Result<bool, String> {
    if yes || !std::io::stdin().is_terminal() {
        return Ok(true);
    }
    print!("{question} [Y/n] ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(|e| format!("could not read your answer: {e}"))?;
    let answer = answer.trim().to_lowercase();
    Ok(answer.is_empty() || answer == "y" || answer == "yes")
}

fn read(path: &Path) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|e| format!("could not read {}: {e}", path.display()))
}

fn filename(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_tags_order_by_their_number_not_their_text() {
        assert!(engine_newer("r1-alpha8", "r1-alpha7"));
        assert!(!engine_newer("r1-alpha7", "r1-alpha8"));
        assert!(!engine_newer("r1-alpha7", "r1-alpha7"));
        // The case a string compare gets backwards.
        assert!(engine_newer("r1-alpha10", "r1-alpha7"));
        assert!("r1-alpha10" < "r1-alpha7");
    }

    /// No floor is the oldest line there is: that release is offered to every
    /// editor, including ones older than any tag names.
    #[test]
    fn any_engine_is_the_oldest_line() {
        assert!(engine_newer("r1-alpha7", ""));
        assert!(!engine_newer("", "r1-alpha7"));
        assert!(!engine_newer("", ""));
    }

    /// A scheme we do not recognise must not block a publish, in either
    /// direction: "cannot tell" is not "newer".
    #[test]
    fn an_unreadable_tag_settles_nothing() {
        assert!(!engine_newer("whatever", "r1-alpha7"));
        assert!(!engine_newer("r1-alpha7", "whatever"));
    }

    const CHANGELOG: &str = "\
# Changelog

## [0.3.0] - 2026-09-01
### Added
- Terrain sculpt brush

## [0.2.0] - 2026-08-01
- First release
";

    #[test]
    fn a_changelog_section_is_cut_at_the_next_heading() {
        let section = changelog_section(CHANGELOG, "0.3.0").unwrap();
        assert_eq!(section, "### Added\n- Terrain sculpt brush");
    }

    #[test]
    fn an_unlisted_version_falls_through() {
        assert!(changelog_section(CHANGELOG, "9.9.9").is_none());
    }

    #[test]
    fn a_version_does_not_match_a_longer_one() {
        let text = "## 11.2.0\n- later\n\n## 1.2.0\n- earlier\n";
        assert_eq!(changelog_section(text, "1.2.0").unwrap(), "- earlier");
    }

    fn manifest_for(tag: &str, extra: &str) -> Manifest {
        let dir = std::env::temp_dir().join(format!("renzora-plan-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("renzora.toml"),
            format!(
                "name = \"Forest\"\nversion = \"1.1.0\"\ndescription = \"trees\"\n\
                 category = \"3d-models\"\nmarketplace_id = \"test-id\"\n{extra}"
            ),
        )
        .unwrap();
        manifest::load(&dir).unwrap()
    }

    fn listing(version: &str) -> registry::Asset {
        registry::Asset {
            id: "id".into(),
            name: "Forest".into(),
            slug: "forest-1a2b3c4d".into(),
            marketplace_id: "forest".into(),
            version: version.into(),
            category: "3d-models".into(),
            metadata: serde_json::json!({}),
        }
    }

    #[test]
    fn a_dry_run_says_it_would_create_an_unknown_listing() {
        let manifest = manifest_for("create", "");
        let lines = plan(&manifest, None);
        assert!(lines[0].contains("create a new listing"), "{lines:?}");
        assert!(lines[0].contains("v1.1.0"), "{lines:?}");
    }

    #[test]
    fn a_dry_run_names_the_version_a_release_would_leave_alone() {
        let manifest = manifest_for("release", "");
        let lines = plan(&manifest, Some(&listing("1.0.0")));

        assert!(lines[0].contains("publish v1.1.0"), "{lines:?}");
        assert!(
            lines.iter().any(|l| l.contains("leave v1.0.0 downloadable")),
            "the whole point of releases is that the old one survives: {lines:?}"
        );
    }

    #[test]
    fn a_release_must_be_ahead_of_the_current_one() {
        // The server has no ordering check at all: whatever arrives becomes the
        // current release. 0.1.0 over 1.0.0 would succeed there and quietly
        // demote the listing, so the refusal has to happen here.
        assert!(!crate::version_gt("0.1.0", "1.0.0"));
        assert!(!crate::version_gt("1.0.0", "1.0.0"));
        assert!(crate::version_gt("1.0.1", "1.0.0"));
        assert!(crate::version_gt("1.10.0", "1.9.0"), "not a string comparison");
    }

    #[test]
    fn only_a_new_listing_reports_its_gallery() {
        let dir = std::env::temp_dir().join(format!("renzora-media-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("thumb.png"), "x").unwrap();
        std::fs::write(dir.join("a.png"), "x").unwrap();
        std::fs::write(dir.join("b.png"), "x").unwrap();
        std::fs::write(
            dir.join("renzora.toml"),
            "name = \"Forest\"\nversion = \"1.1.0\"\ndescription = \"trees\"\n\
             category = \"3d-models\"\nmarketplace_id = \"test-id\"\nthumbnail = \"thumb.png\"\n\
             screenshots = [\"a.png\", \"b.png\"]\n",
        )
        .unwrap();
        let manifest = manifest::load(&dir).unwrap();

        assert_eq!(
            describe_media(&manifest).unwrap(),
            "a thumbnail, 2 screenshots"
        );
        assert!(
            !plan(&manifest, Some(&listing("1.0.0")))
                .iter()
                .any(|l| l.contains("thumbnail")),
            "a release uploads files only, so it must not promise to upload media"
        );
    }

    #[test]
    fn empty_release_notes_are_called_out() {
        assert!(describe_notes("   ").contains("none"));
        assert_eq!(describe_notes("- One change"), "- One change");
        assert_eq!(
            describe_notes("- One change\n- And another"),
            "- One change (+1 more lines)"
        );
    }

    #[test]
    fn an_update_does_not_overwrite_what_the_manifest_never_stated() {
        // The manifest states a category and an id and nothing else about the
        // listing. A release published from it must not rename "CRT Fx" to the
        // crate name, zero a price, or reset a licence chosen on the website.
        let manifest = crate_manifest("bare");
        let existing = serde_json::json!({});

        let fields = listing_fields(&manifest, Some(&existing));
        let sent = fields.as_object().unwrap();

        for key in ["name", "price_credits", "licence", "ai_generated"] {
            assert!(
                !sent.contains_key(key),
                "an update sent `{key}`, which the manifest never stated: {fields}"
            );
        }
        // What it does state still goes.
        assert_eq!(sent["description"], "trees");
    }

    #[test]
    fn creating_a_listing_sends_the_defaults_it_needs() {
        // A listing that does not exist yet has to be called something, and
        // starts from a price and a licence rather than from nothing.
        let manifest = manifest_for("fresh", "");
        let sent = listing_fields(&manifest, None);

        assert_eq!(sent["name"], "Forest");
        assert_eq!(sent["price_credits"], 0);
        assert_eq!(sent["licence"], "standard");
        assert_eq!(sent["ai_generated"], false);
    }

    #[test]
    fn a_stated_title_still_overwrites() {
        // Stated under the renzora section of a crate, and stated by a
        // renzora.toml simply having a `name` — there is no crate name for it
        // to have come from.
        let titled = crate_manifest_with("titled", "name = \"Forest Pack\"
");
        assert_eq!(
            listing_fields(&titled, Some(&serde_json::json!({})))["name"],
            "Forest Pack"
        );
        assert_eq!(
            listing_fields(&manifest_for("standalone-title", ""), Some(&serde_json::json!({})))
                ["name"],
            "Forest"
        );
    }

    /// A crate whose renzora section states nothing but the essentials, so
    /// `name` comes from `[package]` and is not a title.
    fn crate_manifest(tag: &str) -> Manifest {
        crate_manifest_with(tag, "")
    }

    fn crate_manifest_with(tag: &str, extra: &str) -> Manifest {
        let dir = std::env::temp_dir().join(format!("renzora-crate-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            format!(
                "[package]
name = \"forest\"
version = \"1.1.0\"
                 description = \"trees\"

[package.metadata.renzora]
                 category = \"3d-models\"
marketplace_id = \"forest\"
{extra}"
            ),
        )
        .unwrap();
        manifest::load(&dir).unwrap()
    }

    #[test]
    fn updating_a_listing_keeps_the_metadata_the_server_owns() {
        let existing = serde_json::json!({ "crate_name": "terrain_sculpt", "poly_count": 10 });
        let from_manifest = serde_json::json!({ "poly_count": 20 });

        let merged = merge_metadata(Some(&existing), &from_manifest);

        assert_eq!(
            merged["crate_name"], "terrain_sculpt",
            "the editor installs a plugin by its crate_name; dropping it breaks the install"
        );
        assert_eq!(merged["poly_count"], 20, "the manifest still wins where it speaks");
    }

    #[test]
    fn upload_metadata_carries_what_the_server_validates() {
        let dir = std::env::temp_dir().join(format!("renzora-meta-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("renzora.toml"),
            "name = \"Forest\"\nversion = \"1.0.0\"\ndescription = \"trees\"\ncategory = \"3d-models\"\nmarketplace_id = \"test-id\"\ntags = [\"nature\"]\n",
        )
        .unwrap();

        let manifest = manifest::load(&dir).unwrap();
        let json: serde_json::Value =
            serde_json::from_str(&upload_metadata(&manifest).unwrap()).unwrap();

        assert_eq!(json["name"], "Forest");
        assert_eq!(json["version"], "1.0.0");
        assert_eq!(json["category"], "3d-models");
        assert_eq!(json["licence"], "standard");
        assert_eq!(json["zip_action"], "extract");
        assert_eq!(json["download_filename"], "forest-1.0.0.zip");
    }
}
