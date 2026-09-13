//! Where the API token lives between runs.
//!
//! One file, `~/.renzora/credentials.toml`, holding the token and the site it
//! belongs to. Tokens are minted at `<site>/developers` and are the only thing
//! `renzora publish` authenticates with — there is no login-with-password path,
//! so a leaked token is revoked on that page rather than by changing a password.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The marketplace a bare `renzora login` talks to.
pub const DEFAULT_SITE: &str = "https://renzora.com";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credentials {
    /// Site root, no trailing slash (`https://renzora.com`). Stored alongside
    /// the token because a token minted on one deployment is meaningless on
    /// another — logging into a local server must not silently keep pointing
    /// `publish` at production.
    pub url: String,
    pub token: String,
}

impl Credentials {
    pub fn api(&self) -> String {
        format!("{}/api", self.url)
    }
}

pub fn path() -> Result<PathBuf, String> {
    Ok(home_dir()?.join(".renzora").join("credentials.toml"))
}

/// Load the stored credentials, or explain how to create them.
pub fn load() -> Result<Credentials, String> {
    let path = path()?;
    let text = std::fs::read_to_string(&path).map_err(|_| {
        format!(
            "not logged in — run `renzora login` first.\n\
             Create a token at {DEFAULT_SITE}/developers."
        )
    })?;
    let mut creds: Credentials = toml::from_str(&text)
        .map_err(|e| format!("{} is not valid TOML: {e}\nRun `renzora login` again.", path.display()))?;
    creds.url = creds.url.trim_end_matches('/').to_string();
    Ok(creds)
}

pub fn save(creds: &Credentials) -> Result<PathBuf, String> {
    let path = path()?;
    let dir = path.parent().expect("credentials path always has a parent");
    std::fs::create_dir_all(dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;

    let body = format!(
        "# Renzora CLI credentials. Anyone holding this token can publish as you.\n\
         # Revoke it at {}/developers.\n\
         url = {}\n\
         token = {}\n",
        creds.url,
        toml_string(&creds.url),
        toml_string(&creds.token),
    );
    std::fs::write(&path, body).map_err(|e| format!("could not write {}: {e}", path.display()))?;
    restrict_permissions(&path);
    Ok(path)
}

pub fn delete() -> Result<Option<PathBuf>, String> {
    let path = path()?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(Some(path)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("could not remove {}: {e}", path.display())),
    }
}

/// Make the file owner-only where the platform can express that.
///
/// On Windows it inherits the user profile's ACL, which is already
/// per-user — there is no portable equivalent of chmod to apply, and the
/// directory it sits in is not world-readable to begin with.
#[cfg(unix)]
fn restrict_permissions(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &std::path::Path) {}

fn home_dir() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| "could not find your home directory (neither HOME nor USERPROFILE is set)".to_string())
}

/// A TOML basic string. Tokens and URLs contain nothing exotic, but quoting
/// them properly beats writing a file we then fail to read back.
fn toml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_url_hangs_off_the_site_root() {
        let creds = Credentials {
            url: "https://renzora.com".into(),
            token: "rz_x".into(),
        };
        assert_eq!(creds.api(), "https://renzora.com/api");
    }

    #[test]
    fn strings_are_quoted_and_escaped() {
        assert_eq!(toml_string("rz_abc"), "\"rz_abc\"");
        assert_eq!(toml_string("a\"b\\c"), "\"a\\\"b\\\\c\"");
    }
}
