//! Tests for `src/providers\url.rs` (split out so the production
//! file shows only the implementation).

use super::*;
use crate::cache::Cache;
use std::fs::File;
use std::io::Write as _;
use std::path::Path;
use tempfile::TempDir;

fn build_zip(path: &Path) {
    let f = File::create(path).unwrap();
    let mut zw = zip::ZipWriter::new(f);
    let opts = zip::write::FileOptions::default();
    zw.start_file("nasm.exe", opts).unwrap();
    zw.write_all(b"fake-nasm").unwrap();
    zw.start_file("LICENSE", opts).unwrap();
    zw.write_all(b"license-text").unwrap();
    zw.finish().unwrap();
}

#[test]
fn url_provider_id() {
    assert_eq!(UrlProvider::new().id(), "url");
}

#[test]
fn url_provider_installs_from_file_url() {
    let tmp = TempDir::new().unwrap();
    let archive = tmp.path().join("nasm-3.01.zip");
    build_zip(&archive);
    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let mut ctx = InstallCtx::new(cache.clone());

    let url = url::Url::from_file_path(&archive).unwrap().to_string();
    let mut opts = toml::Table::new();
    opts.insert("url".into(), toml::Value::String(url));

    let provider = UrlProvider::new();
    let installed = provider.install(&opts, &mut ctx).unwrap();
    assert!(installed.freshly_extracted);
    assert!(installed.fingerprint.starts_with("url-nasm-3.01-"));

    // Staging contains the extracted files.
    let raw = ctx.staging_dir().unwrap();
    assert_eq!(std::fs::read(raw.join("nasm.exe")).unwrap(), b"fake-nasm");
}

#[test]
fn url_provider_strip_prefix_works() {
    let tmp = TempDir::new().unwrap();
    let archive = tmp.path().join("foo.zip");
    let f = File::create(&archive).unwrap();
    let mut zw = zip::ZipWriter::new(f);
    let opts = zip::write::FileOptions::default();
    zw.start_file("foo-1.0/bin/foo", opts).unwrap();
    zw.write_all(b"!").unwrap();
    zw.finish().unwrap();

    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let mut ctx = InstallCtx::new(cache);

    let url = url::Url::from_file_path(&archive).unwrap().to_string();
    let mut opts = toml::Table::new();
    opts.insert("url".into(), toml::Value::String(url));
    opts.insert(
        "strip_prefix".into(),
        toml::Value::String("foo-1.0".into()),
    );

    let provider = UrlProvider::new();
    provider.install(&opts, &mut ctx).unwrap();
    let raw = ctx.staging_dir().unwrap();
    assert_eq!(std::fs::read(raw.join("bin").join("foo")).unwrap(), b"!");
}

#[test]
fn url_provider_archive_kind_inference() {
    // Test the helper directly with various filenames.
    let mk = |archive: Option<&str>, url: &str| {
        let mut t = toml::Table::new();
        if let Some(a) = archive {
            t.insert("archive".into(), toml::Value::String(a.into()));
        }
        resolve_archive_kind(&t, url)
    };
    assert_eq!(mk(None, "https://x/foo.zip").unwrap(), ArchiveKind::Zip);
    assert_eq!(mk(None, "https://x/foo.tar.xz").unwrap(), ArchiveKind::TarXz);
    assert_eq!(mk(None, "https://x/foo.tgz").unwrap(), ArchiveKind::TarGz);
    assert!(mk(None, "https://x/foo.bin").is_err());
    // explicit archive overrides
    assert_eq!(
        mk(Some("zip"), "https://x/foo.bin").unwrap(),
        ArchiveKind::Zip
    );
}

#[test]
fn url_provider_missing_url_errors() {
    let tmp = TempDir::new().unwrap();
    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let mut ctx = InstallCtx::new(cache);
    let opts = toml::Table::new();
    let err = UrlProvider::new().install(&opts, &mut ctx).unwrap_err();
    assert!(err.to_string().contains("requires options.url"));
}

#[test]
fn url_provider_sha_pin_mismatch_errors() {
    let tmp = TempDir::new().unwrap();
    let archive = tmp.path().join("foo.zip");
    build_zip(&archive);
    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let mut ctx = InstallCtx::new(cache);

    let url = url::Url::from_file_path(&archive).unwrap().to_string();
    let mut opts = toml::Table::new();
    opts.insert("url".into(), toml::Value::String(url));
    opts.insert(
        "sha256".into(),
        toml::Value::String(
            "0000000000000000000000000000000000000000000000000000000000000000".into(),
        ),
    );

    let err = UrlProvider::new().install(&opts, &mut ctx).unwrap_err();
    assert!(err.to_string().contains("sha256 mismatch"));
}

#[test]
fn url_provider_fingerprint_changes_with_strip_prefix() {
    let url = "https://example.com/foo.zip";
    let a = compute_fingerprint(url, &None, ArchiveKind::Zip);
    let b = compute_fingerprint(url, &Some("foo-1".into()), ArchiveKind::Zip);
    assert_ne!(a, b);
}
