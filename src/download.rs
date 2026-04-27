//! HTTP/HTTPS + `file://` download helper with sha256 streaming verification
//! and a URL/sha256-keyed local cache under `<cache>/downloads/`.

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use url::Url;

/// Fetch `url` into the `cache` directory and return the absolute path of the
/// cached file. If `expected_sha256` is provided, the download stream is
/// verified against it; if a cached file exists with the right hash, no
/// network call is made.
///
/// Cache key: when sha256 is known up-front we use it as the filename. When
/// it's not known, we use `<8-hex-of-url-hash>-<basename>`. On hit we either
/// re-verify (if expected sha is supplied) or trust the cache.
pub fn fetch(url: &str, expected_sha256: Option<&str>, cache: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(cache)
        .with_context(|| format!("creating download cache {}", cache.display()))?;

    let parsed = Url::parse(url).with_context(|| format!("parsing URL {url}"))?;
    let cached_name = derive_cached_name(&parsed, expected_sha256);
    let cached_path = cache.join(&cached_name);

    // Cache hit?
    if cached_path.is_file() {
        if let Some(expected) = expected_sha256 {
            match verify_file_sha256(&cached_path, expected) {
                Ok(()) => {
                    tracing::debug!("download cache hit: {}", cached_path.display());
                    return Ok(cached_path);
                }
                Err(err) => {
                    tracing::warn!(
                        "cached file {} failed sha256 verify ({err}); re-downloading",
                        cached_path.display()
                    );
                    let _ = std::fs::remove_file(&cached_path);
                }
            }
        } else {
            tracing::debug!("download cache hit (no sha verify): {}", cached_path.display());
            return Ok(cached_path);
        }
    }

    match parsed.scheme() {
        "file" => {
            let src = parsed
                .to_file_path()
                .map_err(|()| anyhow!("invalid file:// URL: {url}"))?;
            stream_file_into_cache(&src, &cached_path, expected_sha256)?;
        }
        "http" | "https" => {
            stream_http_into_cache(url, &cached_path, expected_sha256)?;
        }
        other => bail!("unsupported URL scheme: {other}"),
    }

    Ok(cached_path)
}

/// Derive the cached filename for a URL. Sha-keyed when known, URL-hash-keyed
/// otherwise.
fn derive_cached_name(url: &Url, expected_sha256: Option<&str>) -> String {
    let basename = url
        .path_segments()
        .and_then(|mut s| s.next_back())
        .filter(|s| !s.is_empty())
        .unwrap_or("download")
        .to_string();
    if let Some(sha) = expected_sha256 {
        // sha-prefixed for at-a-glance identification, basename for filename
        // continuity (extension-aware tools work).
        format!("{sha}-{basename}")
    } else {
        let hash = xxhash_rust::xxh3::xxh3_64(url.as_str().as_bytes());
        format!("{:08x}-{basename}", hash & 0xFFFF_FFFF)
    }
}

fn stream_http_into_cache(
    url: &str,
    cached_path: &Path,
    expected_sha256: Option<&str>,
) -> Result<()> {
    tracing::info!("downloading {url}");
    let response = ureq::get(url)
        .call()
        .map_err(|e| anyhow!("HTTP GET {url}: {e}"))?;
    let status = response.status();
    if status.as_u16() >= 400 {
        bail!("HTTP {status} for {url}");
    }

    let parent = cached_path
        .parent()
        .ok_or_else(|| anyhow!("no parent for {}", cached_path.display()))?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".natron-dl-")
        .tempfile_in(parent)
        .with_context(|| format!("creating tempfile in {}", parent.display()))?;

    let mut reader = response.into_body().into_reader();
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];

    loop {
        let n = reader
            .read(&mut buf)
            .with_context(|| format!("reading from {url}"))?;
        if n == 0 {
            break;
        }
        tmp.as_file_mut().write_all(&buf[..n])?;
        if expected_sha256.is_some() {
            hasher.update(&buf[..n]);
        }
    }
    tmp.as_file_mut().flush()?;

    if let Some(expected) = expected_sha256 {
        let got = hex_of(hasher.finalize().as_slice());
        if !sha256_eq(&got, expected) {
            bail!(
                "sha256 mismatch downloading {url}: expected {expected}, got {got}"
            );
        }
    }

    let tmp_path = tmp.into_temp_path();
    tmp_path
        .persist(cached_path)
        .map_err(|e| anyhow!("persisting download to {}: {e}", cached_path.display()))?;
    Ok(())
}

fn stream_file_into_cache(
    src: &Path,
    cached_path: &Path,
    expected_sha256: Option<&str>,
) -> Result<()> {
    tracing::debug!("file:// fetch {} -> {}", src.display(), cached_path.display());
    if !src.is_file() {
        bail!("file:// source does not exist or is not a file: {}", src.display());
    }
    if let Some(expected) = expected_sha256 {
        verify_file_sha256(src, expected)?;
    }
    let parent = cached_path
        .parent()
        .ok_or_else(|| anyhow!("no parent for {}", cached_path.display()))?;
    std::fs::create_dir_all(parent)?;
    let tmp = tempfile::Builder::new()
        .prefix(".natron-dl-")
        .tempfile_in(parent)?;
    std::fs::copy(src, tmp.path())?;
    let tmp_path = tmp.into_temp_path();
    tmp_path
        .persist(cached_path)
        .map_err(|e| anyhow!("persisting file copy to {}: {e}", cached_path.display()))?;
    Ok(())
}

/// Compute sha256 of an existing file and compare to the expected hex string.
pub fn verify_file_sha256(path: &Path, expected: &str) -> Result<()> {
    let got = sha256_of_file(path)?;
    if !sha256_eq(&got, expected) {
        bail!(
            "sha256 mismatch for {}: expected {expected}, got {got}",
            path.display()
        );
    }
    Ok(())
}

pub fn sha256_of_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex_of(hasher.finalize().as_slice()))
}

fn hex_of(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        write!(s, "{b:02x}").unwrap();
    }
    s
}

fn sha256_eq(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn fetch_file_url_no_sha() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("payload.bin");
        std::fs::write(&src, b"hello world").unwrap();
        let cache = tmp.path().join("cache");
        let url = url::Url::from_file_path(&src).unwrap();
        let out = fetch(url.as_str(), None, &cache).unwrap();
        assert!(out.is_file());
        assert_eq!(std::fs::read(&out).unwrap(), b"hello world");
    }

    #[test]
    fn fetch_file_url_with_correct_sha() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("payload.bin");
        std::fs::write(&src, b"hello world").unwrap();
        let sha = sha256_of_file(&src).unwrap();
        let cache = tmp.path().join("cache");
        let url = url::Url::from_file_path(&src).unwrap();
        let out = fetch(url.as_str(), Some(&sha), &cache).unwrap();
        assert!(out.is_file());
    }

    #[test]
    fn fetch_file_url_rejects_wrong_sha() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("payload.bin");
        std::fs::write(&src, b"hello").unwrap();
        let cache = tmp.path().join("cache");
        let url = url::Url::from_file_path(&src).unwrap();
        let bogus = "0000000000000000000000000000000000000000000000000000000000000000";
        let err = fetch(url.as_str(), Some(bogus), &cache).unwrap_err();
        assert!(err.to_string().contains("sha256 mismatch"));
    }

    #[test]
    fn fetch_uses_cache_on_second_call() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("payload.bin");
        std::fs::write(&src, b"data").unwrap();
        let cache = tmp.path().join("cache");
        let url = url::Url::from_file_path(&src).unwrap();

        let p1 = fetch(url.as_str(), None, &cache).unwrap();
        let m1 = std::fs::metadata(&p1).unwrap().modified().unwrap();
        // Simulate src deletion; cache hit should still succeed.
        std::fs::remove_file(&src).unwrap();
        let p2 = fetch(url.as_str(), None, &cache).unwrap();
        let m2 = std::fs::metadata(&p2).unwrap().modified().unwrap();
        assert_eq!(p1, p2);
        assert_eq!(m1, m2, "cached file mtime should be unchanged");
    }

    #[test]
    fn fetch_redownloads_on_corrupted_cache_with_sha() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("payload.bin");
        std::fs::write(&src, b"valid").unwrap();
        let sha = sha256_of_file(&src).unwrap();
        let cache = tmp.path().join("cache");

        // Pre-populate the cache with corrupted content.
        std::fs::create_dir_all(&cache).unwrap();
        let url = url::Url::from_file_path(&src).unwrap();
        let cached_name = derive_cached_name(&url, Some(&sha));
        let cached = cache.join(&cached_name);
        std::fs::write(&cached, b"corrupt").unwrap();

        // Fetch should detect corruption, redownload, and verify.
        let out = fetch(url.as_str(), Some(&sha), &cache).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"valid");
    }

    #[test]
    fn fetch_rejects_unknown_scheme() {
        let tmp = TempDir::new().unwrap();
        let cache = tmp.path().join("cache");
        let err = fetch("ftp://example.com/foo", None, &cache).unwrap_err();
        assert!(err.to_string().contains("unsupported URL scheme"));
    }

    #[test]
    fn sha256_of_known_bytes() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("a");
        std::fs::write(&p, b"abc").unwrap();
        // Known SHA-256("abc") = ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad
        assert_eq!(
            sha256_of_file(&p).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
