//! Tests for `src/providers\zig.rs` (split out so the production
//! file shows only the implementation).

use super::*;
use crate::cache::Cache;
use crate::download;
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

fn write_index_json(
    index_path: &Path,
    version: &str,
    platform: &str,
    tarball_url: &str,
    sha: &str,
) {
    let json = format!(
        r#"{{"{version}":{{"{platform}":{{"tarball":"{tarball_url}","shasum":"{sha}"}}}}}}"#
    );
    std::fs::write(index_path, json).unwrap();
}

#[test]
fn zig_provider_id() {
    assert_eq!(ZigProvider::new().id(), "zig");
}

#[test]
fn zig_provider_full_install() {
    let tmp = TempDir::new().unwrap();
    // Build a synthetic zig zip with the conventional top-level dir.
    let archive = tmp.path().join("zig-windows-x86_64-0.15.2.zip");
    build_zip(
        &archive,
        &[
            ("zig-windows-x86_64-0.15.2/zig.exe", b"ZIG"),
            ("zig-windows-x86_64-0.15.2/lib/std.zig", b"STD"),
        ],
    );
    // Compute its sha so the index can claim a correct hash.
    let sha = download::sha256_of_file(&archive).unwrap();

    // Build a fake index.json pointing at the local archive.
    let index_path = tmp.path().join("index.json");
    let archive_url = url::Url::from_file_path(&archive).unwrap().to_string();
    write_index_json(
        &index_path,
        "0.15.2",
        "x86_64-windows",
        &archive_url,
        &sha,
    );
    let index_url = url::Url::from_file_path(&index_path).unwrap().to_string();

    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let mut ctx = InstallCtx::new(cache);

    let mut opts = toml::Table::new();
    opts.insert("version".into(), toml::Value::String("0.15.2".into()));
    opts.insert("platform".into(), toml::Value::String("x86_64-windows".into()));

    let provider = ZigProvider::with_index_url(index_url);
    let installed = provider.install(&opts, &mut ctx).unwrap();
    assert!(installed.freshly_extracted);
    assert_eq!(installed.fingerprint, "zig-0.15.2-x86_64-windows");

    // Strip prefix should have been auto-derived; zig.exe sits at the
    // staging root rather than in a nested dir.
    let raw = ctx.staging_dir().unwrap();
    assert_eq!(fs::read(raw.join("zig.exe")).unwrap(), b"ZIG");
    assert_eq!(fs::read(raw.join("lib").join("std.zig")).unwrap(), b"STD");
    assert!(!raw.join("zig-windows-x86_64-0.15.2").exists());
}

#[test]
fn zig_provider_missing_version_in_index() {
    let tmp = TempDir::new().unwrap();
    let index_path = tmp.path().join("index.json");
    std::fs::write(&index_path, r#"{"0.15.2":{"x86_64-linux":{"tarball":"x","shasum":"y"}}}"#).unwrap();
    let index_url = url::Url::from_file_path(&index_path).unwrap().to_string();

    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let mut ctx = InstallCtx::new(cache);
    let mut opts = toml::Table::new();
    opts.insert("version".into(), toml::Value::String("9.9.9".into()));
    opts.insert("platform".into(), toml::Value::String("x86_64-linux".into()));
    let err = ZigProvider::with_index_url(index_url)
        .install(&opts, &mut ctx)
        .unwrap_err();
    assert!(err.to_string().contains("no entry"));
}

#[test]
fn zig_provider_required_fields() {
    let tmp = TempDir::new().unwrap();
    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let mut ctx = InstallCtx::new(cache);
    let opts = toml::Table::new();
    let err = ZigProvider::new().install(&opts, &mut ctx).unwrap_err();
    assert!(err.to_string().contains("version"));
}

#[test]
fn zig_derive_strip_prefix_works() {
    assert_eq!(
        derive_strip_prefix("zig-windows-x86_64-0.15.2.zip", ArchiveKind::Zip),
        Some("zig-windows-x86_64-0.15.2".into())
    );
    assert_eq!(
        derive_strip_prefix("zig-linux-0.15.2.tar.xz", ArchiveKind::TarXz),
        Some("zig-linux-0.15.2".into())
    );
}

#[test]
fn zig_filename_from_url() {
    assert_eq!(
        filename_from_url("https://ziglang.org/download/0.15.2/zig-windows-x86_64-0.15.2.zip")
            .unwrap(),
        "zig-windows-x86_64-0.15.2.zip"
    );
}

#[test]
fn zig_provider_sha_mismatch_in_index() {
    let tmp = TempDir::new().unwrap();
    let archive = tmp.path().join("z.zip");
    build_zip(&archive, &[("zig.exe", b"X")]);
    let index_path = tmp.path().join("index.json");
    let archive_url = url::Url::from_file_path(&archive).unwrap().to_string();
    write_index_json(
        &index_path,
        "0.15.2",
        "p",
        &archive_url,
        "0000000000000000000000000000000000000000000000000000000000000000",
    );
    let index_url = url::Url::from_file_path(&index_path).unwrap().to_string();

    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let mut ctx = InstallCtx::new(cache);
    let mut opts = toml::Table::new();
    opts.insert("version".into(), toml::Value::String("0.15.2".into()));
    opts.insert("platform".into(), toml::Value::String("p".into()));
    let err = ZigProvider::with_index_url(index_url)
        .install(&opts, &mut ctx)
        .unwrap_err();
    assert!(err.to_string().contains("sha256 mismatch"));
}
