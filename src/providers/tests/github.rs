//! Tests for `src/providers\github.rs` (split out so the production
//! file shows only the implementation).

use super::*;
use crate::cache::Cache;
use std::fs;
use std::fs::File;
use std::io::Write as _;
use std::path::Path;
use tempfile::TempDir;

fn build_zip(path: &Path, entries: &[(&str, &[u8])]) {
    let f = File::create(path).unwrap();
    let mut zw = zip::ZipWriter::new(f);
    let opts = zip::write::FileOptions::default();
    for (name, bytes) in entries {
        zw.start_file(*name, opts).unwrap();
        zw.write_all(bytes).unwrap();
    }
    zw.finish().unwrap();
}

/// Write a fake GitHub release-info JSON. The asset's
/// browser_download_url points at the local archive file.
fn write_release_json(
    api_base_dir: &Path,
    repo: &str,
    tag: &str,
    asset_name: &str,
    archive_path: &Path,
) {
    let path = api_base_dir
        .join("repos")
        .join(repo)
        .join("releases")
        .join("tags")
        .join(tag);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let asset_url = url::Url::from_file_path(archive_path).unwrap().to_string();
    let json = format!(
        r#"{{"tag_name":"{tag}","assets":[{{"name":"{asset_name}","browser_download_url":"{asset_url}"}}]}}"#
    );
    fs::write(&path, json).unwrap();
}

#[test]
fn github_provider_id() {
    assert_eq!(GithubProvider::new().id(), "github");
}

#[test]
fn github_provider_resolves_release_and_extracts() {
    let tmp = TempDir::new().unwrap();
    let api = tmp.path().join("api");
    let archive = tmp.path().join("clang.zip");
    build_zip(&archive, &[("bin/clang.exe", b"BIN"), ("LICENSE", b"LIC")]);
    write_release_json(&api, "llvm/llvm-project", "llvmorg-21.1.6", "clang.zip", &archive);

    let api_base = url::Url::from_directory_path(&api).unwrap().to_string();
    // Trim trailing slash because we add one when constructing URLs.
    let api_base = api_base.trim_end_matches('/').to_string();

    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let mut ctx = InstallCtx::new(cache);

    let mut opts = toml::Table::new();
    opts.insert("repo".into(), toml::Value::String("llvm/llvm-project".into()));
    opts.insert("tag".into(), toml::Value::String("llvmorg-21.1.6".into()));
    opts.insert("asset".into(), toml::Value::String("clang.zip".into()));

    let provider = GithubProvider::with_api_base(api_base);
    let installed = provider.install(&opts, &mut ctx).unwrap();
    assert!(installed.freshly_extracted);
    assert!(installed.fingerprint.contains("llvmorg-21.1.6"));

    let raw = ctx.staging_dir().unwrap();
    assert_eq!(fs::read(raw.join("bin").join("clang.exe")).unwrap(), b"BIN");
}

#[test]
fn github_provider_missing_asset_errors() {
    let tmp = TempDir::new().unwrap();
    let api = tmp.path().join("api");
    let archive = tmp.path().join("real.zip");
    build_zip(&archive, &[("f", b"F")]);
    write_release_json(&api, "owner/repo", "v1", "real.zip", &archive);

    let api_base = url::Url::from_directory_path(&api).unwrap().to_string();
    let api_base = api_base.trim_end_matches('/').to_string();

    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let mut ctx = InstallCtx::new(cache);

    let mut opts = toml::Table::new();
    opts.insert("repo".into(), toml::Value::String("owner/repo".into()));
    opts.insert("tag".into(), toml::Value::String("v1".into()));
    opts.insert("asset".into(), toml::Value::String("does-not-exist.zip".into()));

    let provider = GithubProvider::with_api_base(api_base);
    let err = provider.install(&opts, &mut ctx).unwrap_err();
    assert!(err.to_string().contains("not found"));
}

#[test]
fn github_provider_required_fields() {
    let tmp = TempDir::new().unwrap();
    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let mut ctx = InstallCtx::new(cache);
    let opts = toml::Table::new();
    let err = GithubProvider::new().install(&opts, &mut ctx).unwrap_err();
    assert!(err.to_string().contains("repo"));
}

#[test]
fn github_fingerprint_deterministic() {
    let a = compute_fingerprint("llvm/llvm-project", "llvmorg-21.1.6", "clang.tar.xz");
    let b = compute_fingerprint("llvm/llvm-project", "llvmorg-21.1.6", "clang.tar.xz");
    assert_eq!(a, b);
    let c = compute_fingerprint("llvm/llvm-project", "llvmorg-18.1.8", "clang.tar.xz");
    assert_ne!(a, c);
}

#[test]
fn github_archive_kind_inference() {
    let mk = |asset: &str| {
        let opts = toml::Table::new();
        resolve_archive_kind(&opts, asset)
    };
    assert_eq!(mk("foo.tar.xz").unwrap(), ArchiveKind::TarXz);
    assert_eq!(mk("foo.zip").unwrap(), ArchiveKind::Zip);
    assert!(mk("foo.exe").is_err());
}

#[test]
fn github_cache_hit_short_circuits() {
    let tmp = TempDir::new().unwrap();
    let api = tmp.path().join("api");
    let archive = tmp.path().join("a.zip");
    build_zip(&archive, &[("f", b"F")]);
    write_release_json(&api, "x/y", "v", "a.zip", &archive);
    let api_base = url::Url::from_directory_path(&api).unwrap().to_string();
    let api_base = api_base.trim_end_matches('/').to_string();

    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let mut opts = toml::Table::new();
    opts.insert("repo".into(), toml::Value::String("x/y".into()));
    opts.insert("tag".into(), toml::Value::String("v".into()));
    opts.insert("asset".into(), toml::Value::String("a.zip".into()));

    // Pre-plant the install dir + metadata to simulate a prior install.
    let fp = compute_fingerprint("x/y", "v", "a.zip");
    let sanitized = crate::cache::sanitize_fingerprint(&fp);
    let install_dir = cache.install_dir(&sanitized);
    fs::create_dir_all(install_dir.join("tree")).unwrap();
    let md = crate::cache::InstallMetadata::new(
        "github",
        sanitized.clone(),
        "test",
        opts.clone(),
    );
    md.write(&cache.install_metadata_path(&sanitized)).unwrap();

    let mut ctx = InstallCtx::new(cache);
    let provider = GithubProvider::with_api_base(api_base);
    let installed = provider.install(&opts, &mut ctx).unwrap();
    assert!(!installed.freshly_extracted, "should have hit cache");
    // No staging dir should have been allocated.
    assert!(ctx.staging_root().is_none());
}
