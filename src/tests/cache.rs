//! Tests for `src/cache.rs` (split out so the production
//! file shows only the implementation).

use super::*;
use tempfile::TempDir;

#[test]
fn sanitize_passes_through_clean_strings() {
    let s = sanitize_fingerprint("zig-0.15.2-x86_64-windows");
    assert_eq!(s, "zig-0.15.2-x86_64-windows");
}

#[test]
fn sanitize_replaces_bad_chars_and_appends_hash() {
    let dirty = "github-llvm/llvm-project-llvmorg-21.1.6-clang+llvm";
    let clean = sanitize_fingerprint(dirty);
    assert!(!clean.contains('/'));
    assert!(!clean.contains('+'));
    // Suffix is exactly 8 hex chars.
    let parts: Vec<&str> = clean.rsplitn(2, '-').collect();
    assert_eq!(parts[0].len(), 8);
    assert!(parts[0].chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn sanitize_distinct_inputs_produce_distinct_outputs() {
    let a = sanitize_fingerprint("foo+bar");
    let b = sanitize_fingerprint("foo*bar");
    assert_ne!(a, b);
}

#[test]
fn ensure_layout_creates_subdirs() {
    let tmp = TempDir::new().unwrap();
    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    assert!(cache.installs.is_dir());
    assert!(cache.cas.is_dir());
    assert!(cache.downloads.is_dir());
    assert!(cache.staging.is_dir());
    assert!(cache.meta.is_dir());
}

#[test]
fn install_paths_compose() {
    let cache = Cache::at("/tmp/c");
    let d = cache.install_dir("zig-0.15.2");
    assert!(d.ends_with("installs/zig-0.15.2") || d.ends_with(r"installs\zig-0.15.2"));
    assert!(cache
        .install_tree("zig-0.15.2")
        .ends_with(format!("zig-0.15.2{}tree", std::path::MAIN_SEPARATOR)));
}

#[test]
fn cas_path_uses_2hex_prefix() {
    let cache = Cache::at("/tmp/c");
    let p = cache.cas_path("ab1234567890");
    let s = p.to_string_lossy();
    assert!(s.contains("cas") && s.contains("ab"));
}

#[test]
fn metadata_round_trip() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("metadata.toml");
    let mut opts = toml::Table::new();
    opts.insert("repo".to_string(), toml::Value::String("foo/bar".into()));
    opts.insert("tag".to_string(), toml::Value::String("v1".into()));
    let md = InstallMetadata::new("github", "github-foo-bar-v1", "foo v1", opts);
    md.write(&path).unwrap();
    let loaded = InstallMetadata::read(&path).unwrap();
    assert_eq!(loaded.provider, "github");
    assert_eq!(loaded.fingerprint, "github-foo-bar-v1");
    assert_eq!(loaded.options.get("repo").and_then(|v| v.as_str()), Some("foo/bar"));
}

#[test]
fn metadata_rejects_unknown_schema_version() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("metadata.toml");
    std::fs::write(
        &path,
        "schema_version = 999\nprovider = \"x\"\nfingerprint = \"y\"\ndisplay = \"z\"\ninstalled_at = 2026-01-01T00:00:00Z\ntool_version = \"0.0.0\"\n[options]\n",
    )
    .unwrap();
    let err = InstallMetadata::read(&path).unwrap_err();
    assert!(err.to_string().contains("schema_version=999"));
}

#[test]
fn allocate_staging_creates_unique_dirs() {
    let tmp = TempDir::new().unwrap();
    let cache = Cache::at(tmp.path());
    cache.ensure_layout().unwrap();
    let a = cache.allocate_staging().unwrap();
    let b = cache.allocate_staging().unwrap();
    assert_ne!(a, b);
    assert!(a.is_dir());
    assert!(b.is_dir());
}

#[test]
fn ymd_known_dates() {
    // 1970-01-01 = day 0
    assert_eq!(ymd_from_days(0), (1970, 1, 1));
    // 2000-01-01 = day 10957
    assert_eq!(ymd_from_days(10957), (2000, 1, 1));
    // Round-trip the formatter on epoch zero.
    assert_eq!(format_unix_seconds(0), "1970-01-01T00:00:00Z");
    // Round-trip for 2024-02-29 (leap day): days = 19782
    let secs_leap = 19782u64 * 86_400;
    let s = format_unix_seconds(secs_leap);
    assert!(s.starts_with("2024-02-29"), "got {s}");
}
