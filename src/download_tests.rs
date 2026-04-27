//! Tests for `src/download.rs` (split out so the production
//! file shows only the implementation).

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
