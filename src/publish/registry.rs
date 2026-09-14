//! The marketplace HTTP API, and the multipart bodies it expects.
//!
//! Everything here is blocking and single-shot: publish makes at most four
//! requests and then exits, so an async runtime would buy nothing. Non-2xx
//! responses are turned back into the server's own `{"error": "..."}` message,
//! because those messages are written for the person who typed the command
//! ("Version '0.3.0' already exists for this asset") and are more useful than
//! any status code we could paraphrase.

use std::time::Duration;

use serde::Deserialize;

use super::credentials::Credentials;

/// Long enough for a 200MB upload on a slow line; still bounded so a dead
/// connection surfaces instead of hanging the terminal forever.
const TIMEOUT: Duration = Duration::from_secs(30 * 60);

pub struct Registry {
    api: String,
    site: String,
    token: String,
    agent: ureq::Agent,
}

#[derive(Debug, Deserialize)]
pub struct Me {
    pub username: String,
    pub role: String,
}

/// The subset of the server's `AssetDetail` that publishing cares about.
#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    pub id: String,
    pub name: String,
    pub slug: String,
    /// The listing's claimed handle. Empty on a marketplace that predates them.
    #[serde(default)]
    pub marketplace_id: String,
    pub version: String,
    pub category: String,
    /// The listing's free-form metadata. Carried because updating an asset
    /// *replaces* this column, and the server keeps things in here that the
    /// CLI never sets — `crate_name`, which the editor needs to install a
    /// plugin at all. Anything we send has to be merged onto this.
    #[serde(default)]
    pub metadata: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct MyAssets {
    assets: Vec<Asset>,
}

/// Who holds a handle, from `GET /marketplace/by-id/:marketplace_id`.
#[derive(Debug, Deserialize)]
pub struct Claim {
    pub name: String,
    /// Whether the authenticated caller owns the listing holding it.
    #[serde(default)]
    pub yours: bool,
}

#[derive(Debug, Deserialize)]
pub struct Category {
    pub name: String,
    pub slug: String,
}

#[derive(Debug, Deserialize)]
struct EngineVersions {
    #[serde(default)]
    versions: Vec<EngineVersion>,
}

/// One engine release.
///
/// Deserializes both shapes this can arrive in, because it comes from either of
/// two endpoints: the marketplace calls the field `version`, the docs index
/// calls it `id`, and they are the same string.
#[derive(Debug, Deserialize)]
pub struct EngineVersion {
    #[serde(alias = "version")]
    pub id: String,
    /// `current` for the release the docs are on, `archived` otherwise. Empty
    /// from the marketplace, which orders by `ordinal` instead and has no
    /// opinion about which release is the documented one.
    #[serde(default)]
    pub status: String,
}

#[derive(Debug, Deserialize)]
pub struct Release {
    pub version: String,
    /// The one a buyer gets by default. Absent from the response `create_release`
    /// returns, which describes the release it just made.
    #[serde(default)]
    pub is_current: bool,
    /// The oldest engine this release runs on. Empty means any.
    ///
    /// Per release, so one listing can carry an r1-alpha7 line and an r1-alpha8
    /// line at once and each engine resolves its own. That is what makes
    /// publishing a fix behind the newest version legitimate, and it is why the
    /// version check has to know which line a release belongs to.
    ///
    /// Empty from a marketplace that predates the field, where every release is
    /// on one line and the older whole-listing comparison is the right one.
    #[serde(default)]
    pub min_engine_version: String,
}

impl Registry {
    pub fn new(creds: &Credentials) -> Self {
        let config = ureq::Agent::config_builder()
            // Keep the body of a 4xx instead of collapsing it into a status.
            .http_status_as_error(false)
            .timeout_global(Some(TIMEOUT))
            .user_agent(concat!("renzora-cli/", env!("CARGO_PKG_VERSION")))
            .build();

        Self {
            api: creds.api(),
            site: creds.url.clone(),
            token: creds.token.clone(),
            agent: config.into(),
        }
    }

    /// Public page for a listing — what we print when publishing succeeds.
    pub fn asset_url(&self, slug: &str) -> String {
        format!("{}/marketplace/asset/{slug}", self.site)
    }

    /// The marketplace root this registry talks to.
    pub fn site(&self) -> &str {
        &self.site
    }

    pub fn tokens_url(&self) -> String {
        format!("{}/developers", self.site)
    }

    pub fn me(&self) -> Result<Me, String> {
        self.get("/user/me")
    }

    pub fn categories(&self) -> Result<Vec<Category>, String> {
        self.get("/marketplace/categories")
    }

    /// The engine releases a listing may name as its floor — the same list the
    /// website's "Minimum Engine Version" dropdown loads.
    /// The marketplace's list first, the docs index as a fallback.
    ///
    /// They are not interchangeable, and the order matters. Since per-release
    /// compatibility, a release's engine is a foreign key into the
    /// marketplace's own table, so that table is the only list that decides
    /// whether a publish is accepted. The docs index is a different thing that
    /// usually agrees: it lists which versions have documentation, and a
    /// version can appear there before the marketplace has it. Validating
    /// against the docs list alone would pass a tag the upload then rejects.
    ///
    /// The fallback stays because a deployment that has not been updated serves
    /// no marketplace list, and losing the check entirely would be worse than
    /// checking against a list that is merely usually right.
    pub fn engine_versions(&self) -> Result<Vec<EngineVersion>, String> {
        if let Ok(versions) = self.get::<Vec<EngineVersion>>("/marketplace/engine-versions") {
            if !versions.is_empty() {
                return Ok(versions);
            }
        }
        let list: EngineVersions = self.get("/docs/versions")?;
        Ok(list.versions)
    }

    /// Listings owned by the authenticated user.
    pub fn my_assets(&self) -> Result<Vec<Asset>, String> {
        let assets: MyAssets = self.get("/marketplace/my-assets")?;
        Ok(assets.assets)
    }

    /// Create a listing. The server publishes it immediately and gives it its
    /// first release.
    pub fn upload(&self, body: Multipart) -> Result<Asset, String> {
        self.send("/marketplace/upload", body)
    }

    /// Who holds a marketplace handle, if anyone.
    ///
    /// `None` means the handle is free. Otherwise `yours` says whether it is
    /// the caller's, which is the difference between "publish a release onto
    /// it" and "pick another name".
    pub fn lookup(&self, marketplace_id: &str) -> Result<Option<Claim>, String> {
        match self.get::<Claim>(&format!("/marketplace/by-id/{marketplace_id}")) {
            Ok(claim) => Ok(Some(claim)),
            // The endpoint answers 404 for a free handle, which `decode` turns
            // into the server's "Not found" message.
            Err(e) if e.contains("not found") => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Every version a listing has ever published, newest first.
    pub fn releases(&self, asset_id: &str) -> Result<Vec<Release>, String> {
        self.get(&format!("/marketplace/{asset_id}/releases"))
    }

    /// Add a version to an existing listing, leaving earlier ones downloadable.
    pub fn create_release(&self, asset_id: &str, body: Multipart) -> Result<Release, String> {
        self.send(&format!("/marketplace/{asset_id}/releases"), body)
    }

    /// Replace an existing listing's cover image.
    ///
    /// `PUT /:id/files` is the endpoint that *replaces a release's files*, which
    /// sounds like the last thing to call here — but it handles its `thumbnail`
    /// part independently and only touches files when the request carries some.
    /// Sending a thumbnail alone changes the cover and nothing else.
    pub fn update_thumbnail(&self, asset_id: &str, filename: &str, data: &[u8]) -> Result<(), String> {
        let mut body = Multipart::new();
        body.file("thumbnail", filename, data);
        let (content_type, bytes) = body.finish();

        let url = format!("{}/marketplace/{asset_id}/files", self.api);
        let response = self
            .agent
            .put(&url)
            .header("Authorization", &format!("Bearer {}", self.token))
            .header("Content-Type", &content_type)
            .send(&bytes[..])
            .map_err(|e| transport_error(&url, e))?;
        self.decode::<serde_json::Value>(url, response)?;
        Ok(())
    }

    /// Push manifest fields (description, tags, price...) onto an existing
    /// listing, so the manifest stays the source of truth across versions.
    pub fn update_asset(&self, asset_id: &str, fields: serde_json::Value) -> Result<(), String> {
        let url = format!("{}/marketplace/{asset_id}/update", self.api);
        let response = self
            .agent
            .put(&url)
            .header("Authorization", &format!("Bearer {}", self.token))
            .header("Content-Type", "application/json")
            .send(serde_json::to_vec(&fields).map_err(|e| e.to_string())?)
            .map_err(|e| transport_error(&url, e))?;
        self.decode::<serde_json::Value>(url, response)?;
        Ok(())
    }

    fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, String> {
        let url = format!("{}{path}", self.api);
        let response = self
            .agent
            .get(&url)
            .header("Authorization", &format!("Bearer {}", self.token))
            .call()
            .map_err(|e| transport_error(&url, e))?;
        self.decode(url, response)
    }

    fn send<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: Multipart,
    ) -> Result<T, String> {
        let url = format!("{}{path}", self.api);
        let (content_type, bytes) = body.finish();
        let response = self
            .agent
            .post(&url)
            .header("Authorization", &format!("Bearer {}", self.token))
            .header("Content-Type", &content_type)
            .send(&bytes[..])
            .map_err(|e| transport_error(&url, e))?;
        self.decode(url, response)
    }

    /// Read a response body, turning a non-2xx into the server's own message.
    fn decode<T: serde::de::DeserializeOwned>(
        &self,
        url: String,
        mut response: ureq::http::Response<ureq::Body>,
    ) -> Result<T, String> {
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .read_to_string()
            .map_err(|e| format!("could not read the response from {url}: {e}"))?;

        if !(200..300).contains(&status) {
            return Err(self.explain(status, &body));
        }
        serde_json::from_str(&body).map_err(|e| {
            format!("could not understand the response from {url}: {e}\n{}", truncate(&body))
        })
    }

    /// Turn a failed response into something worth reading.
    fn explain(&self, status: u16, body: &str) -> String {
        let message = serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
            .unwrap_or_else(|| truncate(body));

        match status {
            401 => format!(
                "the server rejected your token ({message}).\n\
                 Run `renzora login` again — tokens can be revoked or expire. \
                 Mint a new one at {}.",
                self.tokens_url()
            ),
            403 => format!("that token is not allowed to do this: {message}"),
            404 => format!("not found: {message}"),
            413 => "the upload is larger than the server accepts.".to_string(),
            429 => "rate limited by the marketplace — wait a minute and try again.".to_string(),
            500..=599 => format!("the marketplace returned a {status}: {message}"),
            _ => message,
        }
    }
}

fn transport_error(url: &str, e: ureq::Error) -> String {
    format!("could not reach {url}: {e}")
}

fn truncate(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.chars().count() > 400 {
        format!("{}...", trimmed.chars().take(400).collect::<String>())
    } else {
        trimmed.to_string()
    }
}

// ── multipart/form-data ────────────────────────────────────────────────────

/// A `multipart/form-data` body, built in memory.
///
/// Held whole rather than streamed because the archive is already in memory
/// (we just compressed it) and the server caps an upload at 200MB — a bound we
/// check before we get here.
pub struct Multipart {
    boundary: String,
    buf: Vec<u8>,
}

impl Multipart {
    pub fn new() -> Self {
        // Uniqueness is all a boundary needs, and it must not occur in any
        // part; the timestamp plus the pid is enough for a one-shot CLI.
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        Self {
            boundary: format!("renzora{nonce:x}{:x}", std::process::id()),
            buf: Vec::new(),
        }
    }

    pub fn text(&mut self, name: &str, value: &str) -> &mut Self {
        self.header(name, None, None);
        self.buf.extend_from_slice(value.as_bytes());
        self.buf.extend_from_slice(b"\r\n");
        self
    }

    pub fn file(&mut self, name: &str, filename: &str, data: &[u8]) -> &mut Self {
        self.header(name, Some(filename), Some(mime_for(filename)));
        self.buf.extend_from_slice(data);
        self.buf.extend_from_slice(b"\r\n");
        self
    }

    fn header(&mut self, name: &str, filename: Option<&str>, mime: Option<&str>) {
        self.buf
            .extend_from_slice(format!("--{}\r\n", self.boundary).as_bytes());
        match filename {
            Some(f) => self.buf.extend_from_slice(
                format!(
                    "Content-Disposition: form-data; name=\"{name}\"; filename=\"{}\"\r\n",
                    escape(f)
                )
                .as_bytes(),
            ),
            None => self.buf.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{name}\"\r\n").as_bytes(),
            ),
        }
        if let Some(mime) = mime {
            self.buf
                .extend_from_slice(format!("Content-Type: {mime}\r\n").as_bytes());
        }
        self.buf.extend_from_slice(b"\r\n");
    }

    /// (Content-Type header, body).
    pub fn finish(mut self) -> (String, Vec<u8>) {
        self.buf
            .extend_from_slice(format!("--{}--\r\n", self.boundary).as_bytes());
        (
            format!("multipart/form-data; boundary={}", self.boundary),
            self.buf,
        )
    }
}

/// Filenames go into a quoted header value, so quotes and newlines can't.
fn escape(name: &str) -> String {
    name.chars()
        .filter(|c| *c != '"' && *c != '\r' && *c != '\n')
        .collect()
}

fn mime_for(filename: &str) -> &'static str {
    match filename.rsplit('.').next().unwrap_or("").to_lowercase().as_str() {
        "zip" => "application/zip",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "flac" => "audio/flac",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body_of(m: Multipart) -> String {
        let (_, bytes) = m.finish();
        String::from_utf8_lossy(&bytes).to_string()
    }

    #[test]
    fn parts_are_framed_by_the_boundary() {
        let mut m = Multipart::new();
        let boundary = m.boundary.clone();
        m.text("metadata", "{\"a\":1}");
        m.file("file", "plugin.zip", b"PK\x03\x04");
        let body = body_of(m);

        assert!(body.contains(&format!("--{boundary}\r\n")));
        assert!(body.contains("Content-Disposition: form-data; name=\"metadata\"\r\n\r\n{\"a\":1}"));
        assert!(body.contains("name=\"file\"; filename=\"plugin.zip\""));
        assert!(body.contains("Content-Type: application/zip"));
        assert!(body.ends_with(&format!("--{boundary}--\r\n")));
    }

    #[test]
    fn filenames_cannot_break_out_of_the_header() {
        let mut m = Multipart::new();
        m.file("file", "ev\"il\r\n.zip", b"x");
        assert!(body_of(m).contains("filename=\"evil.zip\""));
    }
}
