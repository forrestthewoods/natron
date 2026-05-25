//! Tests for `src/download.rs` (split out so the production
//! file shows only the implementation).

use super::*;
use std::io::Read as IoRead;
use std::io::Write as IoWrite;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;
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
fn fetch_with_outcome_reports_downloaded_then_cached() {
    // First call: cache miss → Downloaded.
    // Second call (same URL+cache): cache hit → Cached.
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("payload.bin");
    std::fs::write(&src, b"abc").unwrap();
    let cache = tmp.path().join("cache");
    let url = url::Url::from_file_path(&src).unwrap();

    let (path1, source1) = fetch_with_outcome(url.as_str(), None, &cache).unwrap();
    assert!(path1.is_file());
    assert_eq!(source1, FetchSource::Downloaded);

    let (path2, source2) = fetch_with_outcome(url.as_str(), None, &cache).unwrap();
    assert_eq!(path1, path2);
    assert_eq!(source2, FetchSource::Cached);
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

// ---------------------------------------------------------------------------
// RetryPolicy math
// ---------------------------------------------------------------------------

#[test]
fn retry_policy_default_values() {
    let p = RetryPolicy::default();
    assert_eq!(p.initial_backoff_ms, 100);
    assert_eq!(p.max_total_backoff_ms, 30_000);
    assert!(p.jitter);
}

#[test]
fn retry_policy_no_jitter_doubles_each_attempt() {
    let p = RetryPolicy {
        initial_backoff_ms: 50,
        max_total_backoff_ms: 1_000_000, // budget irrelevant for compute_delay
        jitter: false,
    };
    assert_eq!(p.compute_delay(1), Duration::from_millis(50));
    assert_eq!(p.compute_delay(2), Duration::from_millis(100));
    assert_eq!(p.compute_delay(3), Duration::from_millis(200));
    assert_eq!(p.compute_delay(4), Duration::from_millis(400));
    assert_eq!(p.compute_delay(5), Duration::from_millis(800));
    assert_eq!(p.compute_delay(6), Duration::from_millis(1_600));
}

#[test]
fn retry_policy_jitter_stays_in_half_to_threehalves() {
    let p = RetryPolicy {
        initial_backoff_ms: 100,
        max_total_backoff_ms: 30_000,
        jitter: true,
    };
    // For attempt=3 the base would be 400ms (no jitter); jittered range is
    // 200..=600. Sample many times to ensure we stay inside.
    for _ in 0..200 {
        let d = p.compute_delay(3).as_millis() as u64;
        assert!((200..=600).contains(&d), "attempt=3 delay {d}ms outside 200..=600");
    }
}

#[test]
fn retry_policy_shift_clamp_prevents_overflow() {
    // attempt=50 would shift past u64 if not clamped; the shift.min(20)
    // guard pins the maximum to 100ms * 2^20 ≈ 104 seconds. Shifts saturate
    // starting at attempt=21 (shift = attempt-1 = 20, which is the clamp).
    let p = RetryPolicy {
        initial_backoff_ms: 100,
        max_total_backoff_ms: u64::MAX,
        jitter: false,
    };
    let at_clamp = p.compute_delay(21);
    let huge = p.compute_delay(50);
    assert_eq!(huge, at_clamp, "compute_delay saturates at attempt=21");
    assert_eq!(p.compute_delay(1000), at_clamp);
    // Sanity: bounded, doesn't panic.
    assert!(huge.as_millis() < 200_000_000);
}

#[test]
fn is_transient_msg_recognizes_common_failures() {
    assert!(is_transient_msg("No such host is known"));
    assert!(is_transient_msg("dns error"));
    assert!(is_transient_msg("connection reset by peer"));
    assert!(is_transient_msg("connection closed before message completed"));
    assert!(is_transient_msg("broken pipe"));
    assert!(is_transient_msg("unexpected EOF"));
    assert!(is_transient_msg("operation timed out"));
    assert!(is_transient_msg("Peer disconnected"));
    // Negative cases.
    assert!(!is_transient_msg("404 Not Found"));
    assert!(!is_transient_msg("invalid utf-8"));
}

// ---------------------------------------------------------------------------
// HTTP retry / resume against an in-process server
// ---------------------------------------------------------------------------

/// 4 KiB of deterministic body. Big enough to exceed one read buffer when
/// the production code uses 64 KiB but small enough to keep tests fast.
fn make_body(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i as u8).wrapping_mul(7).wrapping_add(3)).collect()
}

/// Build a deterministic policy that allows exactly `attempts` attempts
/// before exhausting the sleep budget. With initial=1ms and no jitter,
/// sleeps follow 1, 2, 4, 8, ... so the sum after N-1 sleeps is 2^(N-1) - 1.
/// Setting the budget to exactly that sum lets N attempts run; the N-th
/// would-be sleep of 2^(N-1) ms pushes the cumulative to 2*2^(N-1) - 1
/// which exceeds the budget and bails.
fn fast_test_policy(attempts: u32) -> RetryPolicy {
    let budget = if attempts == 0 {
        0
    } else {
        (1u64 << (attempts - 1)) - 1
    };
    RetryPolicy {
        initial_backoff_ms: 1,
        max_total_backoff_ms: budget,
        jitter: false,
    }
}

#[test]
fn http_full_download_succeeds() {
    let body = make_body(4096);
    let server = TestHttpServer::spawn(body.clone(), Behavior::Normal);
    let tmp = TempDir::new().unwrap();
    let cached = tmp.path().join("cache").join("out.bin");
    std::fs::create_dir_all(cached.parent().unwrap()).unwrap();

    stream_http_into_cache(&server.url(), &cached, None, fast_test_policy(3)).unwrap();

    assert_eq!(std::fs::read(&cached).unwrap(), body);
    assert_eq!(server.request_count(), 1, "single GET sufficed");
}

#[test]
fn http_resumes_after_mid_stream_drop_via_range() {
    let body = make_body(8192);
    let server = TestHttpServer::spawn(
        body.clone(),
        Behavior::DropFirst {
            drops_remaining: Arc::new(AtomicI32::new(1)),
            drop_after: 1024,
        },
    );
    let tmp = TempDir::new().unwrap();
    let cached = tmp.path().join("cache").join("out.bin");
    std::fs::create_dir_all(cached.parent().unwrap()).unwrap();

    stream_http_into_cache(&server.url(), &cached, None, fast_test_policy(4)).unwrap();

    assert_eq!(std::fs::read(&cached).unwrap(), body, "resumed file matches original");
    // Ordered subsequence assertion: the first request had no Range and the
    // first Range request asked for bytes=1024-. Spurious connect-RSTs on
    // Windows can produce extra requests in between; we just want to see the
    // resume path was actually exercised.
    let ranges = server.range_starts();
    assert!(ranges.iter().any(|r| *r == Some(1024)),
        "no Range: bytes=1024- request observed; got {ranges:?}");
}

#[test]
fn http_full_restart_when_server_ignores_range() {
    let body = make_body(4096);
    let server = TestHttpServer::spawn(
        body.clone(),
        Behavior::IgnoreRangeAndDropFirst {
            drops_remaining: Arc::new(AtomicI32::new(1)),
            drop_after: 1024,
        },
    );
    let tmp = TempDir::new().unwrap();
    let cached = tmp.path().join("cache").join("out.bin");
    std::fs::create_dir_all(cached.parent().unwrap()).unwrap();

    stream_http_into_cache(&server.url(), &cached, None, fast_test_policy(4)).unwrap();

    assert_eq!(std::fs::read(&cached).unwrap(), body, "full body recovered");
    // Functional check: the file was successfully re-downloaded after the
    // server refused to honor Range. Exact request counts are noisy on
    // Windows due to transient connect-RSTs.
    assert!(server.request_count() >= 2, "at least the initial drop + a recovery");
}

#[test]
fn http_persistent_failure_exhausts_attempts() {
    let body = make_body(2048);
    let server = TestHttpServer::spawn(
        body,
        Behavior::AlwaysDrop { drop_after: 256 },
    );
    let tmp = TempDir::new().unwrap();
    let cached = tmp.path().join("cache").join("out.bin");
    std::fs::create_dir_all(cached.parent().unwrap()).unwrap();

    let err = stream_http_into_cache(&server.url(), &cached, None, fast_test_policy(3))
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("failed after 3 attempts"),
        "expected exhausted-attempts message, got: {msg}"
    );
    assert!(!cached.is_file(), "tempfile not persisted on failure");
    // request_count is noisy: transient connect-RSTs on Windows can fail an
    // attempt before it reaches the server. We only require that at least one
    // attempt actually arrived.
    assert!(server.request_count() >= 1);
}

#[test]
fn http_resume_preserves_sha256_across_drops() {
    let body = make_body(6000);
    let mut hasher = sha2::Sha256::new();
    hasher.update(&body);
    let sha = hex_of(hasher.finalize().as_slice());

    let server = TestHttpServer::spawn(
        body.clone(),
        Behavior::DropFirst {
            drops_remaining: Arc::new(AtomicI32::new(2)),
            drop_after: 1500,
        },
    );
    let tmp = TempDir::new().unwrap();
    let cached = tmp.path().join("cache").join("out.bin");
    std::fs::create_dir_all(cached.parent().unwrap()).unwrap();

    stream_http_into_cache(&server.url(), &cached, Some(&sha), fast_test_policy(5)).unwrap();
    assert_eq!(std::fs::read(&cached).unwrap(), body);
}

#[test]
fn http_sha_mismatch_after_resume_errors() {
    let body = make_body(3000);
    let server = TestHttpServer::spawn(
        body,
        Behavior::DropFirst {
            drops_remaining: Arc::new(AtomicI32::new(1)),
            drop_after: 800,
        },
    );
    let tmp = TempDir::new().unwrap();
    let cached = tmp.path().join("cache").join("out.bin");
    std::fs::create_dir_all(cached.parent().unwrap()).unwrap();
    let bogus = "0000000000000000000000000000000000000000000000000000000000000000";

    let err = stream_http_into_cache(&server.url(), &cached, Some(bogus), fast_test_policy(4))
        .unwrap_err();
    assert!(err.to_string().contains("sha256 mismatch"));
}

// ---------------------------------------------------------------------------
// In-process HTTP fixture
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum Behavior {
    /// Serve full body on every request. Honor Range with 206.
    Normal,
    /// First N requests drop the connection after writing `drop_after` body
    /// bytes. Subsequent requests serve normally and honor Range.
    DropFirst {
        drops_remaining: Arc<AtomicI32>,
        drop_after: usize,
    },
    /// Always ignore Range and drop after N bytes for the first N requests.
    /// After drops_remaining hits zero, serves full body with 200.
    IgnoreRangeAndDropFirst {
        drops_remaining: Arc<AtomicI32>,
        drop_after: usize,
    },
    /// Drop after N body bytes on every request. Used to exhaust retries.
    AlwaysDrop { drop_after: usize },
}

struct TestHttpServer {
    addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    request_count: Arc<AtomicU32>,
    range_log: Arc<std::sync::Mutex<Vec<Option<u64>>>>,
}

impl TestHttpServer {
    fn spawn(body: Vec<u8>, behavior: Behavior) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let body = Arc::new(body);
        let shutdown = Arc::new(AtomicBool::new(false));
        let request_count = Arc::new(AtomicU32::new(0));
        let range_log: Arc<std::sync::Mutex<Vec<Option<u64>>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));

        let body_c = body.clone();
        let shutdown_c = shutdown.clone();
        let request_count_c = request_count.clone();
        let range_log_c = range_log.clone();

        let handle = thread::spawn(move || {
            loop {
                if shutdown_c.load(Ordering::SeqCst) {
                    return;
                }
                match listener.accept() {
                    Ok((stream, _)) => {
                        handle_conn(
                            stream,
                            body_c.clone(),
                            behavior.clone(),
                            request_count_c.clone(),
                            range_log_c.clone(),
                        );
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => return,
                }
            }
        });

        Self {
            addr,
            shutdown,
            handle: Some(handle),
            request_count,
            range_log,
        }
    }

    fn url(&self) -> String {
        format!("http://{}/file", self.addr)
    }

    fn request_count(&self) -> u32 {
        self.request_count.load(Ordering::SeqCst)
    }

    fn range_starts(&self) -> Vec<Option<u64>> {
        self.range_log.lock().unwrap().clone()
    }
}

impl Drop for TestHttpServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn handle_conn(
    mut stream: TcpStream,
    body: Arc<Vec<u8>>,
    behavior: Behavior,
    request_count: Arc<AtomicU32>,
    range_log: Arc<std::sync::Mutex<Vec<Option<u64>>>>,
) {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .ok();
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .ok();

    let req = match read_http_request(&mut stream) {
        Some(r) => r,
        None => return,
    };
    request_count.fetch_add(1, Ordering::SeqCst);

    let range_start = parse_range_start(&req);
    range_log.lock().unwrap().push(range_start);

    // Decide whether to honor Range and whether to drop.
    let honor_range = !matches!(behavior, Behavior::IgnoreRangeAndDropFirst { .. });
    let drop_after = match &behavior {
        Behavior::Normal => None,
        Behavior::DropFirst {
            drops_remaining,
            drop_after,
        } => {
            let prev = drops_remaining.fetch_sub(1, Ordering::SeqCst);
            if prev > 0 { Some(*drop_after) } else { None }
        }
        Behavior::IgnoreRangeAndDropFirst {
            drops_remaining,
            drop_after,
        } => {
            let prev = drops_remaining.fetch_sub(1, Ordering::SeqCst);
            if prev > 0 { Some(*drop_after) } else { None }
        }
        Behavior::AlwaysDrop { drop_after } => Some(*drop_after),
    };

    let effective_start = if honor_range { range_start.unwrap_or(0) } else { 0 };
    if (effective_start as usize) > body.len() {
        let _ = write!(
            stream,
            "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        return;
    }
    let slice = &body[effective_start as usize..];

    let status_line = if honor_range && range_start.is_some() {
        "HTTP/1.1 206 Partial Content"
    } else {
        "HTTP/1.1 200 OK"
    };

    let _ = write!(stream, "{status_line}\r\n");
    let _ = write!(stream, "Content-Length: {}\r\n", slice.len());
    if status_line.contains("206") {
        let _ = write!(
            stream,
            "Content-Range: bytes {}-{}/{}\r\n",
            effective_start,
            body.len() - 1,
            body.len()
        );
    }
    let _ = write!(stream, "Connection: close\r\n\r\n");

    if let Some(n) = drop_after {
        let n = n.min(slice.len());
        let _ = stream.write_all(&slice[..n]);
        let _ = stream.flush();
        let _ = stream.shutdown(std::net::Shutdown::Both);
    } else {
        let _ = stream.write_all(slice);
        let _ = stream.flush();
    }
}

fn read_http_request(stream: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::with_capacity(1024);
    let mut tmp = [0u8; 256];
    loop {
        let n = match stream.read(&mut tmp) {
            Ok(0) => return None,
            Ok(n) => n,
            Err(_) => return None,
        };
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > 16 * 1024 {
            return None;
        }
    }
    String::from_utf8(buf).ok()
}

fn parse_range_start(req: &str) -> Option<u64> {
    for line in req.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("range:") {
            let value = rest.trim();
            if let Some(rest) = value.strip_prefix("bytes=") {
                let start = rest.split('-').next()?.trim();
                return start.parse::<u64>().ok();
            }
        }
    }
    None
}
