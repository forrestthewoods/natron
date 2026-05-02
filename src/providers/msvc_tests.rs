//! Tests for `src/providers\msvc.rs` (split out so the production
//! file shows only the implementation).

use super::*;
use crate::cache::Cache;
use std::path::Path;
use tempfile::TempDir;

#[test]
fn msvc_provider_id() {
    assert_eq!(MsvcProvider::new().id(), "msvc");
}

#[test]
fn msvc_provider_required_fields() {
    let tmp = TempDir::new().unwrap();
    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let mut ctx = InstallCtx::new(cache);
    let opts = toml::Table::new();
    let err = MsvcProvider::new().install(&opts, &mut ctx).unwrap_err();
    assert!(err.to_string().contains("vs_channel"));
}

#[test]
fn msvc_provider_pinned_version_fast_path_no_network() {
    // Pre-plant an install dir matching the deterministic fingerprint.
    // The provider should short-circuit without ever calling fetch.
    let tmp = TempDir::new().unwrap();
    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let fp = sanitize_fingerprint("msvc-14.50.18.0-18");
    let install_dir = cache.install_dir(&fp);
    std::fs::create_dir_all(install_dir.join("tree")).unwrap();
    let md = crate::cache::InstallMetadata::new(
        "msvc",
        fp.clone(),
        "msvc 14.50.18.0 (vs18)",
        toml::Table::new(),
    );
    md.write(&cache.install_metadata_path(&fp)).unwrap();

    let mut ctx = InstallCtx::new(cache);
    let mut opts = toml::Table::new();
    opts.insert("vs_channel".into(), toml::Value::String("18".into()));
    opts.insert(
        "msvc_version".into(),
        toml::Value::String("14.50.18.0".into()),
    );

    // Use a deliberately invalid template — if the provider tries to
    // hit it, we'll see the failure.
    let provider =
        MsvcProvider::with_channel_url_template("file:///never/exists/{channel}");
    let installed = provider.install(&opts, &mut ctx).unwrap();
    assert!(!installed.freshly_extracted);
    assert_eq!(installed.fingerprint, fp);
}

#[test]
fn msvc_manifest_history_requires_pinned_version() {
    let tmp = TempDir::new().unwrap();
    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let mut ctx = InstallCtx::new(cache);

    let mut opts = toml::Table::new();
    opts.insert("vs_channel".into(), toml::Value::String("17".into()));
    opts.insert("manifest_history".into(), toml::Value::Boolean(true));

    let err = MsvcProvider::new().install(&opts, &mut ctx).unwrap_err();
    assert!(
        err.to_string().contains("manifest_history"),
        "unexpected error: {err}"
    );
    assert!(err.to_string().contains("msvc_version"));
}

/// Build a fixture VS manifest containing every package the msvc provider
/// resolves: the MSVC base + the four companion packages
/// (Res.base, CRT.Headers.base, CRT.x64.Desktop.base, CRT.x64.Store.base).
/// Each gets a single VSIX payload pointing at a local file.
fn write_full_msvc_manifest(path: &Path, version: &str, vsix_url: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let payload = format!(
        r#"{{"url":"{vsix_url}","fileName":"vc.vsix"}}"#
    );
    let pkg = |id: &str| {
        format!(
            r#"{{"id":"{id}","payloads":[{payload}]}}"#
        )
    };
    let json = format!(
        r#"{{"packages":[
            {},
            {},
            {},
            {},
            {}
        ]}}"#,
        pkg(&format!(
            "Microsoft.VC.{version}.Tools.HostX64.TargetX64.base"
        )),
        pkg(&format!(
            "Microsoft.VC.{version}.Tools.HostX64.TargetX64.Res.base"
        )),
        pkg(&format!("Microsoft.VC.{version}.CRT.Headers.base")),
        pkg(&format!(
            "Microsoft.VC.{version}.CRT.x64.Desktop.base"
        )),
        pkg(&format!(
            "Microsoft.VC.{version}.CRT.x64.Store.base"
        )),
    );
    std::fs::write(path, json).unwrap();
}

fn write_commits_list(path: &Path, shas: &[&str]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let entries: Vec<String> = shas
        .iter()
        .map(|s| format!(r#"{{"sha":"{s}"}}"#))
        .collect();
    std::fs::write(path, format!("[{}]", entries.join(","))).unwrap();
}

/// Minimal VSIX = zip with at least one entry under `Contents/`.
fn build_vsix(path: &Path) {
    use std::fs::File;
    use std::io::Write as _;
    let f = File::create(path).unwrap();
    let mut zw = zip::ZipWriter::new(f);
    let opts = zip::write::FileOptions::default();
    zw.start_file("Contents/marker.txt", opts).unwrap();
    zw.write_all(b"hello").unwrap();
    zw.finish().unwrap();
}

#[test]
fn msvc_manifest_history_walks_and_installs() {
    // End-to-end: provider with manifest_history = true walks past a commit
    // whose manifest carries a different MSVC version, lands on the commit
    // that has the requested version, then proceeds with the normal payload
    // download + extraction.
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let vsix = root.join("vc.vsix");
    build_vsix(&vsix);
    let vsix_url = url::Url::from_file_path(&vsix).unwrap().to_string();

    write_commits_list(
        &root.join("commits/release-17/page-1"),
        &["newer", "wanted"],
    );
    write_commits_list(&root.join("commits/release-17/page-2"), &[]);
    write_full_msvc_manifest(
        &root.join("raw/newer/manifest.json"),
        "14.50.18.0",
        &vsix_url,
    );
    write_full_msvc_manifest(
        &root.join("raw/wanted/manifest.json"),
        "14.39.33519.0",
        &vsix_url,
    );

    let base = url::Url::from_directory_path(root).unwrap().to_string();
    let base = base.trim_end_matches('/').to_string();
    let commits_template =
        format!("{base}/commits/release-{{channel}}/page-{{page}}");
    let raw_template = format!("{base}/raw/{{sha}}/manifest.json");

    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let mut ctx = InstallCtx::new(cache);

    let mut opts = toml::Table::new();
    opts.insert("vs_channel".into(), toml::Value::String("17".into()));
    opts.insert(
        "msvc_version".into(),
        toml::Value::String("14.39.33519.0".into()),
    );
    opts.insert("manifest_history".into(), toml::Value::Boolean(true));

    let provider = MsvcProvider::with_history_urls(&commits_template, &raw_template);
    let installed = provider.install(&opts, &mut ctx).unwrap();
    assert!(installed.freshly_extracted);
    assert_eq!(
        installed.fingerprint,
        sanitize_fingerprint("msvc-14.39.33519.0-17")
    );
    assert_eq!(installed.display, "msvc 14.39.33519.0 (vs17)");
    // VSIX content landed under staging.
    let raw = ctx.staging_dir().unwrap();
    assert_eq!(std::fs::read(raw.join("marker.txt")).unwrap(), b"hello");
}

#[test]
fn msvc_manifest_history_unknown_version_errors() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let vsix = root.join("vc.vsix");
    build_vsix(&vsix);
    let vsix_url = url::Url::from_file_path(&vsix).unwrap().to_string();

    write_commits_list(&root.join("commits/release-17/page-1"), &["only"]);
    write_commits_list(&root.join("commits/release-17/page-2"), &[]);
    write_full_msvc_manifest(
        &root.join("raw/only/manifest.json"),
        "14.50.18.0",
        &vsix_url,
    );

    let base = url::Url::from_directory_path(root).unwrap().to_string();
    let base = base.trim_end_matches('/').to_string();
    let commits_template =
        format!("{base}/commits/release-{{channel}}/page-{{page}}");
    let raw_template = format!("{base}/raw/{{sha}}/manifest.json");

    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let mut ctx = InstallCtx::new(cache);

    let mut opts = toml::Table::new();
    opts.insert("vs_channel".into(), toml::Value::String("17".into()));
    opts.insert(
        "msvc_version".into(),
        toml::Value::String("14.39.33519.0".into()),
    );
    opts.insert("manifest_history".into(), toml::Value::Boolean(true));

    let provider = MsvcProvider::with_history_urls(&commits_template, &raw_template);
    let err = provider.install(&opts, &mut ctx).unwrap_err();
    assert!(
        err.to_string().contains("no matching manifest"),
        "unexpected: {err}"
    );
}
