//! HTTP/HTTPS + `file://` download helper with sha256 streaming verification
//! and a URL/sha256-keyed local cache under `<cache>/downloads/`.

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use url::Url;

/// Whether `fetch_with_outcome` served the request from cache or did a
/// fresh network/file copy. Useful for "X downloaded, Y cached"
/// reporting in install loops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchSource {
    Cached,
    Downloaded,
}

/// Fetch `url` into the `cache` directory and return the absolute path of the
/// cached file. If `expected_sha256` is provided, the download stream is
/// verified against it; if a cached file exists with the right hash, no
/// network call is made.
///
/// Cache key: when sha256 is known up-front we use it as the filename. When
/// it's not known, we use `<8-hex-of-url-hash>-<basename>`. On hit we either
/// re-verify (if expected sha is supplied) or trust the cache.
pub fn fetch(url: &str, expected_sha256: Option<&str>, cache: &Path) -> Result<PathBuf> {
    fetch_with_outcome(url, expected_sha256, cache).map(|(p, _)| p)
}

/// Same as [`fetch`], but additionally reports whether the result was
/// served from cache or required a fresh download. Lets callers track
/// per-install cache-hit ratios.
pub fn fetch_with_outcome(
    url: &str,
    expected_sha256: Option<&str>,
    cache: &Path,
) -> Result<(PathBuf, FetchSource)> {
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
                    return Ok((cached_path, FetchSource::Cached));
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
            return Ok((cached_path, FetchSource::Cached));
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
            stream_http_into_cache(url, &cached_path, expected_sha256, RetryPolicy::default())?;
        }
        other => bail!("unsupported URL scheme: {other}"),
    }

    Ok((cached_path, FetchSource::Downloaded))
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

/// Tunable retry behavior for HTTP downloads.
///
/// The two knobs are the **initial backoff** and the **total backoff
/// budget** — the number of attempts isn't configured directly, it's
/// whatever fits inside the budget given the exponential schedule. With
/// the defaults (100ms initial, 30s budget) the loop gets ~8-9 attempts
/// before giving up, spending no more than 30s of cumulative
/// `thread::sleep` between them. The actual transfer time per attempt
/// is not counted against the budget — a legitimately slow download
/// won't be cut short.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RetryPolicy {
    pub initial_backoff_ms: u64,
    pub max_total_backoff_ms: u64,
    pub jitter: bool,
}

impl RetryPolicy {
    pub(crate) const fn default() -> Self {
        Self {
            initial_backoff_ms: 100,
            max_total_backoff_ms: 30_000,
            jitter: true,
        }
    }

    /// Delay before the next retry, given the 1-indexed `attempt` number
    /// that just failed. Exponential (doubling each attempt), optionally
    /// multiplied by a 0.5..1.5 jitter factor. The `shift.min(20)` guard
    /// prevents u64 overflow in pathological loops; the budget check in
    /// the retry loop is the real ceiling on individual sleep length.
    pub(crate) fn compute_delay(&self, attempt: u32) -> Duration {
        let shift = attempt.saturating_sub(1).min(20);
        let base = self.initial_backoff_ms.saturating_mul(1u64 << shift);
        let final_ms = if self.jitter {
            let frac = pseudo_random_unit();
            ((base as f64) * (0.5 + frac)) as u64
        } else {
            base
        };
        Duration::from_millis(final_ms)
    }
}

/// 0..1 quasi-random value for jitter. Quality matters only enough to
/// desynchronize concurrent retries hitting the same upstream.
fn pseudo_random_unit() -> f64 {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let h = xxhash_rust::xxh3::xxh3_64(&nanos.to_le_bytes());
    (h as f64) / (u64::MAX as f64)
}

/// Result of one HTTP attempt inside the retry loop.
enum StreamErr {
    /// Network blip; back off and retry. If `bytes_received > 0`, the next
    /// attempt sends `Range:` to resume.
    Transient(anyhow::Error),
    /// Server responded but didn't honor `Range` (returned 200 not 206, or
    /// 416). Truncate the tempfile, reset the hasher, retry from byte 0.
    ResetRequired(String),
    /// Permanent error; surface immediately.
    Fatal(anyhow::Error),
}

fn stream_http_into_cache(
    url: &str,
    cached_path: &Path,
    expected_sha256: Option<&str>,
    policy: RetryPolicy,
) -> Result<()> {
    tracing::info!("downloading {url}");
    let parent = cached_path
        .parent()
        .ok_or_else(|| anyhow!("no parent for {}", cached_path.display()))?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".natron-dl-")
        .tempfile_in(parent)
        .with_context(|| format!("creating tempfile in {}", parent.display()))?;

    let mut hasher = Sha256::new();
    let mut bytes_received: u64 = 0;
    let verify = expected_sha256.is_some();
    let mut attempt: u32 = 1;
    let mut total_slept_ms: u64 = 0;

    // Loop terminates one of three ways: Ok, Fatal, or budget exhaustion on
    // a Transient. ResetRequired can't loop forever — it requires
    // `bytes_received > 0`, and we reset that to 0 each time it fires.
    loop {
        match stream_once(url, &mut tmp, &mut hasher, &mut bytes_received, verify) {
            Ok(()) => {
                if let Some(expected) = expected_sha256 {
                    let got = hex_of(hasher.finalize_reset().as_slice());
                    if !sha256_eq(&got, expected) {
                        bail!(
                            "sha256 mismatch downloading {url}: expected {expected}, got {got}"
                        );
                    }
                }
                tmp.as_file_mut().flush()?;
                let tmp_path = tmp.into_temp_path();
                tmp_path
                    .persist(cached_path)
                    .map_err(|e| anyhow!("persisting download to {}: {e}", cached_path.display()))?;
                return Ok(());
            }
            Err(StreamErr::Transient(err)) => {
                let delay = policy.compute_delay(attempt);
                let delay_ms = delay.as_millis() as u64;
                let new_total = total_slept_ms.saturating_add(delay_ms);
                if new_total > policy.max_total_backoff_ms {
                    return Err(err.context(format!(
                        "{url} failed after {attempt} attempts ({total_slept_ms}ms of retry budget exhausted)"
                    )));
                }
                if bytes_received > 0 {
                    tracing::warn!(
                        "transient error on {url} after {bytes_received} bytes (attempt {attempt}): {err}; retrying in {delay_ms}ms with Range"
                    );
                } else {
                    tracing::warn!(
                        "transient error on {url} (attempt {attempt}): {err}; retrying in {delay_ms}ms"
                    );
                }
                std::thread::sleep(delay);
                total_slept_ms = new_total;
            }
            Err(StreamErr::ResetRequired(reason)) => {
                tracing::warn!(
                    "{reason} (attempt {attempt}); restarting from byte 0"
                );
                bytes_received = 0;
                hasher = Sha256::new();
                tmp.as_file_mut().set_len(0)?;
                tmp.as_file_mut().seek(SeekFrom::Start(0))?;
                // No sleep — server is responsive, just non-cooperative.
            }
            Err(StreamErr::Fatal(err)) => return Err(err),
        }
        attempt = attempt.saturating_add(1);
    }
}

/// One HTTP attempt. Sends `Range: bytes=N-` when `*bytes_received > 0`.
/// Updates `tmp`, `hasher`, and `*bytes_received` in place; the retry loop
/// reuses these across attempts.
fn stream_once(
    url: &str,
    tmp: &mut tempfile::NamedTempFile,
    hasher: &mut Sha256,
    bytes_received: &mut u64,
    verify_sha: bool,
) -> std::result::Result<(), StreamErr> {
    let mut req = ureq::get(url);
    if *bytes_received > 0 {
        req = req.header("Range", format!("bytes={}-", *bytes_received));
    }
    let resp = match req.call() {
        Ok(r) => r,
        Err(err) => {
            let msg = err.to_string();
            return if is_transient_msg(&msg) {
                Err(StreamErr::Transient(anyhow!("HTTP GET {url}: {err}")))
            } else {
                Err(StreamErr::Fatal(anyhow!("HTTP GET {url}: {err}")))
            };
        }
    };

    let status = resp.status();
    let code = status.as_u16();
    if code == 200 && *bytes_received > 0 {
        return Err(StreamErr::ResetRequired(format!(
            "server returned 200 to Range request for {url}"
        )));
    }
    if code == 416 {
        return Err(StreamErr::ResetRequired(format!(
            "HTTP 416 (range not satisfiable) for {url}"
        )));
    }
    if code >= 400 {
        return Err(StreamErr::Fatal(anyhow!("HTTP {status} for {url}")));
    }

    let advertised_len: Option<u64> = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());

    let mut reader = resp.into_body().into_reader();
    let mut buf = vec![0u8; 64 * 1024];
    let mut chunk_read: u64 = 0;

    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                let msg = e.to_string();
                if is_transient_msg(&msg) {
                    return Err(StreamErr::Transient(anyhow!(
                        "reading body of {url} after {} bytes: {e}",
                        *bytes_received + chunk_read
                    )));
                }
                return Err(StreamErr::Fatal(anyhow!("reading body of {url}: {e}")));
            }
        };
        if let Err(e) = tmp.as_file_mut().write_all(&buf[..n]) {
            return Err(StreamErr::Fatal(anyhow!("write tempfile: {e}")));
        }
        if verify_sha {
            hasher.update(&buf[..n]);
        }
        *bytes_received += n as u64;
        chunk_read += n as u64;
    }

    // Detect short body: stream EOF'd before Content-Length was satisfied.
    // Some HTTP libraries surface this as a read error; some return Ok(0).
    if let Some(expected) = advertised_len {
        if chunk_read < expected {
            return Err(StreamErr::Transient(anyhow!(
                "short read from {url}: got {chunk_read} of {expected} bytes in this chunk"
            )));
        }
    }

    Ok(())
}

/// Pattern-matches transient network failure strings. ureq v3 doesn't expose
/// a structured error kind we can switch on cleanly, so we sniff the message.
fn is_transient_msg(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("no such host is known")
        || m.contains("dns error")
        || m.contains("resolve")
        || m.contains("connection reset")
        || m.contains("connection closed")
        || m.contains("connection aborted")
        || m.contains("connection was aborted")
        || m.contains("established connection")
        || m.contains("forcibly closed")
        || m.contains("broken pipe")
        || m.contains("unexpected eof")
        || m.contains("timed out")
        || m.contains("timeout")
        || m.contains("peer disconnected")
        || m.contains("peer closed")
        || m.contains("end of file")
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
#[path = "tests/download.rs"]
mod tests;
