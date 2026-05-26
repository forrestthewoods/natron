//! Tests for `src/cas.rs` (split out so the production
//! file shows only the implementation).

use super::*;
use tempfile::TempDir;

fn write(p: &Path, bytes: &[u8]) {
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(p, bytes).unwrap();
}

fn nlinks(path: &Path) -> u64 {
    // Cross-platform link count via metadata.
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).unwrap().nlink()
    }
    #[cfg(windows)]
    {
        // No portable nlink in stdlib on Windows; rely on a different
        // signal: byte-equality + same xxh3-128. Tests below exercise
        // dedupe via the report (`dedupe_hits`).
        // Return 1 so any code that asserts > 1 must be on Unix.
        let _ = path;
        1
    }
}

#[test]
fn hash_known_short_input() {
    let tmp = TempDir::new().unwrap();
    let p = tmp.path().join("a");
    write(&p, b"hello");
    let h1 = hash_file_xxh3_128(&p).unwrap();
    write(&p, b"hello"); // identical
    let h2 = hash_file_xxh3_128(&p).unwrap();
    assert_eq!(h1, h2, "hash must be deterministic");
    assert_eq!(h1.len(), 32, "32-char hex string");
    write(&p, b"goodbye");
    let h3 = hash_file_xxh3_128(&p).unwrap();
    assert_ne!(h1, h3);
}

#[test]
fn cas_run_dedupes_identical_files() {
    let tmp = TempDir::new().unwrap();
    let cache = Cache::at(tmp.path().join("cache"));
    cache.ensure_layout().unwrap();

    let raw = tmp.path().join("staging").join("raw");
    write(&raw.join("bin").join("clang"), b"binary-bytes");
    write(&raw.join("LICENSE"), b"shared-license-bytes");
    let tree = tmp.path().join("staging").join("tree");

    let report = run(&cache, &raw, &tree).unwrap();
    assert_eq!(report.files_processed, 2);
    assert_eq!(report.dedupe_hits, 0);

    // Run again with a *different* staging dir but overlapping content.
    let raw2 = tmp.path().join("staging2").join("raw");
    write(&raw2.join("bin").join("clang"), b"different-binary");
    write(&raw2.join("LICENSE"), b"shared-license-bytes"); // same content
    let tree2 = tmp.path().join("staging2").join("tree");

    let report2 = run(&cache, &raw2, &tree2).unwrap();
    assert_eq!(report2.files_processed, 2);
    assert_eq!(report2.dedupe_hits, 1, "LICENSE should have deduped");

    // Both LICENSE files are present and contain the right bytes.
    assert_eq!(
        std::fs::read(tree.join("LICENSE")).unwrap(),
        b"shared-license-bytes"
    );
    assert_eq!(
        std::fs::read(tree2.join("LICENSE")).unwrap(),
        b"shared-license-bytes"
    );

    // On Unix, both LICENSE files share an inode (hardlinked into CAS).
    #[cfg(unix)]
    {
        assert!(
            nlinks(&tree.join("LICENSE")) >= 2,
            "expected nlink>=2 on Unix"
        );
    }
}

#[test]
fn cas_run_handles_subdirs() {
    let tmp = TempDir::new().unwrap();
    let cache = Cache::at(tmp.path().join("cache"));
    cache.ensure_layout().unwrap();
    let raw = tmp.path().join("staging").join("raw");
    write(&raw.join("a").join("b").join("c.txt"), b"deep");
    write(&raw.join("top.txt"), b"shallow");
    let tree = tmp.path().join("staging").join("tree");
    let report = run(&cache, &raw, &tree).unwrap();
    assert_eq!(report.files_processed, 2);
    assert!(tree.join("a").join("b").join("c.txt").exists());
    assert!(tree.join("top.txt").exists());
}

#[test]
fn cas_run_creates_2hex_parent_lazily() {
    let tmp = TempDir::new().unwrap();
    let cache = Cache::at(tmp.path().join("cache"));
    cache.ensure_layout().unwrap();
    // No cas/<XX>/ subdir exists yet. Run must create it.
    let raw = tmp.path().join("staging").join("raw");
    write(&raw.join("file.txt"), b"unique");
    let tree = tmp.path().join("staging").join("tree");
    run(&cache, &raw, &tree).unwrap();
    // Find the (single) created cas/<2hex>/ dir.
    let mut entries: Vec<_> = std::fs::read_dir(&cache.cas).unwrap().collect();
    assert_eq!(entries.len(), 1);
    let prefix_dir = entries.remove(0).unwrap();
    let name = prefix_dir.file_name();
    let s = name.to_string_lossy();
    assert_eq!(s.len(), 2, "2-hex prefix dir, got '{s}'");
    assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn files_equal_known_cases() {
    let tmp = TempDir::new().unwrap();
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    let c = tmp.path().join("c");
    write(&a, b"hello");
    write(&b, b"hello");
    write(&c, b"world");
    assert!(files_equal(&a, &b).unwrap());
    assert!(!files_equal(&a, &c).unwrap());
}

#[test]
fn cas_run_existing_cas_blob_is_reused() {
    // Pre-populate the CAS with a file matching the content we'll
    // stage; run() should detect the existing blob and dedupe.
    let tmp = TempDir::new().unwrap();
    let cache = Cache::at(tmp.path().join("cache"));
    cache.ensure_layout().unwrap();

    let bytes = b"shared-content-payload";
    let preplant = tmp.path().join("preplant.bin");
    write(&preplant, bytes);
    let hex = hash_file_xxh3_128(&preplant).unwrap();
    let cas_target = cache.cas_path(&hex);
    std::fs::create_dir_all(cas_target.parent().unwrap()).unwrap();
    std::fs::copy(&preplant, &cas_target).unwrap();

    let raw = tmp.path().join("staging").join("raw");
    write(&raw.join("file.bin"), bytes);
    let tree = tmp.path().join("staging").join("tree");
    let report = run(&cache, &raw, &tree).unwrap();
    assert_eq!(report.files_processed, 1);
    assert_eq!(report.dedupe_hits, 1);
    assert_eq!(std::fs::read(tree.join("file.bin")).unwrap(), bytes);
}

#[allow(unused)]
fn assert_nlink_at_least(p: &Path, min: u64) {
    let n = nlinks(p);
    #[cfg(unix)]
    assert!(n >= min, "expected nlink>={min}, got {n} for {}", p.display());
    let _ = (n, min);
}
