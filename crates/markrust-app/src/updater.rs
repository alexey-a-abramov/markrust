// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Bounded, opt-in staging of stable releases from MarkRust's fixed GitHub repository.
//!
//! Run network and signature checks on a background thread. This module never
//! modifies an installation or runs a downloaded executable. SHA-256 detects
//! corruption; it is not an independent publisher signature. Publisher trust is
//! the configured GitHub repository over TLS until signed releases are available.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use flate2::read::GzDecoder;
use reqwest::blocking::{Client, Response};
use reqwest::redirect::Policy;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use url::Url;

pub const REPOSITORY_URL: &str = "https://github.com/alexey-a-abramov/markrust";
pub const RELEASES_URL: &str = "https://github.com/alexey-a-abramov/markrust/releases";
const LATEST_API: &str = "https://api.github.com/repos/alexey-a-abramov/markrust/releases/latest";
const BUNDLE_ID: &str = "com.alexeyabramov.markrust";
const MAX_API_BYTES: u64 = 1024 * 1024;
const MAX_CHECKSUM_BYTES: u64 = 4096;
const MAX_ARCHIVE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_EXPANDED_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_ENTRY_BYTES: u64 = 512 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 4096;
const MAX_PATH_BYTES: usize = 4096;
const MAX_PLIST_BYTES: u64 = 1024 * 1024;
const SIGNATURE_TIMEOUT: Duration = Duration::from_secs(30);
static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("In-app updates currently support macOS Apple Silicon and Intel only")]
    UnsupportedPlatform,
    #[error("Update request failed: {0}")]
    Network(String),
    #[error("GitHub update request returned HTTP {0}")]
    Http(u16),
    #[error("Invalid release metadata: {0}")]
    InvalidRelease(String),
    #[error("Update failed its integrity check: {0}")]
    Integrity(String),
    #[error("Unsafe update archive: {0}")]
    UnsafeArchive(String),
    #[error("Invalid application bundle: {0}")]
    InvalidBundle(String),
    #[error("Update storage is unavailable: {0}")]
    Storage(#[from] io::Error),
}

/// A validated upgrade offer. Fields are private to prevent arbitrary download URLs.
#[derive(Debug, Clone)]
pub struct Release {
    version: String,
    tag: String,
    name: String,
    page_url: String,
    archive: Asset,
    checksum: Asset,
    target: MacTarget,
}

impl Release {
    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn tag(&self) -> &str {
        &self.tag
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn page_url(&self) -> &str {
        &self.page_url
    }
}

/// Keeps a private staging directory alive until the install helper has copied it.
/// Dropping this value removes only the unique directory created by this module.
#[derive(Debug)]
pub struct StagedUpdate {
    directory: PrivateDirectory,
    bundle: PathBuf,
    version: String,
    archive_sha256: String,
}

impl StagedUpdate {
    pub fn bundle_path(&self) -> &Path {
        &self.bundle
    }

    pub fn staging_directory(&self) -> &Path {
        &self.directory.path
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn archive_sha256(&self) -> &str {
        &self.archive_sha256
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MacTarget {
    AppleSilicon,
    Intel,
}

impl MacTarget {
    fn current() -> Result<Self, UpdateError> {
        match (std::env::consts::OS, std::env::consts::ARCH) {
            ("macos", "aarch64") => Ok(Self::AppleSilicon),
            ("macos", "x86_64") => Ok(Self::Intel),
            _ => Err(UpdateError::UnsupportedPlatform),
        }
    }

    fn archive_name(self) -> &'static str {
        match self {
            Self::AppleSilicon => "markrust-macos-aarch64.tar.gz",
            Self::Intel => "markrust-macos-x86_64.tar.gz",
        }
    }

    fn cpu_type(self) -> u32 {
        match self {
            Self::AppleSilicon => 0x0100_000c,
            Self::Intel => 0x0100_0007,
        }
    }
}

pub fn is_supported() -> bool {
    MacTarget::current().is_ok()
}

#[derive(Debug, Clone, Deserialize)]
struct Asset {
    id: u64,
    name: String,
    size: u64,
    browser_download_url: String,
    #[serde(default)]
    digest: Option<String>,
    state: String,
}

#[derive(Deserialize)]
struct ApiRelease {
    id: u64,
    tag_name: String,
    #[serde(default)]
    name: Option<String>,
    html_url: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}

/// Discover only a newer stable release, without downloading its application.
/// GitHub's 404 when no stable releases exist is an ordinary no-update result.
pub fn check_latest(current: &str) -> Result<Option<Release>, UpdateError> {
    let target = MacTarget::current()?;
    let current = parse_current_version(current)?;
    let response = http_client()?
        .get(LATEST_API)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .timeout(Duration::from_secs(20))
        .send()
        .map_err(network_error)?;
    if response.status().as_u16() == 404 {
        return Ok(None);
    }
    let bytes = bounded_response(response, MAX_API_BYTES)?;
    parse_release(&bytes, &current, target)
}

/// Download, verify and safely extract an upgrade. Installation remains separate
/// and must be requested explicitly after the application's recovery checkpoint.
pub fn stage_update(release: &Release) -> Result<StagedUpdate, UpdateError> {
    if release.target != MacTarget::current()? {
        return Err(UpdateError::InvalidRelease(
            "release architecture does not match this application".into(),
        ));
    }
    // Revalidate before any network operation, even though Release is opaque.
    validate_asset(&release.archive, &release.tag, MAX_ARCHIVE_BYTES)?;
    validate_asset(&release.checksum, &release.tag, MAX_CHECKSUM_BYTES)?;
    let client = http_client()?;
    let checksum_bytes = fetch_asset_bytes(&client, &release.checksum, MAX_CHECKSUM_BYTES)?;
    let expected_sha256 = parse_checksum(&checksum_bytes, &release.archive.name)?;
    let github_digest = asset_digest(&release.archive)?;
    if github_digest
        .as_ref()
        .is_some_and(|digest| digest != &expected_sha256)
    {
        return Err(UpdateError::Integrity(
            "GitHub asset digest and SHA-256 sidecar disagree".into(),
        ));
    }
    let directory = private_staging_directory()?;
    let archive_path = directory.path.join("release.tar.gz");
    let computed = download_archive(&client, &release.archive, &archive_path)?;
    if computed != expected_sha256 {
        return Err(UpdateError::Integrity("archive SHA-256 mismatch".into()));
    }
    let extracted = directory.path.join("extracted");
    create_private_directory(&extracted)?;
    extract_archive(&archive_path, &extracted)?;
    validate_bundle_identity(
        &extracted.join("MarkRust.app"),
        &release.version,
        release.target,
    )?;
    verify_codesign(&extracted.join("MarkRust.app"))?;
    Ok(StagedUpdate {
        bundle: extracted.join("MarkRust.app"),
        directory,
        version: release.version.clone(),
        archive_sha256: computed,
    })
}

/// Revalidate a staged or installed bundle without executing its executable.
/// The installer calls this again after copying onto the installation volume.
pub fn validate_candidate_bundle(path: &Path, expected_version: &str) -> Result<(), UpdateError> {
    validate_bundle_identity(path, expected_version, MacTarget::current()?)?;
    verify_codesign(path)
}

/// Offline packaging probe available only in the GUI-test build. The artifact
/// is copied into a unique private system-temporary directory before using the
/// exact production extractor and bundle checks. No configuration, recovery,
/// installation, downloaded executable, or network request is touched.
#[cfg(feature = "gui-tests")]
pub fn validate_local_archive_for_test(
    archive: &Path,
    expected_version: &str,
) -> Result<StagedUpdate, UpdateError> {
    let target = MacTarget::current()?;
    parse_current_version(expected_version)?;
    let metadata = fs::symlink_metadata(archive)?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() == 0
        || metadata.len() > MAX_ARCHIVE_BYTES
    {
        return Err(UpdateError::UnsafeArchive(
            "local probe requires a bounded regular release archive".into(),
        ));
    }
    let directory = unique_directory(&std::env::temp_dir(), "markrust-archive-probe")?;
    let archive_path = directory.path.join("release.tar.gz");
    let mut source = File::open(archive)?.take(MAX_ARCHIVE_BYTES + 1);
    let mut destination = private_file(&archive_path)?;
    let mut digest = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = source.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total += count as u64;
        if total > MAX_ARCHIVE_BYTES || total > metadata.len() {
            return Err(UpdateError::UnsafeArchive(
                "local probe archive changed or exceeds its size limit".into(),
            ));
        }
        destination.write_all(&buffer[..count])?;
        digest.update(&buffer[..count]);
    }
    if total != metadata.len() {
        return Err(UpdateError::UnsafeArchive(
            "local probe archive changed or is incomplete".into(),
        ));
    }
    destination.sync_all()?;
    drop(destination);
    let extracted = directory.path.join("extracted");
    create_private_directory(&extracted)?;
    extract_archive(&archive_path, &extracted)?;
    let bundle = extracted.join("MarkRust.app");
    validate_bundle_identity(&bundle, expected_version, target)?;
    verify_codesign(&bundle)?;
    Ok(StagedUpdate {
        directory,
        bundle,
        version: expected_version.to_owned(),
        archive_sha256: format!("{:x}", digest.finalize()),
    })
}

fn parse_current_version(value: &str) -> Result<Version, UpdateError> {
    Version::parse(value).map_err(|_| UpdateError::InvalidRelease("invalid current version".into()))
}

fn parse_release(
    bytes: &[u8],
    current: &Version,
    target: MacTarget,
) -> Result<Option<Release>, UpdateError> {
    if bytes.len() as u64 > MAX_API_BYTES {
        return Err(UpdateError::InvalidRelease(
            "release response is too large".into(),
        ));
    }
    let api: ApiRelease = serde_json::from_slice(bytes)
        .map_err(|_| UpdateError::InvalidRelease("invalid GitHub release JSON".into()))?;
    if api.draft || api.prerelease {
        return Ok(None);
    }
    let version_text = api
        .tag_name
        .strip_prefix('v')
        .filter(|value| value.len() <= 64 && value.is_ascii())
        .ok_or_else(|| {
            UpdateError::InvalidRelease("release tag must be vMAJOR.MINOR.PATCH".into())
        })?;
    let version = Version::parse(version_text)
        .map_err(|_| UpdateError::InvalidRelease("invalid release semantic version".into()))?;
    if !version.pre.is_empty() || !version.build.is_empty() {
        return Ok(None);
    }
    // Explicit tuple ordering avoids build metadata or a local prerelease ever
    // turning the same numeric version into a spurious upgrade offer.
    if (version.major, version.minor, version.patch)
        <= (current.major, current.minor, current.patch)
    {
        return Ok(None);
    }
    if api.id == 0 || api.assets.len() > 128 {
        return Err(UpdateError::InvalidRelease(
            "invalid release identity or asset count".into(),
        ));
    }
    let expected_page = format!("{REPOSITORY_URL}/releases/tag/{}", api.tag_name);
    if api.html_url != expected_page {
        return Err(UpdateError::InvalidRelease(
            "release page belongs to another repository".into(),
        ));
    }
    let archive_name = target.archive_name();
    let checksum_name = format!("{archive_name}.sha256");
    let archive = unique_asset(&api.assets, archive_name)?.clone();
    let checksum = unique_asset(&api.assets, &checksum_name)?.clone();
    validate_asset(&archive, &api.tag_name, MAX_ARCHIVE_BYTES)?;
    validate_asset(&checksum, &api.tag_name, MAX_CHECKSUM_BYTES)?;
    let name = api
        .name
        .filter(|name| {
            !name.trim().is_empty() && name.len() <= 256 && !name.chars().any(char::is_control)
        })
        .unwrap_or_else(|| api.tag_name.clone());
    Ok(Some(Release {
        version: version.to_string(),
        tag: api.tag_name,
        name,
        page_url: expected_page,
        archive,
        checksum,
        target,
    }))
}

fn unique_asset<'a>(assets: &'a [Asset], name: &str) -> Result<&'a Asset, UpdateError> {
    let mut matches = assets.iter().filter(|asset| asset.name == name);
    let Some(asset) = matches.next() else {
        return Err(UpdateError::InvalidRelease(format!(
            "release is missing {name}"
        )));
    };
    if matches.next().is_some() {
        return Err(UpdateError::InvalidRelease(format!(
            "release has duplicate {name} assets"
        )));
    }
    Ok(asset)
}

fn validate_asset(asset: &Asset, tag: &str, max_bytes: u64) -> Result<(), UpdateError> {
    if asset.id == 0 || asset.state != "uploaded" || asset.size == 0 || asset.size > max_bytes {
        return Err(UpdateError::InvalidRelease(
            "asset is incomplete or exceeds the size limit".into(),
        ));
    }
    if asset.name.len() > 128 || asset.name.contains(['/', '\\']) {
        return Err(UpdateError::InvalidRelease("invalid asset filename".into()));
    }
    let expected = format!("{REPOSITORY_URL}/releases/download/{tag}/{}", asset.name);
    if asset.browser_download_url != expected {
        return Err(UpdateError::InvalidRelease(
            "asset URL belongs to another release or repository".into(),
        ));
    }
    validate_https_url(&asset.browser_download_url)?;
    asset_digest(asset)?;
    Ok(())
}

fn asset_digest(asset: &Asset) -> Result<Option<String>, UpdateError> {
    asset
        .digest
        .as_ref()
        .map(|digest| {
            let hex = digest.strip_prefix("sha256:").ok_or_else(|| {
                UpdateError::InvalidRelease("unsupported GitHub asset digest".into())
            })?;
            parse_sha256(hex)
        })
        .transpose()
}

fn parse_sha256(value: &str) -> Result<String, UpdateError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(UpdateError::Integrity("invalid SHA-256 digest".into()));
    }
    Ok(value.to_ascii_lowercase())
}

fn parse_checksum(bytes: &[u8], archive_name: &str) -> Result<String, UpdateError> {
    if bytes.len() as u64 > MAX_CHECKSUM_BYTES {
        return Err(UpdateError::Integrity(
            "SHA-256 sidecar is too large".into(),
        ));
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| UpdateError::Integrity("SHA-256 sidecar is not UTF-8".into()))?;
    let mut words = text.split_ascii_whitespace();
    let hash = words
        .next()
        .ok_or_else(|| UpdateError::Integrity("empty SHA-256 sidecar".into()))?;
    let name = words
        .next()
        .ok_or_else(|| UpdateError::Integrity("missing SHA-256 filename".into()))?;
    if name.strip_prefix('*').unwrap_or(name) != archive_name || words.next().is_some() {
        return Err(UpdateError::Integrity(
            "SHA-256 sidecar names another archive".into(),
        ));
    }
    parse_sha256(hash)
}

fn validate_https_url(value: &str) -> Result<(), UpdateError> {
    let url = Url::parse(value).map_err(|_| UpdateError::Network("invalid update URL".into()))?;
    if !trusted_redirect(&url) {
        return Err(UpdateError::Network(
            "untrusted update URL or redirect".into(),
        ));
    }
    Ok(())
}

fn trusted_redirect(url: &Url) -> bool {
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port_or_known_default() == Some(443)
        && matches!(
            url.host_str(),
            Some(
                "api.github.com"
                    | "github.com"
                    | "release-assets.githubusercontent.com"
                    | "objects.githubusercontent.com"
            )
        )
}

fn http_client() -> Result<Client, UpdateError> {
    Client::builder()
        .user_agent(concat!(
            "MarkRust/",
            env!("CARGO_PKG_VERSION"),
            " release-updater"
        ))
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(180))
        .referer(false)
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd()
        .redirect(Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 {
                attempt.error("too many update redirects")
            } else if !trusted_redirect(attempt.url()) {
                attempt.error("update redirect must use HTTPS on a GitHub download host")
            } else {
                attempt.follow()
            }
        }))
        .build()
        .map_err(network_error)
}

fn network_error(error: reqwest::Error) -> UpdateError {
    // CDN redirect URLs contain temporary query credentials; never expose them.
    UpdateError::Network(error.without_url().to_string())
}

fn check_response(response: &Response, max_bytes: u64) -> Result<(), UpdateError> {
    if !response.status().is_success() {
        return Err(UpdateError::Http(response.status().as_u16()));
    }
    if response
        .content_length()
        .is_some_and(|size| size > max_bytes)
    {
        return Err(UpdateError::Integrity(
            "download exceeds its size limit".into(),
        ));
    }
    Ok(())
}

fn bounded_response(response: Response, max_bytes: u64) -> Result<Vec<u8>, UpdateError> {
    check_response(&response, max_bytes)?;
    let mut bytes = Vec::new();
    response.take(max_bytes + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(UpdateError::Integrity(
            "download exceeds its size limit".into(),
        ));
    }
    Ok(bytes)
}

fn fetch_asset_bytes(client: &Client, asset: &Asset, limit: u64) -> Result<Vec<u8>, UpdateError> {
    let response = client
        .get(&asset.browser_download_url)
        .timeout(Duration::from_secs(20))
        .send()
        .map_err(network_error)?;
    let bytes = bounded_response(response, limit)?;
    if bytes.len() as u64 != asset.size {
        return Err(UpdateError::Integrity(
            "download size differs from GitHub metadata".into(),
        ));
    }
    if let Some(expected) = asset_digest(asset)? {
        if format!("{:x}", Sha256::digest(&bytes)) != expected {
            return Err(UpdateError::Integrity(
                "sidecar GitHub asset digest mismatch".into(),
            ));
        }
    }
    Ok(bytes)
}

fn download_archive(client: &Client, asset: &Asset, output: &Path) -> Result<String, UpdateError> {
    let mut response = client
        .get(&asset.browser_download_url)
        .send()
        .map_err(network_error)?;
    check_response(&response, MAX_ARCHIVE_BYTES)?;
    let mut destination = private_file(output)?;
    let mut digest = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = response.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total = total
            .checked_add(count as u64)
            .ok_or_else(|| UpdateError::Integrity("archive size overflow".into()))?;
        if total > MAX_ARCHIVE_BYTES || total > asset.size {
            return Err(UpdateError::Integrity(
                "archive exceeds its declared size".into(),
            ));
        }
        destination.write_all(&buffer[..count])?;
        digest.update(&buffer[..count]);
    }
    if total != asset.size {
        return Err(UpdateError::Integrity(
            "archive download is incomplete".into(),
        ));
    }
    destination.sync_all()?;
    Ok(format!("{:x}", digest.finalize()))
}

#[derive(Debug)]
struct PrivateDirectory {
    path: PathBuf,
}

impl Drop for PrivateDirectory {
    fn drop(&mut self) {
        // No shared root or unresolved environment variable is a deletion target.
        // remove_dir_all does not follow symbolic links inside this unique tree.
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn private_staging_directory() -> Result<PrivateDirectory, UpdateError> {
    let base = dirs::cache_dir().ok_or_else(|| {
        UpdateError::Storage(io::Error::new(
            io::ErrorKind::NotFound,
            "user cache directory is unavailable",
        ))
    })?;
    let root = base.join("MarkRustUpdates");
    match create_private_directory(&root) {
        Ok(()) => {}
        Err(UpdateError::Storage(error)) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    verify_private_directory(&root)?;
    unique_directory(&root, "stage")
}

fn unique_directory(root: &Path, prefix: &str) -> Result<PrivateDirectory, UpdateError> {
    for _ in 0..8 {
        let sequence = STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = root.join(format!(
            "{prefix}-{}-{time:x}-{sequence:x}",
            std::process::id()
        ));
        match create_private_directory(&path) {
            Ok(()) => return Ok(PrivateDirectory { path }),
            Err(UpdateError::Storage(error)) if error.kind() == io::ErrorKind::AlreadyExists => {
                continue
            }
            Err(error) => return Err(error),
        }
    }
    Err(UpdateError::Storage(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not create a unique update directory",
    )))
}

fn create_private_directory(path: &Path) -> Result<(), UpdateError> {
    let builder = fs::DirBuilder::new();
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = builder;
        builder.mode(0o700);
        builder
    };
    builder.create(path)?;
    Ok(())
}

fn verify_private_directory(path: &Path) -> Result<(), UpdateError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(UpdateError::UnsafeArchive(
            "update cache is not a real directory".into(),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            return Err(UpdateError::UnsafeArchive(
                "update cache must be private and owned by this user".into(),
            ));
        }
    }
    Ok(())
}

fn private_file(path: &Path) -> Result<File, UpdateError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}

/// Validated paths are deliberately more restrictive than tar::Entry::unpack.
/// No absolute paths, dot components, Windows drive names, or backslash aliases.
fn archive_path(bytes: &[u8]) -> Result<PathBuf, UpdateError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| UpdateError::UnsafeArchive("non-UTF-8 entry path".into()))?;
    if text.is_empty()
        || text.len() > MAX_PATH_BYTES
        || text.contains(['\\', ':'])
        || text.chars().any(char::is_control)
        || text
            .split('/')
            .any(|part| part == "." || part == ".." || part.is_empty())
    {
        return Err(UpdateError::UnsafeArchive(
            "invalid archive entry path".into(),
        ));
    }
    let path = Path::new(text);
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(UpdateError::UnsafeArchive(
            "archive path escapes its staging directory".into(),
        ));
    }
    let top = path
        .components()
        .next()
        .and_then(|component| component.as_os_str().to_str());
    if !matches!(
        top,
        Some(
            "MarkRust.app"
                | "markrust"
                | "BUILD-INFO.json"
                | "LICENSE-MPL-2.0"
                | "LICENSE-OFL-Inter.txt"
                | "README.txt"
        )
    ) {
        return Err(UpdateError::UnsafeArchive(
            "unexpected release archive contents".into(),
        ));
    }
    if top != Some("MarkRust.app") && path.components().count() != 1 {
        return Err(UpdateError::UnsafeArchive(
            "unexpected nested release metadata".into(),
        ));
    }
    Ok(path.to_path_buf())
}

fn ensure_parents(root: &Path, relative: &Path) -> Result<(), UpdateError> {
    let Some(parent) = relative.parent() else {
        return Ok(());
    };
    let mut cursor = root.to_path_buf();
    for component in parent.components() {
        cursor.push(component.as_os_str());
        match fs::symlink_metadata(&cursor) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(UpdateError::UnsafeArchive(
                    "entry parent is not a directory".into(),
                ))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                create_private_directory(&cursor)?
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn extract_archive(archive_file: &Path, destination: &Path) -> Result<(), UpdateError> {
    preflight_archive(archive_file)?;
    let compressed = File::open(archive_file)?;
    let decoder = GzDecoder::new(compressed);
    // Reading at most this many decompressed bytes also bounds large PAX/header
    // metadata and padding, not only the declared file contents.
    let mut archive = tar::Archive::new(decoder.take(MAX_EXPANDED_BYTES + 1));
    let mut paths = HashSet::new();
    let mut total = 0_u64;
    for (index, result) in archive.entries()?.enumerate() {
        if index >= MAX_ARCHIVE_ENTRIES {
            return Err(UpdateError::UnsafeArchive(
                "too many archive entries".into(),
            ));
        }
        let mut entry = result?;
        let kind = entry.header().entry_type();
        if !kind.is_file() && !kind.is_dir() {
            return Err(UpdateError::UnsafeArchive(
                "links and special archive entries are forbidden".into(),
            ));
        }
        let raw_path = entry.path_bytes();
        // tar producers conventionally terminate directory names with one slash.
        let bytes = if kind.is_dir() {
            raw_path.strip_suffix(b"/").unwrap_or(&raw_path)
        } else {
            &raw_path
        };
        let relative = archive_path(bytes)?;
        if !paths.insert(relative.clone()) {
            return Err(UpdateError::UnsafeArchive("duplicate archive entry".into()));
        }
        let size = entry.size();
        let mode = entry.header().mode()?;
        if mode & 0o7000 != 0 || size > MAX_ENTRY_BYTES || (kind.is_dir() && size != 0) {
            return Err(UpdateError::UnsafeArchive(
                "invalid entry size or privileged mode".into(),
            ));
        }
        total = total
            .checked_add(size)
            .ok_or_else(|| UpdateError::UnsafeArchive("archive size overflow".into()))?;
        if total > MAX_EXPANDED_BYTES {
            return Err(UpdateError::UnsafeArchive(
                "expanded archive exceeds its size limit".into(),
            ));
        }
        ensure_parents(destination, &relative)?;
        let output = destination.join(&relative);
        if kind.is_dir() {
            match create_private_directory(&output) {
                Ok(()) => {}
                Err(UpdateError::Storage(error))
                    if error.kind() == io::ErrorKind::AlreadyExists =>
                {
                    verify_private_directory(&output)?;
                }
                Err(error) => return Err(error),
            }
        } else {
            let mut file = private_file(&output)?;
            let copied = io::copy(&mut entry, &mut file)?;
            if copied != size {
                return Err(UpdateError::UnsafeArchive("truncated archive entry".into()));
            }
            file.sync_all()?;
            #[cfg(unix)]
            if mode & 0o111 != 0 {
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(fs::Permissions::from_mode(0o700))?;
            }
        }
    }
    // Force the gzip decoder to the end: CRC/truncation errors cannot hide after
    // tar's two zero blocks, and trailing expansion is still size-bounded.
    let mut remaining = archive.into_inner();
    io::copy(&mut remaining, &mut io::sink())?;
    if remaining.limit() == 0 {
        return Err(UpdateError::UnsafeArchive(
            "expanded archive exceeds its size limit".into(),
        ));
    }
    Ok(())
}

fn preflight_archive(archive_file: &Path) -> Result<(), UpdateError> {
    let decoder = GzDecoder::new(File::open(archive_file)?);
    let mut archive = tar::Archive::new(decoder.take(MAX_EXPANDED_BYTES + 1));
    // tar's normal iterator allocates PAX/GNU long-name metadata before yielding
    // an entry. A raw first pass limits those allocations before interpretation.
    // Python's release packager uses local PAX records for fractional mtimes.
    for (index, result) in archive.entries()?.raw(true).enumerate() {
        if index >= MAX_ARCHIVE_ENTRIES * 2 {
            return Err(UpdateError::UnsafeArchive(
                "too many raw archive entries".into(),
            ));
        }
        let mut entry = result?;
        let kind = entry.header().entry_type();
        let limit = if kind.is_pax_local_extensions() {
            64 * 1024
        } else if kind.is_gnu_longname() {
            MAX_PATH_BYTES as u64 + 1
        } else if kind.is_file() {
            MAX_ENTRY_BYTES
        } else if kind.is_dir() {
            0
        } else {
            return Err(UpdateError::UnsafeArchive(
                "links and special archive entries are forbidden".into(),
            ));
        };
        if entry.size() > limit {
            return Err(UpdateError::UnsafeArchive(
                "archive entry or metadata exceeds its size limit".into(),
            ));
        }
        // Consuming the entry is required to validate truncation and bounded
        // expansion rather than seeking past a malicious claimed file size.
        if io::copy(&mut entry, &mut io::sink())? != entry.size() {
            return Err(UpdateError::UnsafeArchive(
                "truncated raw archive entry".into(),
            ));
        }
    }
    let mut remaining = archive.into_inner();
    io::copy(&mut remaining, &mut io::sink())?;
    if remaining.limit() == 0 {
        return Err(UpdateError::UnsafeArchive(
            "expanded archive exceeds its size limit".into(),
        ));
    }
    Ok(())
}

fn validate_bundle_identity(
    path: &Path,
    expected: &str,
    target: MacTarget,
) -> Result<(), UpdateError> {
    let expected = Version::parse(expected)
        .map_err(|_| UpdateError::InvalidBundle("invalid expected version".into()))?;
    inspect_bundle_tree(path)?;
    let plist_path = path.join("Contents/Info.plist");
    if fs::symlink_metadata(&plist_path)?.len() > MAX_PLIST_BYTES {
        return Err(UpdateError::InvalidBundle("Info.plist is too large".into()));
    }
    let plist = plist::Value::from_file(&plist_path)
        .map_err(|_| UpdateError::InvalidBundle("invalid Info.plist".into()))?;
    let dictionary = plist
        .as_dictionary()
        .ok_or_else(|| UpdateError::InvalidBundle("Info.plist is not a dictionary".into()))?;
    let string = |key: &str| dictionary.get(key).and_then(plist::Value::as_string);
    if string("CFBundleIdentifier") != Some(BUNDLE_ID)
        || string("CFBundleExecutable") != Some("markrust")
        || string("CFBundlePackageType") != Some("APPL")
    {
        return Err(UpdateError::InvalidBundle(
            "bundle identity does not match MarkRust".into(),
        ));
    }
    let core = format!("{}.{}.{}", expected.major, expected.minor, expected.patch);
    if string("CFBundleShortVersionString") != Some(core.as_str())
        || string("CFBundleVersion") != Some(core.as_str())
        || string("MarkRustFullVersion").unwrap_or(&core) != expected.to_string()
    {
        return Err(UpdateError::InvalidBundle(
            "bundle version does not match the expected release".into(),
        ));
    }
    let executable_path = path.join("Contents/MacOS/markrust");
    let metadata = fs::symlink_metadata(&executable_path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(UpdateError::InvalidBundle(
            "bundle executable is not a regular file".into(),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o100 == 0 {
            return Err(UpdateError::InvalidBundle(
                "bundle executable is not executable by its owner".into(),
            ));
        }
    }
    let mut header = [0_u8; 12];
    File::open(executable_path)?.read_exact(&mut header)?;
    if header[..4] != [0xcf, 0xfa, 0xed, 0xfe]
        || u32::from_le_bytes(header[4..8].try_into().expect("four header bytes"))
            != target.cpu_type()
    {
        return Err(UpdateError::InvalidBundle(
            "bundle executable has the wrong Mach-O architecture".into(),
        ));
    }
    Ok(())
}

fn inspect_bundle_tree(path: &Path) -> Result<(), UpdateError> {
    let mut pending = vec![path.to_path_buf()];
    let mut entries = 0_usize;
    let mut total = 0_u64;
    while let Some(path) = pending.pop() {
        entries += 1;
        if entries > MAX_ARCHIVE_ENTRIES {
            return Err(UpdateError::InvalidBundle(
                "bundle has too many entries".into(),
            ));
        }
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() || (!metadata.is_file() && !metadata.is_dir()) {
            return Err(UpdateError::InvalidBundle(
                "bundle contains links or special files".into(),
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.mode() & 0o7000 != 0 || (metadata.is_file() && metadata.nlink() != 1) {
                return Err(UpdateError::InvalidBundle(
                    "bundle contains privileged modes or hard links".into(),
                ));
            }
        }
        if metadata.is_dir() {
            for child in fs::read_dir(&path)? {
                pending.push(child?.path());
            }
        } else {
            total = total
                .checked_add(metadata.len())
                .ok_or_else(|| UpdateError::InvalidBundle("bundle size overflow".into()))?;
            if metadata.len() > MAX_ENTRY_BYTES || total > MAX_EXPANDED_BYTES {
                return Err(UpdateError::InvalidBundle(
                    "bundle exceeds its size limit".into(),
                ));
            }
        }
    }
    Ok(())
}

fn verify_codesign(path: &Path) -> Result<(), UpdateError> {
    if !cfg!(target_os = "macos") {
        return Err(UpdateError::UnsupportedPlatform);
    }
    let mut process = Command::new("/usr/bin/codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let start = Instant::now();
    loop {
        if let Some(status) = process.try_wait()? {
            return if status.success() {
                Ok(())
            } else {
                Err(UpdateError::InvalidBundle(
                    "code signature verification failed".into(),
                ))
            };
        }
        if start.elapsed() >= SIGNATURE_TIMEOUT {
            let _ = process.kill();
            let _ = process.wait();
            return Err(UpdateError::InvalidBundle(
                "code signature verification timed out".into(),
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api_release(version: &str) -> serde_json::Value {
        let tag = format!("v{version}");
        let assets: Vec<_> = [MacTarget::AppleSilicon, MacTarget::Intel]
            .into_iter()
            .flat_map(|target| [target.archive_name().to_string(), format!("{}.sha256", target.archive_name())])
            .enumerate()
            .map(|(index, name)| serde_json::json!({
                "id": index + 1,
                "name": name,
                "size": 100,
                "browser_download_url": format!("{REPOSITORY_URL}/releases/download/{tag}/{name}"),
                "digest": null,
                "state": "uploaded"
            }))
            .collect();
        serde_json::json!({
            "id": 1,
            "tag_name": tag,
            "name": format!("MarkRust {version}"),
            "html_url": format!("{REPOSITORY_URL}/releases/tag/{tag}"),
            "draft": false,
            "prerelease": false,
            "assets": assets
        })
    }

    fn parsed(
        value: serde_json::Value,
        current: &str,
        target: MacTarget,
    ) -> Result<Option<Release>, UpdateError> {
        parse_release(
            &serde_json::to_vec(&value).unwrap(),
            &Version::parse(current).unwrap(),
            target,
        )
    }

    fn test_directory() -> PrivateDirectory {
        let sequence = STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "markrust-update-test-{}-{sequence}",
            std::process::id()
        ));
        create_private_directory(&directory).unwrap();
        PrivateDirectory { path: directory }
    }

    #[test]
    fn offers_only_a_numeric_upgrade_for_the_current_architecture() {
        for target in [MacTarget::AppleSilicon, MacTarget::Intel] {
            let offer = parsed(api_release("0.8.0"), "0.7.0", target)
                .unwrap()
                .unwrap();
            assert_eq!(offer.version(), "0.8.0");
            assert_eq!(offer.tag(), "v0.8.0");
            assert_eq!(offer.archive.name, target.archive_name());
            assert_eq!(
                offer.page_url(),
                format!("{REPOSITORY_URL}/releases/tag/v0.8.0")
            );
        }
        for current in ["0.8.0", "0.9.0", "0.8.0-beta.1", "0.8.0+local"] {
            assert!(parsed(api_release("0.8.0"), current, MacTarget::Intel)
                .unwrap()
                .is_none());
        }
        assert!(parsed(api_release("0.10.0"), "0.9.9", MacTarget::Intel)
            .unwrap()
            .is_some());
    }

    #[test]
    fn ignores_drafts_prereleases_and_build_metadata() {
        for flag in ["draft", "prerelease"] {
            let mut value = api_release("1.0.0");
            value[flag] = true.into();
            assert!(parsed(value, "0.7.0", MacTarget::Intel).unwrap().is_none());
        }
        for version in ["1.0.0-rc.1", "1.0.0+metadata"] {
            assert!(parsed(api_release(version), "0.7.0", MacTarget::Intel)
                .unwrap()
                .is_none());
        }
    }

    #[test]
    fn rejects_invalid_versions_pages_and_foreign_asset_urls() {
        for tag in ["0.8.0", "v00.8.0", "v0.8", "v0.8.0/../other", "v0.8.0\n"] {
            let mut value = api_release("0.8.0");
            value["tag_name"] = tag.into();
            assert!(
                parsed(value, "0.7.0", MacTarget::AppleSilicon).is_err(),
                "{tag}"
            );
        }
        for url in [
            "https://github.com/another/repository/releases/download/v0.8.0/markrust-macos-aarch64.tar.gz",
            "http://github.com/alexey-a-abramov/markrust/releases/download/v0.8.0/markrust-macos-aarch64.tar.gz",
            "https://github.com/alexey-a-abramov/markrust/releases/download/v0.8.0/markrust-macos-aarch64.tar.gz?token=unexpected",
        ] {
            let mut value = api_release("0.8.0");
            value["assets"][0]["browser_download_url"] = url.into();
            assert!(parsed(value, "0.7.0", MacTarget::AppleSilicon).is_err());
        }
        let mut value = api_release("0.8.0");
        value["html_url"] = "https://github.com/other/repo/releases/tag/v0.8.0".into();
        assert!(parsed(value, "0.7.0", MacTarget::Intel).is_err());
    }

    #[test]
    fn rejects_missing_duplicate_incomplete_and_oversize_assets() {
        let mut duplicate = api_release("0.8.0");
        let asset = duplicate["assets"][0].clone();
        duplicate["assets"].as_array_mut().unwrap().push(asset);
        assert!(parsed(duplicate, "0.7.0", MacTarget::AppleSilicon).is_err());
        let mut missing = api_release("0.8.0");
        missing["assets"].as_array_mut().unwrap().remove(0);
        assert!(parsed(missing, "0.7.0", MacTarget::AppleSilicon).is_err());
        for (key, value) in [
            ("state", serde_json::json!("new")),
            ("size", serde_json::json!(0)),
            ("size", serde_json::json!(MAX_ARCHIVE_BYTES + 1)),
            ("id", serde_json::json!(0)),
        ] {
            let mut release = api_release("0.8.0");
            release["assets"][0][key] = value;
            assert!(parsed(release, "0.7.0", MacTarget::AppleSilicon).is_err());
        }
    }

    #[test]
    fn redirects_are_https_only_with_fixed_github_hosts() {
        for value in [
            "https://github.com/path",
            "https://api.github.com/path",
            "https://release-assets.githubusercontent.com/download?signature=temporary",
            "https://objects.githubusercontent.com/object",
        ] {
            assert!(validate_https_url(value).is_ok());
        }
        for value in [
            "http://github.com/path",
            "https://github.com.evil.example/path",
            "https://user:password@github.com/path",
            "https://github.com:444/path",
            "file:///tmp/archive",
            "https://127.0.0.1/download",
            "https://example.com/download",
        ] {
            assert!(validate_https_url(value).is_err(), "{value}");
        }
    }

    #[test]
    fn checksums_require_the_exact_archive_and_one_digest() {
        let hash = "a".repeat(64);
        let archive = MacTarget::AppleSilicon.archive_name();
        for sidecar in [
            format!("{hash}  {archive}\n"),
            format!("{} *{archive}\n", hash.to_uppercase()),
        ] {
            assert_eq!(parse_checksum(sidecar.as_bytes(), archive).unwrap(), hash);
        }
        for sidecar in [
            format!("{hash}  other.tar.gz\n"),
            format!("{hash}  ../{archive}\n"),
            format!("{hash}  {archive}\n{hash} {archive}"),
            format!("{}  {archive}", "a".repeat(63)),
            format!("{}  {archive}", "z".repeat(64)),
            hash,
        ] {
            assert!(parse_checksum(sidecar.as_bytes(), archive).is_err());
        }
        assert!(parse_checksum(&[0xff], archive).is_err());
    }

    #[test]
    fn github_digests_are_optional_but_never_silently_ignored() {
        let mut release = api_release("0.8.0");
        release["assets"][0]["digest"] = format!("sha256:{}", "A".repeat(64)).into();
        let release = parsed(release, "0.7.0", MacTarget::AppleSilicon)
            .unwrap()
            .unwrap();
        assert_eq!(
            asset_digest(&release.archive).unwrap().unwrap(),
            "a".repeat(64)
        );
        for digest in ["sha1:abcd", "sha256:abcd", "", "md5:0123456789abcdef"] {
            let mut release = api_release("0.8.0");
            release["assets"][0]["digest"] = digest.into();
            assert!(parsed(release, "0.7.0", MacTarget::AppleSilicon).is_err());
        }
    }

    #[test]
    fn archive_paths_reject_cross_platform_traversal_and_unexpected_files() {
        for path in [
            "MarkRust.app",
            "MarkRust.app/Contents/Info.plist",
            "markrust",
            "BUILD-INFO.json",
        ] {
            assert!(archive_path(path.as_bytes()).is_ok());
        }
        for path in [
            "",
            "/MarkRust.app",
            "../MarkRust.app",
            "MarkRust.app/../other",
            "MarkRust.app/./Contents",
            "MarkRust.app//Contents",
            "MarkRust.app/Contents/",
            "MarkRust.app\\Contents\\Info.plist",
            "C:/MarkRust.app",
            "MarkRust.app/Contents\n",
            "evil.sh",
            "README.txt/nested",
        ] {
            assert!(archive_path(path.as_bytes()).is_err(), "{path}");
        }
        assert!(archive_path(&[0xff]).is_err());
        assert!(archive_path(&vec![b'a'; MAX_PATH_BYTES + 1]).is_err());
    }

    fn make_archive(entries: &[(&str, tar::EntryType, &[u8], u32)]) -> Vec<u8> {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut tar = tar::Builder::new(encoder);
        for (path, kind, bytes, mode) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(*kind);
            header.set_size(bytes.len() as u64);
            header.set_mode(*mode);
            if kind.is_symlink() || kind.is_hard_link() {
                header.set_link_name("../../outside").unwrap();
            }
            header.set_cksum();
            tar.append_data(&mut header, path, *bytes).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap()
    }

    fn extraction_result(bytes: &[u8]) -> Result<(), UpdateError> {
        let temp = test_directory();
        let archive = temp.path.join("archive.tar.gz");
        let extracted = temp.path.join("extracted");
        fs::write(&archive, bytes).unwrap();
        create_private_directory(&extracted).unwrap();
        extract_archive(&archive, &extracted)
    }

    #[test]
    fn safe_extraction_accepts_regular_files_and_directories_only() {
        let archive = make_archive(&[
            ("MarkRust.app/", tar::EntryType::Directory, b"", 0o755),
            (
                "MarkRust.app/Contents/Info.plist",
                tar::EntryType::Regular,
                b"plist",
                0o644,
            ),
            (
                "MarkRust.app/Contents/MacOS/markrust",
                tar::EntryType::Regular,
                b"executable",
                0o755,
            ),
            ("README.txt", tar::EntryType::Regular, b"readme", 0o644),
        ]);
        extraction_result(&archive).unwrap();
    }

    #[test]
    fn safe_extraction_rejects_links_devices_duplicates_and_privileged_modes() {
        for kind in [
            tar::EntryType::Symlink,
            tar::EntryType::Link,
            tar::EntryType::Char,
            tar::EntryType::Block,
            tar::EntryType::Fifo,
        ] {
            let archive = make_archive(&[("MarkRust.app/Contents/link", kind, b"", 0o644)]);
            assert!(extraction_result(&archive).is_err(), "{kind:?}");
        }
        let duplicate = make_archive(&[
            ("README.txt", tar::EntryType::Regular, b"first", 0o644),
            ("README.txt", tar::EntryType::Regular, b"second", 0o644),
        ]);
        assert!(extraction_result(&duplicate).is_err());
        for mode in [0o4644, 0o2644, 0o1644] {
            assert!(extraction_result(&make_archive(&[(
                "README.txt",
                tar::EntryType::Regular,
                b"text",
                mode
            )]))
            .is_err());
        }
    }

    #[test]
    fn safe_extraction_detects_truncated_and_corrupt_gzip() {
        let archive = make_archive(&[("README.txt", tar::EntryType::Regular, b"text", 0o644)]);
        assert!(extraction_result(&archive[..archive.len() - 5]).is_err());
        let mut corrupt = archive;
        let index = corrupt.len() - 8;
        corrupt[index] ^= 1;
        assert!(extraction_result(&corrupt).is_err());
        assert!(extraction_result(b"not gzip").is_err());
    }

    #[test]
    fn local_pax_metadata_is_bounded_and_overridden_paths_are_revalidated() {
        for (path, accepted) in [
            ("MarkRust.app/Contents/Info.plist", true),
            ("../../outside", false),
            ("/tmp/outside", false),
            ("MarkRust.app/../outside", false),
            ("MarkRust.app\\Contents\\outside", false),
        ] {
            let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            let mut tar = tar::Builder::new(encoder);
            tar.append_pax_extensions([
                ("path", path.as_bytes()),
                ("mtime", b"123.456".as_slice()),
            ])
            .unwrap();
            let mut header = tar::Header::new_ustar();
            header.set_size(4);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, "README.txt", b"text".as_slice())
                .unwrap();
            let archive = tar.into_inner().unwrap().finish().unwrap();
            assert_eq!(extraction_result(&archive).is_ok(), accepted, "{path}");
        }
        let metadata = vec![b'a'; 64 * 1024 + 1];
        assert!(extraction_result(&make_archive(&[(
            "README.txt",
            tar::EntryType::XHeader,
            &metadata,
            0o644,
        )]))
        .is_err());
    }

    #[test]
    fn raw_archive_preflight_rejects_oversize_declarations_before_reading_contents() {
        for kind in [
            tar::EntryType::Regular,
            tar::EntryType::XHeader,
            tar::EntryType::GNULongName,
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_path("README.txt").unwrap();
            header.set_entry_type(kind);
            header.set_size(MAX_ENTRY_BYTES + 1);
            header.set_mode(0o644);
            header.set_cksum();
            let mut gzip =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            gzip.write_all(header.as_bytes()).unwrap();
            gzip.write_all(&[0_u8; 1024]).unwrap();
            assert!(
                extraction_result(&gzip.finish().unwrap()).is_err(),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn many_tiny_archive_entries_cannot_evade_the_entry_limit() {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut tar = tar::Builder::new(encoder);
        // Exceed the raw-pass bound so this test rejects before creating
        // thousands of filesystem entries (and performing per-file fsync).
        for index in 0..=MAX_ARCHIVE_ENTRIES * 2 {
            let mut header = tar::Header::new_gnu();
            header.set_size(0);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(
                &mut header,
                format!("MarkRust.app/file-{index}"),
                io::empty(),
            )
            .unwrap();
        }
        assert!(extraction_result(&tar.into_inner().unwrap().finish().unwrap()).is_err());
    }

    #[test]
    fn extraction_cannot_replace_existing_files_or_follow_parent_links() {
        let temp = test_directory();
        let archive_path = temp.path.join("archive.tar.gz");
        let extracted = temp.path.join("extracted");
        create_private_directory(&extracted).unwrap();
        fs::write(extracted.join("README.txt"), b"existing").unwrap();
        fs::write(
            &archive_path,
            make_archive(&[("README.txt", tar::EntryType::Regular, b"new", 0o644)]),
        )
        .unwrap();
        assert!(extract_archive(&archive_path, &extracted).is_err());
        assert_eq!(fs::read(extracted.join("README.txt")).unwrap(), b"existing");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&temp.path, extracted.join("MarkRust.app")).unwrap();
            fs::write(
                &archive_path,
                make_archive(&[(
                    "MarkRust.app/Contents/Info.plist",
                    tar::EntryType::Regular,
                    b"malicious",
                    0o644,
                )]),
            )
            .unwrap();
            assert!(extract_archive(&archive_path, &extracted).is_err());
            assert!(!temp.path.join("Contents").exists());
        }
    }

    fn fake_bundle(root: &Path, version: &str, target: MacTarget) -> PathBuf {
        let app = root.join("MarkRust.app");
        fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        let mut dictionary = plist::Dictionary::new();
        for (key, value) in [
            ("CFBundleIdentifier", BUNDLE_ID),
            ("CFBundleExecutable", "markrust"),
            ("CFBundlePackageType", "APPL"),
            ("CFBundleShortVersionString", version),
            ("CFBundleVersion", version),
            ("MarkRustFullVersion", version),
        ] {
            dictionary.insert(key.into(), value.into());
        }
        plist::Value::Dictionary(dictionary)
            .to_file_xml(app.join("Contents/Info.plist"))
            .unwrap();
        let mut header = vec![0xcf, 0xfa, 0xed, 0xfe];
        header.extend_from_slice(&target.cpu_type().to_le_bytes());
        header.extend_from_slice(&0_u32.to_le_bytes());
        let executable = app.join("Contents/MacOS/markrust");
        fs::write(&executable, header).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(executable, fs::Permissions::from_mode(0o700)).unwrap();
        }
        app
    }

    #[test]
    fn bundle_identity_checks_version_identifier_and_native_macho_without_running_it() {
        let temp = test_directory();
        let bundle = fake_bundle(&temp.path, "0.8.0", MacTarget::AppleSilicon);
        validate_bundle_identity(&bundle, "0.8.0", MacTarget::AppleSilicon).unwrap();
        assert!(validate_bundle_identity(&bundle, "0.9.0", MacTarget::AppleSilicon).is_err());
        assert!(validate_bundle_identity(&bundle, "0.8.0", MacTarget::Intel).is_err());
        let plist_path = bundle.join("Contents/Info.plist");
        let mut value = plist::Value::from_file(&plist_path).unwrap();
        value
            .as_dictionary_mut()
            .unwrap()
            .insert("CFBundleIdentifier".into(), "com.other.application".into());
        value.to_file_xml(plist_path).unwrap();
        assert!(validate_bundle_identity(&bundle, "0.8.0", MacTarget::AppleSilicon).is_err());
    }

    #[test]
    fn numeric_legacy_bundle_version_is_supported_without_full_version_key() {
        let temp = test_directory();
        let bundle = fake_bundle(&temp.path, "0.7.0", MacTarget::Intel);
        let plist_path = bundle.join("Contents/Info.plist");
        let mut value = plist::Value::from_file(&plist_path).unwrap();
        value
            .as_dictionary_mut()
            .unwrap()
            .remove("MarkRustFullVersion");
        value.to_file_xml(plist_path).unwrap();
        validate_bundle_identity(&bundle, "0.7.0", MacTarget::Intel).unwrap();
        assert!(validate_bundle_identity(&bundle, "0.7.0-beta.1", MacTarget::Intel).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn bundle_validation_rejects_symlinked_and_hardlinked_files() {
        let temp = test_directory();
        let bundle = fake_bundle(&temp.path, "0.8.0", MacTarget::Intel);
        let executable = bundle.join("Contents/MacOS/markrust");
        fs::hard_link(&executable, bundle.join("Contents/duplicate")).unwrap();
        assert!(validate_bundle_identity(&bundle, "0.8.0", MacTarget::Intel).is_err());
        fs::remove_file(bundle.join("Contents/duplicate")).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&executable, bundle.join("Contents/link")).unwrap();
            assert!(validate_bundle_identity(&bundle, "0.8.0", MacTarget::Intel).is_err());
        }
    }

    #[test]
    fn dropping_staging_removes_only_its_unique_directory() {
        let parent = test_directory();
        let sibling = parent.path.join("keep");
        fs::write(&sibling, b"keep").unwrap();
        let owned = parent.path.join("stage");
        create_private_directory(&owned).unwrap();
        fs::write(owned.join("temporary"), b"data").unwrap();
        drop(PrivateDirectory {
            path: owned.clone(),
        });
        assert!(!owned.exists());
        assert_eq!(fs::read(sibling).unwrap(), b"keep");
    }
}
