// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Resolve Markdown image destinations to filesystem paths.
//!
//! Local files stay as [`PathBuf`]s (GPUI background decode). `http(s)` URLs
//! map to a cache path; the WYSIWYG view fetches off the UI thread and writes
//! the file, then the same `img(PathBuf)` pipeline paints pixels. Missing or
//! failed remotes keep the alt placeholder.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use markrust_core::rich::{Block, Inline, RichTree};
use sha2::{Digest, Sha256};

const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_IMAGE_BYTES: u64 = 16 * 1024 * 1024;

/// How a Markdown image destination is handed to GPUI's `img()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResolvedImage {
    /// Local file or a cached remote; GPUI `From<PathBuf>` → `Resource::Path`.
    File(PathBuf),
    /// `data:` (and anything else that is not a fetchable file).
    Uri(String),
}

#[derive(Debug)]
pub(crate) enum RemoteImageError {
    Network(String),
    Status(u16),
    TooLarge,
    NotImage,
    Io(String),
}

impl std::fmt::Display for RemoteImageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Network(msg) => write!(f, "network error: {msg}"),
            Self::Status(code) => write!(f, "http status {code}"),
            Self::TooLarge => write!(f, "image exceeds {MAX_IMAGE_BYTES} bytes"),
            Self::NotImage => write!(f, "response was not an image"),
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
    if is_http_url(trimmed) {
        return ResolvedImage::File(cache_path_for_url(cache_dir, trimmed));
    }
    if trimmed.starts_with("data:") {
        return ResolvedImage::Uri(trimmed.to_string());
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
    ResolvedImage::File(path)
}

/// True when the resolved destination exists on disk (ready for `img(PathBuf)`).
#[cfg(test)]
pub(crate) fn image_pixels_available(resolved: &ResolvedImage) -> bool {
    match resolved {
        ResolvedImage::File(path) => path.is_file(),
        ResolvedImage::Uri(_) => false,
    }
}

pub(crate) fn collect_remote_image_urls(tree: &RichTree) -> Vec<String> {
    let mut urls = Vec::new();
    collect_from_blocks(&tree.blocks, &mut urls);
    urls.sort();
    urls.dedup();
    urls
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
    if !is_http_url(url) {
        return Err(RemoteImageError::Network(
            "refusing to fetch a non-http(s) image URL".into(),
        ));
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .connect_timeout(connect_timeout)
        .user_agent(concat!("MarkRust/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()?;
    let response = client.get(url.trim()).send()?;
    let status = response.status();
    if !status.is_success() {
        return Err(RemoteImageError::Status(status.as_u16()));
    }
    let mut reader = response;
    let mut buf = Vec::new();
    Read::take(&mut reader, MAX_IMAGE_BYTES + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > MAX_IMAGE_BYTES {
        return Err(RemoteImageError::TooLarge);
    }
    if !looks_like_image(&buf) {
        return Err(RemoteImageError::NotImage);
    }
    atomic_write_file(dest, &buf)
}

fn collect_from_blocks(blocks: &[Block], urls: &mut Vec<String>) {
    for block in blocks {
        for inline in &block.inlines {
            if let Inline::Image { url, .. } = inline {
                if is_http_url(url) {
                    urls.push(url.trim().to_string());
                }
            }
        }
        collect_from_blocks(&block.children, urls);
    }
}

fn cache_file_name(url: &str) -> String {
    let digest = Sha256::digest(url.as_bytes());
    let mut name = hex_encode(&digest);
    if let Some(ext) = image_extension(url) {
        name.push('.');
        name.push_str(ext);
    }
    name
}

fn image_extension(url: &str) -> Option<&'static str> {
    let path = url.split(['?', '#']).next().unwrap_or(url);
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
    String::from_utf8(out).unwrap_or_else(|_| input.to_string())
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn looks_like_image(bytes: &[u8]) -> bool {
    if bytes.len() >= 8 && bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return true;
    }
    if bytes.len() >= 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF {
        return true;
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return true;
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return true;
    }
    if bytes.starts_with(b"BM") {
        return true;
    }
    let head = &bytes[..256.min(bytes.len())];
    let trimmed = head.trim_ascii_start();
    trimmed.starts_with(b"<svg") || (trimmed.starts_with(b"<?xml") && contains_svg_tag(trimmed))
}

fn contains_svg_tag(bytes: &[u8]) -> bool {
    bytes.windows(4).any(|w| w.eq_ignore_ascii_case(b"<svg"))
}

fn atomic_write_file(dest: &Path, bytes: &[u8]) -> Result<(), RemoteImageError> {
    let parent = dest
        .parent()
        .ok_or_else(|| RemoteImageError::Io("cache path has no parent directory".into()))?;
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        "{}.part-{}",
        dest.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let write = (|| {
        fs::write(&tmp, bytes)?;
        fs::rename(&tmp, dest)?;
        Ok(())
    })();
    if write.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    write
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

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

    fn serve_http(status_line: &str, body: &[u8]) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let body = body.to_vec();
        let status_line = status_line.to_string();
        let handle = thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf);
            let header = format!(
                "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\nContent-Type: image/png\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(&body);
        });
        (format!("http://{addr}/img.png"), handle)
    }

    #[test]
    fn local_relative_url_is_a_filesystem_path() {
        let base = Path::new("/docs/notes");
        let cache = Path::new("/var/cache/markrust-images");
        assert_eq!(
            resolve_image_source_in(Some(base), "assets/icon/icon.png", cache),
            ResolvedImage::File(base.join("assets/icon/icon.png"))
        );
        assert_eq!(
            resolve_image_source_in(Some(base), "photo%20one.png", cache),
            ResolvedImage::File(base.join("photo one.png"))
        );
    }

    #[test]
    fn absolute_and_file_urls_stay_paths() {
        let cache = Path::new("/var/cache/markrust-images");
        assert_eq!(
            resolve_image_source_in(Some(Path::new("/docs")), "/tmp/pic.png", cache),
            ResolvedImage::File(PathBuf::from("/tmp/pic.png"))
        );
        assert_eq!(
            resolve_image_source_in(None, "file:///Users/me/pic.png", cache),
            ResolvedImage::File(PathBuf::from("/Users/me/pic.png"))
        );
    }

    #[test]
    fn local_paths_are_unchanged_by_the_remote_cache() {
        let cache = Path::new("/var/cache/markrust-images");
        let base = Path::new("/docs");
        let local = resolve_image_source_in(Some(base), "pic.png", cache);
        assert_eq!(local, ResolvedImage::File(base.join("pic.png")));
        assert!(
            !matches!(local, ResolvedImage::File(ref p) if p.starts_with(cache)),
            "local destinations must not be rewritten into the cache dir"
        );
        assert_eq!(
            resolve_image_source_in(Some(base), "/abs/pic.webp", cache),
            ResolvedImage::File(PathBuf::from("/abs/pic.webp"))
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
    fn data_urls_stay_uris() {
        assert_eq!(
            resolve_image_source(None, "data:image/png;base64,xx"),
            ResolvedImage::Uri("data:image/png;base64,xx".into())
        );
    }

    #[test]
    fn string_img_source_would_not_be_a_file() {
        // Regression lock: GPUI treats String as Embedded or Uri, never Path.
        // Local markdown images must keep going through ResolvedImage::File.
        let resolved = resolve_image_source(Some(Path::new("/doc")), "img.webp");
        assert!(matches!(resolved, ResolvedImage::File(_)));
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
    fn failed_fetch_keeps_placeholder() {
        let dir = TestDir::new("fetch-404");
        let (url, server) = serve_http("404 Not Found", b"gone");
        let dest = cache_path_for_url(dir.path(), &url);
        let err =
            fetch_remote_image_timed(&url, &dest, Duration::from_secs(2), Duration::from_secs(1))
                .unwrap_err();
        let _ = server.join();
        assert!(
            matches!(err, RemoteImageError::Status(404)),
            "expected 404, got {err:?}"
        );
        assert!(!dest.exists(), "failed fetch must not write a cache file");
        let resolved = resolve_image_source_in(None, &url, dir.path());
        assert_eq!(resolved, ResolvedImage::File(dest.clone()));
        assert!(
            !image_pixels_available(&resolved),
            "missing cache file is the alt placeholder path"
        );
    }

    #[test]
    fn successful_fetch_writes_cache_file() {
        let dir = TestDir::new("fetch-ok");
        let (url, server) = serve_http("200 OK", TINY_PNG);
        let dest = cache_path_for_url(dir.path(), &url);
        fetch_remote_image_timed(&url, &dest, Duration::from_secs(2), Duration::from_secs(1))
            .unwrap();
        let _ = server.join();
        assert_eq!(fs::read(&dest).unwrap(), TINY_PNG);
        let resolved = resolve_image_source_in(None, &url, dir.path());
        assert!(image_pixels_available(&resolved));
    }

    #[test]
    fn fetch_timeout_does_not_write_cache() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}/slow.png");
        let dir = TestDir::new("fetch-timeout");
        let dest = cache_path_for_url(dir.path(), &url);
        let started = Instant::now();
        let err = fetch_remote_image_timed(
            &url,
            &dest,
            Duration::from_millis(400),
            Duration::from_millis(200),
        )
        .unwrap_err();
        drop(listener);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "fetch hung: {:?}",
            started.elapsed()
        );
        assert!(
            matches!(err, RemoteImageError::Network(_)),
            "expected timeout/network error, got {err:?}"
        );
        assert!(!dest.exists());
        assert!(!image_pixels_available(&resolve_image_source_in(
            None,
            &url,
            dir.path()
        )));
    }
}
