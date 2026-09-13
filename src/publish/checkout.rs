//! Publishing from a git repository rather than from a working directory.
//!
//! `renzora publish --repo <url>` fetches the repository into a temporary
//! directory and publishes from that. The point is what it *excludes*: a
//! working directory carries build output, half-finished edits and whatever
//! else happens to be sitting there, and none of that is in a commit. What
//! ships is then exactly what someone else would get by cloning — which is
//! also what makes publishing from CI possible at all.
//!
//! `git` is shelled out to rather than linked, the same way `renzora new`
//! already clones the engine. It brings no dependency, and it means the user's
//! existing credentials and SSH agent work for a private repository without the
//! CLI ever handling a secret.

use std::path::{Path, PathBuf};
use std::process::Command;

/// A repository fetched to a temporary directory, removed when dropped.
pub struct Checkout {
    pub dir: PathBuf,
    /// The commit that was actually fetched, so the run can say what it built.
    pub commit: String,
}

impl Checkout {
    /// Fetch `url` at `reference` (a branch, tag or commit; `None` for the
    /// repository's default branch).
    pub fn fetch(url: &str, reference: Option<&str>) -> Result<Self, String> {
        let dir = temp_dir(url)?;
        // A previous run that died before its cleanup would otherwise leave a
        // directory git refuses to initialise into.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("could not create {}: {e}", dir.display()))?;

        // Filled in below rather than rebuilt into a new value: `Drop` deletes
        // the directory, so moving out of one Checkout to make another would
        // delete the very tree just fetched.
        let mut checkout = Self {
            dir,
            commit: String::new(),
        };

        // init + fetch rather than `git clone`, because this form takes a
        // branch, a tag and a raw commit identically. `clone --branch` does not
        // accept a commit, which is the one a CI job pinning a build would use.
        let reference = reference.unwrap_or("HEAD");
        checkout.git(&["init", "--quiet"])?;
        checkout.git(&["remote", "add", "origin", url])?;
        checkout
            .git(&["fetch", "--depth", "1", "--quiet", "origin", reference])
            .map_err(|e| {
                format!(
                    "{e}\nCheck the URL, and that `{reference}` is a branch, tag \
                     or commit that exists there."
                )
            })?;
        checkout.git(&["checkout", "--quiet", "FETCH_HEAD"])?;

        let commit = checkout.git(&["rev-parse", "HEAD"])?.trim().to_string();
        checkout.commit = commit;
        Ok(checkout)
    }

    /// Where a path given on the command line resolves inside the checkout.
    pub fn resolve(&self, path: &str) -> String {
        self.dir.join(path).to_string_lossy().to_string()
    }

    fn git(&self, args: &[&str]) -> Result<String, String> {
        let out = Command::new("git")
            .current_dir(&self.dir)
            .args(args)
            .output()
            .map_err(|e| {
                format!(
                    "could not run git: {e}\n\
                     Publishing from a repository needs git on PATH."
                )
            })?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(format!(
                "git {} failed: {}",
                args[0],
                stderr.trim().lines().next().unwrap_or("no output")
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

impl Drop for Checkout {
    fn drop(&mut self) {
        // Best effort. Git marks objects read-only, which on Windows can make a
        // plain recursive delete fail; a leftover directory in the system temp
        // is not worth failing a successful publish over.
        let _ = remove_read_only(&self.dir);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Clear the read-only bit git sets on its object files, so the tree can go.
fn remove_read_only(dir: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            remove_read_only(&path)?;
        } else if let Ok(meta) = entry.metadata() {
            let mut perms = meta.permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false);
            let _ = std::fs::set_permissions(&path, perms);
        }
    }
    Ok(())
}

/// A directory name tied to the repository and this process, so two publishes
/// running at once do not fetch into the same place.
fn temp_dir(url: &str) -> Result<PathBuf, String> {
    let slug: String = url
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .rsplit('/')
        .next()
        .unwrap_or("repo")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(40)
        .collect();
    let slug = if slug.is_empty() { "repo".into() } else { slug };
    Ok(std::env::temp_dir().join(format!("renzora-publish-{slug}-{}", std::process::id())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_resolves_inside_the_checkout() {
        let checkout = Checkout {
            dir: PathBuf::from("/tmp/x"),
            commit: String::new(),
        };
        let resolved = checkout.resolve("clouds");
        assert!(resolved.ends_with("clouds"));
        assert!(resolved.contains("x"));
    }

    #[test]
    fn the_temp_directory_is_named_after_the_repository() {
        let dir = temp_dir("https://github.com/renzora/plugins.git").unwrap();
        let name = dir.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.contains("plugins"), "{name}");
        // Two runs must not collide, so the pid is in there.
        assert!(name.contains(&std::process::id().to_string()), "{name}");
    }

    #[test]
    fn a_url_that_is_all_punctuation_still_names_something() {
        let dir = temp_dir("https://example.com/../").unwrap();
        let name = dir.file_name().unwrap().to_string_lossy().to_string();
        assert!(!name.contains(".."), "{name}");
    }
}
