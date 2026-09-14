// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Resolve Markdown image destinations to filesystem paths.
//!
//! Every document-controlled image is materialized into MarkRust's cache
//! before GPUI decodes it. `http(s)` URLs are fetched off the UI thread;
//! local files and `data:` URLs are read, bounded, and validated first. This
//! prevents a source path or URI from selecting GPUI's SVG fallback after the
//! editor has approved different bytes. Missing or failed images keep the alt
//! placeholder.

use std::fs;
use std::io::{self, Cursor, Read};
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use markrust_core::rich::{Block, BlockKind, Inline, RichTree};
use reqwest::Url;
use sha2::{Digest, Sha256};

const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_IMAGE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_DATA_IMAGE_URL_BYTES: usize = 24 * 1024 * 1024;
/// Keep the native decoder's worst-case RGBA allocation below 64 MiB per
/// static frame. Animated formats still have their compressed-byte cap.
const MAX_RASTER_PIXELS: u64 = 16 * 1024 * 1024;
const MAX_RASTER_DIMENSION: u32 = 8 * 1024;
// We must bound the background read before asking the shared validator to
// inspect document-controlled local bytes.
const MAX_LOCAL_SVG_BYTES: usize = markrust_core::html_visual::MAX_SAFE_SVG_BYTES;
const MAX_REDIRECTS: usize = 3;
// v3 adds full byte, format, and pixel-budget validation before any cache
// entry is handed to GPUI. Do not reuse v2 URL-keyed cache entries.
const IMAGE_CACHE_VERSION: &str = "v3-";

// A content-addressed SVG can be requested by more than one Markdown image
// in the same document. Distinct temporary names keep their atomic writes
// from racing before they converge on the same final cache path.
static CACHE_WRITE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// How a Markdown image destination is handed to GPUI's `img()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResolvedImage {
    /// A cached remote, local, or data image whose bytes were validated; GPUI
    /// `From<PathBuf>` → `Resource::Path`.
    File(PathBuf),
    /// A local document-controlled path. It must first be classified on the
    /// background executor, then looked up in the snapshot's approved local
    /// image map before it is passed to `img()`.
    Local(PathBuf),
    /// A document-controlled `data:` URL. It must be decoded, validated, and
    /// materialized on the background executor before the snapshot's approved
    /// data-image map can hand its cache path to `img()`.
    Data,
    /// A destination intentionally kept out of the image decoder (for example
    /// insecure HTTP, an unsafe SVG, or a non-image data URI).
    Blocked,
}

#[derive(Debug)]
pub(crate) enum RemoteImageError {
    UnsafeUrl(String),
    UnsafeAddress(String),
    RedirectLimit,
    Network(String),
    Status(u16),
    TooLarge,
    TooManyPixels,
    NotImage,
    UnsafeSvg,
    Io(String),
}

/// Failure while proving a local image safe for GPUI's decoder.
///
/// GPUI falls back to its SVG renderer for *any* path whose bytes do not look
/// like a raster image. Consequently, extension checks alone are not a safe
/// boundary: `photo.png` can contain SVG markup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LocalImageError {
    TooLarge,
    TooManyPixels,
    NotImage,
    UnsafeSvg,
    Io(String),
}

impl From<io::Error> for LocalImageError {
    fn from(err: io::Error) -> Self {
        Self::Io(err.to_string())
    }
}

impl std::fmt::Display for RemoteImageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsafeUrl(msg) => write!(f, "unsafe remote image URL: {msg}"),
            Self::UnsafeAddress(msg) => write!(f, "unsafe remote image address: {msg}"),
            Self::RedirectLimit => write!(f, "remote image redirected too many times"),
            Self::Network(msg) => write!(f, "network error: {msg}"),
            Self::Status(code) => write!(f, "http status {code}"),
            Self::TooLarge => write!(f, "image exceeds {MAX_IMAGE_BYTES} bytes"),
            Self::TooManyPixels => write!(
                f,
                "image exceeds the {MAX_RASTER_DIMENSION}px / {MAX_RASTER_PIXELS}-pixel decode budget"
            ),
            Self::NotImage => write!(f, "response was not an image"),
            Self::UnsafeSvg => write!(f, "response contained an unsafe SVG"),
            Self::Io(msg) => write!(f, "io error: {msg}"),
        }
    }
}

impl std::error::Error for RemoteImageError {}

impl From<io::Error> for RemoteImageError {
    fn from(err: io::Error) -> Self {
        Self::Io(err.to_string())
    }
}

impl From<reqwest::Error> for RemoteImageError {
    fn from(err: reqwest::Error) -> Self {
        Self::Network(err.to_string())
    }
}

pub(crate) fn default_image_cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("markrust")
        .join("images")
}

pub(crate) fn is_http_url(url: &str) -> bool {
    let bytes = url.trim().as_bytes();
    (bytes.len() >= 8 && bytes[..8].eq_ignore_ascii_case(b"https://"))
        || (bytes.len() >= 7 && bytes[..7].eq_ignore_ascii_case(b"http://"))
}

fn is_https_url(url: &str) -> bool {
    let bytes = url.trim().as_bytes();
    bytes.len() >= 8 && bytes[..8].eq_ignore_ascii_case(b"https://")
}

pub(crate) fn cache_path_for_url(cache_dir: &Path, url: &str) -> PathBuf {
    cache_dir.join(cache_file_name(url.trim()))
}

pub(crate) fn resolve_image_source(base_dir: Option<&Path>, url: &str) -> ResolvedImage {
    resolve_image_source_in(base_dir, url, &default_image_cache_dir())
}

pub(crate) fn resolve_image_source_in(
    base_dir: Option<&Path>,
    url: &str,
    cache_dir: &Path,
) -> ResolvedImage {
    let trimmed = url.trim();
    if is_https_url(trimmed) {
        return ResolvedImage::File(cache_path_for_url(cache_dir, trimmed));
    }
    if is_http_url(trimmed) {
        // Never let `img()` interpret an insecure URL itself. Remote content
        // is HTTPS-only and is loaded only by the explicit safe fetch path.
        return ResolvedImage::Blocked;
    }
    if starts_with_ascii_case_insensitive(trimmed, "data:") {
        // Decoding a document-controlled data URL can involve 16 MiB of bytes
        // and cache I/O. The render path must only classify it; `RichEditorView`
        // performs the bounded preflight on its background executor.
        return ResolvedImage::Data;
    }
    let path_url = trimmed
        .strip_prefix("file://")
        .map(|rest| rest.strip_prefix("localhost").unwrap_or(rest))
        .unwrap_or(trimmed);
    let decoded = percent_decode_path(path_url);
    let path = match base_dir {
        Some(dir) if !Path::new(&decoded).is_absolute() => dir.join(&decoded),
        _ => PathBuf::from(&decoded),
    };
    ResolvedImage::Local(path)
}

/// True when the resolved destination exists on disk (ready for `img(PathBuf)`).
#[cfg(test)]
pub(crate) fn image_pixels_available(resolved: &ResolvedImage) -> bool {
    match resolved {
        ResolvedImage::File(path) => path.is_file(),
        // A local path is only safe to render once `RichEditorView` has
        // populated its approved-path map.
        ResolvedImage::Local(_) | ResolvedImage::Data | ResolvedImage::Blocked => false,
    }
}

pub(crate) fn collect_remote_image_urls(tree: &RichTree) -> Vec<String> {
    let mut urls = Vec::new();
    collect_from_blocks(&tree.blocks, &mut urls);
    urls.sort();
    urls.dedup();
    urls
}

/// Return every document-controlled `data:` image URL that needs the same
/// bounded background preflight as a local file. A `data:` URL is deliberately
/// not decoded from the render path: a document can make it megabytes long.
pub(crate) fn collect_data_image_urls(tree: &RichTree) -> Vec<String> {
    let mut urls = Vec::new();
    collect_data_urls_from_blocks(&tree.blocks, &mut urls);
    urls.sort();
    urls.dedup();
    urls
}

fn collect_data_urls_from_blocks(blocks: &[Block], urls: &mut Vec<String>) {
    for block in blocks {
        for inline in &block.inlines {
            match inline {
                Inline::Image { url, .. } => push_data_image_url(url, urls),
                Inline::OpaqueInline { raw, .. } => {
                    if let Some((url, _)) = markrust_core::html_visual::html_inline_image(raw) {
                        push_data_image_url(&url, urls);
                    }
                }
                _ => {}
            }
        }
        if let BlockKind::Opaque { raw } = &block.kind {
            if let markrust_core::html_visual::HtmlBlockVisual::Image { url, .. } =
                markrust_core::html_visual::project_html_block(raw)
            {
                push_data_image_url(&url, urls);
            }
        }
        collect_data_urls_from_blocks(&block.children, urls);
    }
}

fn push_data_image_url(url: &str, urls: &mut Vec<String>) {
    let trimmed = url.trim();
    if starts_with_ascii_case_insensitive(trimmed, "data:") {
        urls.push(trimmed.to_string());
    }
}

/// Return every local image path that can reach the WYSIWYG `img()` surface.
///
/// This deliberately includes ordinary Markdown images and visual HTML
/// `<img>` elements. The latter share the same renderer, so validating only
/// Markdown syntax would leave a bypass for `src="diagram.svg"`.
pub(crate) fn collect_local_image_paths(tree: &RichTree, base_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    collect_local_paths_from_blocks(&tree.blocks, base_dir, &mut paths);
    paths.sort();
    paths.dedup();
    paths
}

fn collect_local_paths_from_blocks(
    blocks: &[Block],
    base_dir: Option<&Path>,
    paths: &mut Vec<PathBuf>,
) {
    for block in blocks {
        for inline in &block.inlines {
            match inline {
                Inline::Image { url, .. } => push_local_image_path(base_dir, url, paths),
                Inline::OpaqueInline { raw, .. } => {
                    if let Some((url, _)) = markrust_core::html_visual::html_inline_image(raw) {
                        push_local_image_path(base_dir, &url, paths);
                    }
                }
                _ => {}
            }
        }
        if let BlockKind::Opaque { raw } = &block.kind {
            if let markrust_core::html_visual::HtmlBlockVisual::Image { url, .. } =
                markrust_core::html_visual::project_html_block(raw)
            {
                push_local_image_path(base_dir, &url, paths);
            }
        }
        collect_local_paths_from_blocks(&block.children, base_dir, paths);
    }
}

fn push_local_image_path(base_dir: Option<&Path>, url: &str, paths: &mut Vec<PathBuf>) {
    if let ResolvedImage::Local(path) = resolve_image_source(base_dir, url) {
        paths.push(path);
    }
}

/// Read and classify a local image away from the UI thread.
///
/// No source path is handed to GPUI after this point. Returning a
/// content-addressed cache copy closes the interval between preflight and
/// native decode, during which a document-controlled path could otherwise be
/// replaced with an unsafe SVG.
pub(crate) fn materialize_safe_local_image(
    source: &Path,
    cache_dir: &Path,
) -> Result<PathBuf, LocalImageError> {
    let bytes = read_bounded_local_image(source)?;
    materialize_validated_local_bytes(cache_dir, &bytes)
}

fn read_bounded_local_image(source: &Path) -> Result<Vec<u8>, LocalImageError> {
    // Do not block a background executor on a FIFO, device, or directory
    // selected by document text. Symlinks to ordinary image files remain
    // supported; only their bytes are retained afterwards.
    let metadata = fs::metadata(source)?;
    if !metadata.is_file() {
        return Err(LocalImageError::NotImage);
    }
    if metadata.len() > MAX_IMAGE_BYTES {
        return Err(LocalImageError::TooLarge);
    }

    let file = fs::File::open(source)?;
    let mut bytes = Vec::with_capacity(metadata.len().min(MAX_IMAGE_BYTES) as usize);
    file.take(MAX_IMAGE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        return Err(LocalImageError::TooLarge);
    }
    Ok(bytes)
}

fn materialize_validated_local_bytes(
    cache_dir: &Path,
    bytes: &[u8],
) -> Result<PathBuf, LocalImageError> {
    match image_kind(bytes) {
        Some(ImageKind::Raster) => {
            let format = validate_raster_image(bytes).map_err(LocalImageError::from)?;
            let dest = content_cache_path(cache_dir, "local", bytes, raster_extension(format));
            write_materialized_image(&dest, bytes)?;
            Ok(dest)
        }
        Some(ImageKind::Svg) => {
            if bytes.len() > MAX_LOCAL_SVG_BYTES {
                return Err(LocalImageError::TooLarge);
            }
            let svg = std::str::from_utf8(bytes).map_err(|_| LocalImageError::NotImage)?;
            if !markrust_core::html_visual::is_safe_svg_document(svg) {
                return Err(LocalImageError::UnsafeSvg);
            }
            let dest = content_cache_path(cache_dir, "local", bytes, "svg");
            write_materialized_image(&dest, bytes)?;
            Ok(dest)
        }
        None => Err(LocalImageError::NotImage),
    }
}

fn content_cache_path(cache_dir: &Path, namespace: &str, bytes: &[u8], extension: &str) -> PathBuf {
    let digest = Sha256::digest(bytes);
    cache_dir.join(format!(
        "{IMAGE_CACHE_VERSION}{namespace}-{}.{}",
        hex_encode(&digest),
        extension
    ))
}

fn write_materialized_image(dest: &Path, bytes: &[u8]) -> Result<(), LocalImageError> {
    // Re-writing content-addressed bytes is deliberate. It replaces a stale
    // or externally altered cache entry before the path can reach GPUI.
    atomic_write_file(dest, bytes).map_err(|error| match error {
        RemoteImageError::Io(message) => LocalImageError::Io(message),
        _ => LocalImageError::Io("could not materialize validated image".into()),
    })
}

pub(crate) fn fetch_remote_image(url: &str, dest: &Path) -> Result<(), RemoteImageError> {
    fetch_remote_image_timed(url, dest, FETCH_TIMEOUT, CONNECT_TIMEOUT)
}

pub(crate) fn fetch_remote_image_timed(
    url: &str,
    dest: &Path,
    timeout: Duration,
    connect_timeout: Duration,
) -> Result<(), RemoteImageError> {
    if dest.is_file() {
        return Ok(());
    }
    let mut current = parse_safe_remote_url(url)?;

    // Resolve and pin every hop ourselves. Reqwest's normal redirect policy
    // would resolve a redirect target after our initial check, leaving an SSRF
    // path through a public URL or DNS rebinding.
    for redirect_count in 0..=MAX_REDIRECTS {
        let target = resolve_safe_remote_target(&current)?;
        let client = safe_remote_client(&target, timeout, connect_timeout)?;
        let response = client.get(current.clone()).send()?;
        let status = response.status();
        if status.is_redirection() {
            if redirect_count == MAX_REDIRECTS {
                return Err(RemoteImageError::RedirectLimit);
            }
            current = next_safe_redirect(&current, &response)?;
            continue;
        }
        if !status.is_success() {
            return Err(RemoteImageError::Status(status.as_u16()));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_IMAGE_BYTES)
        {
            return Err(RemoteImageError::TooLarge);
        }
        let mut reader = response;
        let mut buf = Vec::new();
        Read::take(&mut reader, MAX_IMAGE_BYTES + 1).read_to_end(&mut buf)?;
        if buf.len() as u64 > MAX_IMAGE_BYTES {
            return Err(RemoteImageError::TooLarge);
        }
        validate_image_bytes(&buf)?;
        return atomic_write_file(dest, &buf);
    }
    Err(RemoteImageError::RedirectLimit)
}

#[derive(Debug)]
struct SafeRemoteTarget {
    /// `None` means the URL already contains a validated IP literal. Domain
    /// targets get passed to Reqwest as a pinned DNS override.
    hostname: Option<String>,
    addresses: Vec<SocketAddr>,
}

fn parse_safe_remote_url(input: &str) -> Result<Url, RemoteImageError> {
    let url =
        Url::parse(input.trim()).map_err(|error| RemoteImageError::UnsafeUrl(error.to_string()))?;
    if url.scheme() != "https" {
        return Err(RemoteImageError::UnsafeUrl(
            "only HTTPS image URLs are allowed".into(),
        ));
    }
    if url.username() != "" || url.password().is_some() {
        return Err(RemoteImageError::UnsafeUrl(
            "embedded credentials are not allowed".into(),
        ));
    }
    if url.port_or_known_default() != Some(443) {
        return Err(RemoteImageError::UnsafeUrl(
            "only the standard HTTPS port is allowed".into(),
        ));
    }
    let host = url
        .host_str()
        .ok_or_else(|| RemoteImageError::UnsafeUrl("URL has no host".into()))?;
    if is_local_hostname(host) {
        return Err(RemoteImageError::UnsafeAddress(format!(
            "{host} is a local hostname"
        )));
    }
    Ok(url)
}

fn resolve_safe_remote_target(url: &Url) -> Result<SafeRemoteTarget, RemoteImageError> {
    let host = url
        .host_str()
        .ok_or_else(|| RemoteImageError::UnsafeUrl("URL has no host".into()))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| RemoteImageError::UnsafeUrl("URL has no HTTPS port".into()))?;
    if let Ok(ip) = host.parse::<IpAddr>() {
        if !is_public_ip(ip) {
            return Err(RemoteImageError::UnsafeAddress(format!(
                "{ip} is not public"
            )));
        }
        return Ok(SafeRemoteTarget {
            hostname: None,
            addresses: vec![SocketAddr::new(ip, port)],
        });
    }

    let addresses = (host, port)
        .to_socket_addrs()
        .map_err(|error| {
            RemoteImageError::Network(format!("DNS lookup failed for {host}: {error}"))
        })?
        .collect::<Vec<_>>();
    if addresses.is_empty() {
        return Err(RemoteImageError::Network(format!(
            "DNS lookup returned no addresses for {host}"
        )));
    }
    // Reject the whole answer if it contains any local address. Selecting only
    // the public subset would make mixed DNS answers and rebinding races too
    // easy to get wrong.
    if addresses.iter().any(|address| !is_public_ip(address.ip())) {
        return Err(RemoteImageError::UnsafeAddress(format!(
            "DNS for {host} includes a non-public address"
        )));
    }
    Ok(SafeRemoteTarget {
        hostname: Some(host.to_string()),
        addresses,
    })
}

fn safe_remote_client(
    target: &SafeRemoteTarget,
    timeout: Duration,
    connect_timeout: Duration,
) -> Result<reqwest::blocking::Client, RemoteImageError> {
    let builder = reqwest::blocking::Client::builder()
        .https_only(true)
        .no_proxy()
        .referer(false)
        .timeout(timeout)
        .connect_timeout(connect_timeout)
        .user_agent(concat!("MarkRust/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none());
    let builder = match &target.hostname {
        Some(hostname) => builder.resolve_to_addrs(hostname, &target.addresses),
        None => builder,
    };
    builder.build().map_err(RemoteImageError::from)
}

fn next_safe_redirect(
    current: &Url,
    response: &reqwest::blocking::Response,
) -> Result<Url, RemoteImageError> {
    let location = response
        .headers()
        .get(reqwest::header::LOCATION)
        .ok_or_else(|| RemoteImageError::UnsafeUrl("redirect has no Location header".into()))?
        .to_str()
        .map_err(|_| RemoteImageError::UnsafeUrl("redirect Location is not text".into()))?;
    safe_redirect_target(current, location)
}

fn safe_redirect_target(current: &Url, location: &str) -> Result<Url, RemoteImageError> {
    let next = current
        .join(location)
        .map_err(|error| RemoteImageError::UnsafeUrl(error.to_string()))?;
    parse_safe_remote_url(next.as_str())
}

fn is_local_hostname(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host.ends_with(".internal")
        || host.ends_with(".lan")
        || !host.contains('.')
}

fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_ipv4(ip.octets()),
        IpAddr::V6(ip) => is_public_ipv6(ip.segments()),
    }
}

fn is_public_ipv4([a, b, c, _]: [u8; 4]) -> bool {
    if a == 0
        || a == 10
        || a == 127
        || a >= 224
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && (b == 0 || b == 88 || b == 168))
        || (a == 198 && (b == 18 || b == 19 || b == 51))
        || (a == 203 && b == 0)
    {
        return false;
    }
    // Documentation networks must never turn examples into live requests.
    !((a == 192 && b == 0 && c == 2)
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113))
}

fn is_public_ipv6(segments: [u16; 8]) -> bool {
    let first = segments[0];
    // Only global unicast is suitable for document-controlled network I/O.
    // This excludes unspecified, loopback, IPv4-mapped, link-local, ULA, and
    // multicast ranges in one intentionally conservative test.
    if !(0x2000..=0x3fff).contains(&first) {
        return false;
    }
    // RFC 3849 documentation range.
    !(first == 0x2001 && segments[1] == 0x0db8)
}

fn collect_from_blocks(blocks: &[Block], urls: &mut Vec<String>) {
    for block in blocks {
        for inline in &block.inlines {
            if let Inline::Image { url, .. } = inline {
                if is_https_url(url) {
                    urls.push(url.trim().to_string());
                }
            }
        }
        collect_from_blocks(&block.children, urls);
    }
}

fn cache_file_name(url: &str) -> String {
    let digest = Sha256::digest(url.as_bytes());
    // Changing the validation policy must not implicitly trust a file written
    // by an older, less restrictive version of the image loader.
    let mut name = format!("{IMAGE_CACHE_VERSION}{}", hex_encode(&digest));
    if let Some(ext) = image_extension(url) {
        name.push('.');
        name.push_str(ext);
    }
    name
}

fn image_extension(url: &str) -> Option<&'static str> {
    let trimmed = url.trim();
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("data:image/svg") {
        return Some("svg");
    }
    if lower.starts_with("data:") {
        return None;
    }
    let path = trimmed.split(['?', '#']).next().unwrap_or(trimmed);
    let ext = path.rsplit('.').next()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "png",
        "jpg" | "jpeg" => "jpg",
        "gif" => "gif",
        "webp" => "webp",
        "svg" => "svg",
        "bmp" => "bmp",
        _ => return None,
    })
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn percent_decode_path(input: &str) -> String {
    String::from_utf8(percent_decode_bytes(input)).unwrap_or_else(|_| input.to_string())
}

/// Percent-decode arbitrary `data:` bytes. Unlike a filesystem path, image
/// bytes need not be UTF-8, so callers must not fall back to the encoded form.
fn percent_decode_bytes(input: &str) -> Vec<u8> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_nibble(bytes[i + 1]), hex_nibble(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn starts_with_ascii_case_insensitive(value: &str, prefix: &str) -> bool {
    value
        .as_bytes()
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix.as_bytes()))
}

fn data_url_parts(url: &str) -> Option<(&str, &str)> {
    let url = url.trim();
    if !starts_with_ascii_case_insensitive(url, "data:") {
        return None;
    }
    url[5..].split_once(',')
}

/// Decode a bounded `data:` image URL without involving GPUI's URI loader.
/// The declared MIME type is checked against the detected raster format below;
/// an SVG relabeled as `image/png` therefore cannot reach the SVG renderer.
fn decode_data_image_url(url: &str) -> Option<(&str, Vec<u8>)> {
    if url.len() > MAX_DATA_IMAGE_URL_BYTES {
        return None;
    }
    let (meta, payload) = data_url_parts(url)?;
    let mime = meta.split(';').next()?.trim();
    let bytes = if meta
        .split(';')
        .skip(1)
        .any(|part| part.trim().eq_ignore_ascii_case("base64"))
    {
        BASE64_STANDARD.decode(payload.as_bytes()).ok()?
    } else {
        percent_decode_bytes(payload)
    };
    (bytes.len() as u64 <= MAX_IMAGE_BYTES).then_some((mime, bytes))
}

/// Materialize an approved `data:` image on the background executor. The
/// caller must never hand the source URI to GPUI, including while preflight is
/// pending or fails.
pub(crate) fn materialize_safe_data_image(url: &str, cache_dir: &Path) -> Option<PathBuf> {
    let (mime, bytes) = decode_data_image_url(url)?;
    if mime.eq_ignore_ascii_case("image/svg+xml") {
        if bytes.len() > MAX_LOCAL_SVG_BYTES {
            return None;
        }
        let svg = std::str::from_utf8(&bytes).ok()?;
        if !markrust_core::html_visual::is_safe_svg_document(svg) {
            return None;
        }
        let dest = content_cache_path(cache_dir, "data", &bytes, "svg");
        write_materialized_image(&dest, &bytes).ok()?;
        return Some(dest);
    }

    let expected_format = raster_format_for_mime(mime)?;
    let detected_format = validate_raster_image(&bytes).ok()?;
    if detected_format != expected_format {
        return None;
    }
    let dest = content_cache_path(cache_dir, "data", &bytes, raster_extension(detected_format));
    write_materialized_image(&dest, &bytes).ok()?;
    Some(dest)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImageKind {
    Raster,
    Svg,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RasterImageError {
    NotImage,
    TooManyPixels,
}

impl From<RasterImageError> for LocalImageError {
    fn from(error: RasterImageError) -> Self {
        match error {
            RasterImageError::NotImage => Self::NotImage,
            RasterImageError::TooManyPixels => Self::TooManyPixels,
        }
    }
}

impl From<RasterImageError> for RemoteImageError {
    fn from(error: RasterImageError) -> Self {
        match error {
            RasterImageError::NotImage => Self::NotImage,
            RasterImageError::TooManyPixels => Self::TooManyPixels,
        }
    }
}

fn validate_image_bytes(bytes: &[u8]) -> Result<(), RemoteImageError> {
    match image_kind(bytes) {
        Some(ImageKind::Raster) => validate_raster_image(bytes).map(|_| ()).map_err(Into::into),
        Some(ImageKind::Svg) => {
            let svg = std::str::from_utf8(bytes).map_err(|_| RemoteImageError::UnsafeSvg)?;
            markrust_core::html_visual::is_safe_svg_document(svg)
                .then_some(())
                .ok_or(RemoteImageError::UnsafeSvg)
        }
        None => Err(RemoteImageError::NotImage),
    }
}

/// Use the same narrow raster set that GPUI's resource loader can display.
/// Anything else remains a placeholder rather than relying on an unknown
/// native decoder or its SVG fallback.
fn is_safe_raster_format(format: image::ImageFormat) -> bool {
    matches!(
        format,
        image::ImageFormat::Png
            | image::ImageFormat::Jpeg
            | image::ImageFormat::Gif
            | image::ImageFormat::WebP
            | image::ImageFormat::Bmp
    )
}

fn raster_format_for_mime(mime: &str) -> Option<image::ImageFormat> {
    if mime.eq_ignore_ascii_case("image/png") {
        Some(image::ImageFormat::Png)
    } else if mime.eq_ignore_ascii_case("image/jpeg") {
        Some(image::ImageFormat::Jpeg)
    } else if mime.eq_ignore_ascii_case("image/gif") {
        Some(image::ImageFormat::Gif)
    } else if mime.eq_ignore_ascii_case("image/webp") {
        Some(image::ImageFormat::WebP)
    } else if mime.eq_ignore_ascii_case("image/bmp") {
        Some(image::ImageFormat::Bmp)
    } else {
        None
    }
}

fn raster_extension(format: image::ImageFormat) -> &'static str {
    match format {
        image::ImageFormat::Png => "png",
        image::ImageFormat::Jpeg => "jpg",
        image::ImageFormat::Gif => "gif",
        image::ImageFormat::WebP => "webp",
        image::ImageFormat::Bmp => "bmp",
        // `format` only comes from `is_safe_raster_format` / MIME mapping.
        _ => "img",
    }
}

fn validate_raster_image(bytes: &[u8]) -> Result<image::ImageFormat, RasterImageError> {
    let format = image::guess_format(bytes).map_err(|_| RasterImageError::NotImage)?;
    if !is_safe_raster_format(format) {
        return Err(RasterImageError::NotImage);
    }
    let (width, height) = image::ImageReader::with_format(Cursor::new(bytes), format)
        .into_dimensions()
        .map_err(|_| RasterImageError::NotImage)?;
    if width > MAX_RASTER_DIMENSION
        || height > MAX_RASTER_DIMENSION
        || u64::from(width) * u64::from(height) > MAX_RASTER_PIXELS
    {
        return Err(RasterImageError::TooManyPixels);
    }
    Ok(format)
}

fn image_kind(bytes: &[u8]) -> Option<ImageKind> {
    if image::guess_format(bytes)
        .ok()
        .is_some_and(is_safe_raster_format)
    {
        return Some(ImageKind::Raster);
    }
    let head = &bytes[..256.min(bytes.len())];
    let trimmed = head.trim_ascii_start();
    (trimmed.starts_with(b"<svg") || (trimmed.starts_with(b"<?xml") && contains_svg_tag(trimmed)))
        .then_some(ImageKind::Svg)
}

fn contains_svg_tag(bytes: &[u8]) -> bool {
    bytes.windows(4).any(|w| w.eq_ignore_ascii_case(b"<svg"))
}

fn atomic_write_file(dest: &Path, bytes: &[u8]) -> Result<(), RemoteImageError> {
    let parent = dest
        .parent()
        .ok_or_else(|| RemoteImageError::Io("cache path has no parent directory".into()))?;
    fs::create_dir_all(parent)?;
    let sequence = CACHE_WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(
        "{}.part-{}-{sequence}",
        dest.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let write = (|| {
        fs::write(&tmp, bytes)?;
        match fs::rename(&tmp, dest) {
            Ok(()) => Ok(()),
            // On Windows `rename` cannot replace an existing destination.
            // A concurrent task is safe to accept only if it wrote exactly
            // the bytes whose hash names this cache entry. Never let an
            // altered cache file become a validation bypass.
            Err(_) if fs::read(dest).is_ok_and(|existing| existing == bytes) => Ok(()),
            Err(error) => Err(error),
        }
    })();
    let _ = fs::remove_file(&tmp);
    Ok(write?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    static SEQ: AtomicU64 = AtomicU64::new(0);

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new(prefix: &str) -> Self {
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("markrust-img-{prefix}-{}-{n}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    /// 1×1 transparent PNG.
    const TINY_PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    /// A BMP header is enough for `ImageReader::into_dimensions`; pixel data
    /// is intentionally omitted because the preflight must reject its huge
    /// dimensions before a native decoder gets a chance to allocate it.
    fn bmp_header(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = vec![0; 54];
        bytes[..2].copy_from_slice(b"BM");
        bytes[2..6].copy_from_slice(&54u32.to_le_bytes());
        bytes[10..14].copy_from_slice(&54u32.to_le_bytes());
        bytes[14..18].copy_from_slice(&40u32.to_le_bytes());
        bytes[18..22].copy_from_slice(&(width as i32).to_le_bytes());
        bytes[22..26].copy_from_slice(&(height as i32).to_le_bytes());
        bytes[26..28].copy_from_slice(&1u16.to_le_bytes());
        bytes[28..30].copy_from_slice(&24u16.to_le_bytes());
        bytes
    }

    #[test]
    fn local_relative_url_is_a_filesystem_path() {
        let base = Path::new("/docs/notes");
        let cache = Path::new("/var/cache/markrust-images");
        assert_eq!(
            resolve_image_source_in(Some(base), "assets/icon/icon.png", cache),
            ResolvedImage::Local(base.join("assets/icon/icon.png"))
        );
        assert_eq!(
            resolve_image_source_in(Some(base), "photo%20one.png", cache),
            ResolvedImage::Local(base.join("photo one.png"))
        );
    }

    #[test]
    fn absolute_and_file_urls_stay_paths() {
        let cache = Path::new("/var/cache/markrust-images");
        assert_eq!(
            resolve_image_source_in(Some(Path::new("/docs")), "/tmp/pic.png", cache),
            ResolvedImage::Local(PathBuf::from("/tmp/pic.png"))
        );
        assert_eq!(
            resolve_image_source_in(None, "file:///Users/me/pic.png", cache),
            ResolvedImage::Local(PathBuf::from("/Users/me/pic.png"))
        );
    }

    #[test]
    fn local_paths_are_unchanged_by_the_remote_cache() {
        let cache = Path::new("/var/cache/markrust-images");
        let base = Path::new("/docs");
        let local = resolve_image_source_in(Some(base), "pic.png", cache);
        assert_eq!(local, ResolvedImage::Local(base.join("pic.png")));
        assert!(
            !matches!(local, ResolvedImage::Local(ref p) if p.starts_with(cache)),
            "local destinations must not be rewritten into the cache dir"
        );
        assert_eq!(
            resolve_image_source_in(Some(base), "/abs/pic.webp", cache),
            ResolvedImage::Local(PathBuf::from("/abs/pic.webp"))
        );
    }

    #[test]
    fn collects_markdown_and_html_local_image_paths_for_preflight() {
        let tree = RichTree {
            blocks: vec![
                Block {
                    id: markrust_core::rich::NodeId(1),
                    source_range: 0..0,
                    content_hash: 0,
                    kind: BlockKind::Paragraph,
                    children: Vec::new(),
                    inlines: vec![
                        Inline::Image {
                            alt: String::new(),
                            url: "photo.png".into(),
                            title: None,
                            source_range: 0..0,
                            marks: Default::default(),
                            link: None,
                        },
                        Inline::Image {
                            alt: String::new(),
                            url: "data:image/png;base64,aGVsbG8=".into(),
                            title: None,
                            source_range: 0..0,
                            marks: Default::default(),
                            link: None,
                        },
                    ],
                },
                Block {
                    id: markrust_core::rich::NodeId(2),
                    source_range: 0..0,
                    content_hash: 0,
                    kind: BlockKind::Opaque {
                        raw: "<img src=\"diagram.svg\" alt=\"diagram\">".into(),
                    },
                    children: Vec::new(),
                    inlines: Vec::new(),
                },
            ],
            source_len: 0,
            ..Default::default()
        };

        assert_eq!(
            collect_local_image_paths(&tree, Some(Path::new("/docs"))),
            vec![
                PathBuf::from("/docs/diagram.svg"),
                PathBuf::from("/docs/photo.png"),
            ]
        );
        assert_eq!(
            collect_data_image_urls(&tree),
            vec!["data:image/png;base64,aGVsbG8="],
            "data URLs are queued for background validation, not resolved while painting"
        );
    }

    #[test]
    fn https_url_maps_to_cache_path() {
        let cache = Path::new("/var/cache/markrust-images");
        let url = "https://cdn.example/a.png";
        let resolved = resolve_image_source_in(Some(Path::new("/docs")), url, cache);
        assert_eq!(
            resolved,
            ResolvedImage::File(cache_path_for_url(cache, url))
        );
        match resolved {
            ResolvedImage::File(path) => {
                assert!(path.starts_with(cache));
                assert_ne!(path, PathBuf::from("/docs/a.png"));
                assert!(path.extension().is_some_and(|e| e == "png"));
            }
            other => panic!("expected cache file, got {other:?}"),
        }
    }

    #[test]
    fn safe_raster_data_urls_are_validated_and_materialized() {
        let dir = TestDir::new("raster-data");
        let url = format!("data:image/png;base64,{}", BASE64_STANDARD.encode(TINY_PNG));
        assert_eq!(
            resolve_image_source_in(None, &url, dir.path()),
            ResolvedImage::Data,
            "the render path only queues data preflight; it never decodes the URI"
        );
        assert!(
            fs::read_dir(dir.path()).unwrap().next().is_none(),
            "render-path classification must not write a data-image cache file"
        );
        let path = materialize_safe_data_image(&url, dir.path()).expect("safe raster data");
        assert!(path.starts_with(dir.path()));
        assert!(path.extension().is_some_and(|extension| extension == "png"));
        assert_eq!(fs::read(path).unwrap(), TINY_PNG);
    }

    #[test]
    fn insecure_and_nonimage_uris_cannot_reach_gpui() {
        let cache = Path::new("/var/cache/markrust-images");
        assert_eq!(
            resolve_image_source_in(None, "http://example.com/pixel.png", cache),
            ResolvedImage::Blocked
        );
        let html = "data:text/html,<script>alert(1)</script>";
        assert_eq!(
            resolve_image_source_in(None, html, cache),
            ResolvedImage::Data
        );
        assert!(materialize_safe_data_image(html, cache).is_none());
    }

    #[test]
    fn svg_data_url_materializes_as_a_cache_file() {
        let dir = TestDir::new("svg-data");
        let url = "data:image/svg+xml;charset=utf-8,%3Csvg%20xmlns%3D%22http%3A%2F%2Fwww.w3.org%2F2000%2Fsvg%22%3E%3C%2Fsvg%3E";
        assert_eq!(
            resolve_image_source_in(None, url, dir.path()),
            ResolvedImage::Data
        );
        let path = materialize_safe_data_image(url, dir.path()).expect("safe SVG data");
        assert!(path.starts_with(dir.path()));
        assert!(path.extension().is_some_and(|e| e == "svg"));
        assert!(path.is_file(), "SVG data must be written for img(PathBuf)");
        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains("<svg"), "{body}");
    }

    #[test]
    fn unsafe_svg_data_url_is_blocked_instead_of_reaching_img() {
        let dir = TestDir::new("unsafe-svg-data");
        let url = "data:image/svg+xml,%3Csvg%20onload%20%3D%20%22alert(1)%22%3E%3C%2Fsvg%3E";
        assert_eq!(
            resolve_image_source_in(None, url, dir.path()),
            ResolvedImage::Data
        );
        assert!(materialize_safe_data_image(url, dir.path()).is_none());
        assert!(
            fs::read_dir(dir.path()).unwrap().next().is_none(),
            "unsafe SVG must not enter the local cache"
        );
    }

    #[test]
    fn svg_disguised_as_a_raster_data_url_is_blocked_before_gpui() {
        let dir = TestDir::new("disguised-data-svg");
        for url in [
            "data:image/png,%3Csvg%20onload%3D%22alert(1)%22%3E%3C%2Fsvg%3E",
            "data:image/png;base64,PHN2ZyBvbmxvYWQ9ImFsZXJ0KDEpIj48L3N2Zz4=",
        ] {
            assert_eq!(
                resolve_image_source_in(None, url, dir.path()),
                ResolvedImage::Data
            );
            assert!(
                materialize_safe_data_image(url, dir.path()).is_none(),
                "MIME relabeling must not expose SVG bytes to GPUI: {url}"
            );
        }
        assert!(
            fs::read_dir(dir.path()).unwrap().next().is_none(),
            "blocked data SVG must never enter the cache"
        );
    }

    #[test]
    fn string_img_source_would_not_be_a_file() {
        // Regression lock: GPUI treats String as Embedded or Uri, never Path.
        // Local Markdown images first remain source paths in the tree, then
        // are preflighted and materialized before any `img()` call.
        let resolved = resolve_image_source(Some(Path::new("/doc")), "img.webp");
        assert!(matches!(resolved, ResolvedImage::Local(_)));
    }

    #[test]
    fn resolve_does_not_fetch_or_create_cache_files() {
        let dir = TestDir::new("resolve-nofetch");
        let url = "https://cdn.example/no-fetch.png";
        let resolved = resolve_image_source_in(None, url, dir.path());
        assert!(!image_pixels_available(&resolved));
        match resolved {
            ResolvedImage::File(path) => assert!(!path.exists()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unsafe_fetch_keeps_placeholder_and_never_connects() {
        let dir = TestDir::new("fetch-private");
        let url = "https://127.0.0.1/private.png";
        let dest = cache_path_for_url(dir.path(), url);
        let err =
            fetch_remote_image_timed(url, &dest, Duration::from_secs(2), Duration::from_secs(1))
                .unwrap_err();
        assert!(
            matches!(err, RemoteImageError::UnsafeAddress(_)),
            "private target must be rejected before network I/O, got {err:?}"
        );
        assert!(!dest.exists(), "failed fetch must not write a cache file");
        let resolved = resolve_image_source_in(None, url, dir.path());
        assert_eq!(resolved, ResolvedImage::File(dest.clone()));
        assert!(
            !image_pixels_available(&resolved),
            "missing cache file is the alt placeholder path"
        );
    }

    #[test]
    fn only_public_https_destinations_can_reach_the_fetcher() {
        let dir = TestDir::new("fetch-policy");
        let dest = cache_path_for_url(dir.path(), "https://example.com/pixel.png");
        let http = fetch_remote_image_timed(
            "http://example.com/pixel.png",
            &dest,
            Duration::from_secs(2),
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(matches!(http, RemoteImageError::UnsafeUrl(_)));

        let local = parse_safe_remote_url("https://localhost/pixel.png").unwrap_err();
        assert!(matches!(local, RemoteImageError::UnsafeAddress(_)));

        let public = parse_safe_remote_url("https://93.184.216.34/pixel.png").unwrap();
        let target = resolve_safe_remote_target(&public).unwrap();
        assert!(target.hostname.is_none(), "literal IPs need no DNS lookup");
        assert_eq!(target.addresses, vec!["93.184.216.34:443".parse().unwrap()]);
    }

    #[test]
    fn redirect_targets_repeat_the_same_https_and_locality_checks() {
        let current = parse_safe_remote_url("https://93.184.216.34/first.png").unwrap();
        assert!(safe_redirect_target(&current, "/second.png").is_ok());
        assert!(matches!(
            safe_redirect_target(&current, "http://example.com/insecure.png"),
            Err(RemoteImageError::UnsafeUrl(_))
        ));
        assert!(
            safe_redirect_target(&current, "https://127.0.0.1/private.png").is_ok(),
            "address validation happens immediately before each request"
        );
        let private = safe_redirect_target(&current, "https://127.0.0.1/private.png").unwrap();
        assert!(matches!(
            resolve_safe_remote_target(&private),
            Err(RemoteImageError::UnsafeAddress(_))
        ));
    }

    #[test]
    fn public_address_filter_rejects_private_reserved_and_documentation_ranges() {
        for input in [
            "0.0.0.0",
            "10.0.0.1",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.1.1",
            "172.16.0.1",
            "192.168.1.1",
            "192.0.2.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "::1",
            "fe80::1",
            "fc00::1",
            "2001:db8::1",
        ] {
            assert!(
                !is_public_ip(input.parse().unwrap()),
                "{input} must not be an image-fetch destination"
            );
        }
        assert!(is_public_ip("1.1.1.1".parse().unwrap()));
        assert!(is_public_ip("2606:4700:4700::1111".parse().unwrap()));
    }

    #[test]
    fn remote_svg_uses_the_shared_svg_validator() {
        assert!(validate_image_bytes(TINY_PNG).is_ok());
        assert!(validate_image_bytes(b"<svg><rect width=\"1\"/></svg>").is_ok());
        assert!(matches!(
            validate_image_bytes(b"<svg><script>alert(1)</script></svg>"),
            Err(RemoteImageError::UnsafeSvg)
        ));
    }

    #[test]
    fn local_raster_is_materialized_and_source_replacement_cannot_change_it() {
        let dir = TestDir::new("local-raster");
        let source = dir.path().join("photo.png");
        fs::write(&source, TINY_PNG).unwrap();

        let approved = materialize_safe_local_image(&source, dir.path()).unwrap();
        assert_ne!(approved, source, "no mutable source path may reach GPUI");
        assert!(approved.starts_with(dir.path()));
        assert!(approved
            .extension()
            .is_some_and(|extension| extension == "png"));
        assert_eq!(fs::read(&approved).unwrap(), TINY_PNG);

        // Replacing the Markdown target after approval used to turn the later
        // native path decode into an SVG bypass. The renderer now retains the
        // immutable cache copy and a fresh preflight rejects the replacement.
        fs::write(&source, b"<svg><script>alert(1)</script></svg>").unwrap();
        assert_eq!(fs::read(&approved).unwrap(), TINY_PNG);
        assert_eq!(
            materialize_safe_local_image(&source, dir.path()),
            Err(LocalImageError::UnsafeSvg)
        );
    }

    #[test]
    fn safe_local_svg_is_materialized_only_after_shared_validation() {
        let dir = TestDir::new("local-safe-svg");
        let source = dir.path().join("diagram.svg");
        let svg =
            b"<svg xmlns=\"http://www.w3.org/2000/svg\"><rect width=\"1\" height=\"1\"/></svg>";
        fs::write(&source, svg).unwrap();

        let approved = materialize_safe_local_image(&source, dir.path()).unwrap();
        assert_ne!(
            approved, source,
            "SVG must not reach GPUI from its source path"
        );
        assert!(approved.starts_with(dir.path()));
        assert!(approved.extension().is_some_and(|ext| ext == "svg"));
        assert_eq!(fs::read(&approved).unwrap(), svg);
    }

    #[test]
    fn unsafe_local_svg_never_reaches_the_materialized_cache() {
        let dir = TestDir::new("local-unsafe-svg");
        // The misleading extension locks the content-based boundary: GPUI
        // would otherwise try its SVG renderer after raster detection fails.
        let source = dir.path().join("photo.png");
        fs::write(&source, b"<svg><script>alert(1)</script></svg>").unwrap();

        assert_eq!(
            materialize_safe_local_image(&source, dir.path()),
            Err(LocalImageError::UnsafeSvg)
        );
        assert!(
            fs::read_dir(dir.path())
                .unwrap()
                .all(|entry| entry.unwrap().path() == source),
            "unsafe SVG must never create an approved cache file"
        );
    }

    #[test]
    fn local_svg_read_is_bounded_before_validation() {
        let dir = TestDir::new("local-large-svg");
        let source = dir.path().join("large.svg");
        let mut svg = b"<svg>".to_vec();
        svg.resize(MAX_LOCAL_SVG_BYTES + 1, b' ');
        fs::write(&source, svg).unwrap();

        assert_eq!(
            materialize_safe_local_image(&source, dir.path()),
            Err(LocalImageError::TooLarge)
        );
    }

    #[test]
    fn oversized_local_raster_is_rejected_before_cache_or_decode() {
        let dir = TestDir::new("local-large-raster");
        let source = dir.path().join("large.png");
        let mut png = TINY_PNG.to_vec();
        png.resize(MAX_IMAGE_BYTES as usize + 1, 0);
        fs::write(&source, png).unwrap();

        assert_eq!(
            materialize_safe_local_image(&source, dir.path()),
            Err(LocalImageError::TooLarge)
        );
        assert!(
            fs::read_dir(dir.path())
                .unwrap()
                .all(|entry| entry.unwrap().path() == source),
            "oversized raster must never enter the cache"
        );
    }

    #[test]
    fn extreme_raster_dimensions_are_rejected_before_native_decode() {
        let dir = TestDir::new("local-huge-dimensions");
        let source = dir.path().join("huge.bmp");
        let bomb = bmp_header(MAX_RASTER_DIMENSION + 1, 1);
        fs::write(&source, &bomb).unwrap();

        assert_eq!(
            materialize_safe_local_image(&source, dir.path()),
            Err(LocalImageError::TooManyPixels)
        );
        assert!(matches!(
            validate_image_bytes(&bomb),
            Err(RemoteImageError::TooManyPixels)
        ));
    }
}
