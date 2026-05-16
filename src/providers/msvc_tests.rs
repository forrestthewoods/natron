//! Tests for `src/providers\msvc.rs` (split out so the production
//! file shows only the implementation).

use super::*;
use crate::cache::Cache;
use crate::providers::InstallCtx;
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
fn pinned_version_resolves_from_live_manifest_first() {
    let tmp = TempDir::new().unwrap();
    let live_manifest = tmp.path().join("live.vsman");
    write_msvc_manifest(&live_manifest, &["14.50.18.0", "14.39.33519.0"]);
    let live_channel = tmp.path().join("channel.json");
    write_channel_manifest(&live_channel, &live_manifest);

    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let ctx = InstallCtx::new(cache);
    let provider = MsvcProvider::with_channel_url_template(file_url(&live_channel))
        .with_archive_manifest_url_template("file:///archive/should/not/be/read.json");

    let (_, resolved, package_id) = provider
        .resolve_manifest_and_candidate("17", Some("14.39.33519.0"), &ctx)
        .unwrap();

    assert_eq!(resolved, "14.39.33519.0");
    assert!(package_id.contains("14.39.33519.0"));
}

#[test]
fn pinned_version_falls_back_to_archive_manifest() {
    let tmp = TempDir::new().unwrap();
    let live_manifest = tmp.path().join("live.vsman");
    write_msvc_manifest(&live_manifest, &["14.50.18.0"]);
    let live_channel = tmp.path().join("channel.json");
    write_channel_manifest(&live_channel, &live_manifest);
    let archive_manifest = tmp.path().join("archive.vsman");
    write_msvc_manifest(&archive_manifest, &["14.39.33519.0"]);

    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let ctx = InstallCtx::new(cache);
    let provider = MsvcProvider::with_channel_url_template(file_url(&live_channel))
        .with_archive_manifest_url_template(file_url(&archive_manifest));

    let (_, resolved, package_id) = provider
        .resolve_manifest_and_candidate("17", Some("14.39.33519.0"), &ctx)
        .unwrap();

    assert_eq!(resolved, "14.39.33519.0");
    assert!(package_id.contains("14.39.33519.0"));
}

#[test]
fn pinned_version_never_falls_back_to_latest() {
    let tmp = TempDir::new().unwrap();
    let live_manifest = tmp.path().join("live.vsman");
    write_msvc_manifest(&live_manifest, &["14.50.18.0"]);
    let live_channel = tmp.path().join("channel.json");
    write_channel_manifest(&live_channel, &live_manifest);

    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let ctx = InstallCtx::new(cache);
    let provider = MsvcProvider::with_channel_url_template(file_url(&live_channel))
        .with_archive_manifest_url_template("file:///archive/missing.json");

    let err = provider
        .resolve_manifest_and_candidate("17", Some("14.39.33519.0"), &ctx)
        .unwrap_err();
    let msg = format!("{err:#}");

    assert!(msg.contains("14.39.33519.0"), "got: {msg}");
    assert!(msg.contains("14.50.18.0"), "got: {msg}");
    assert!(msg.contains("archived manifest failed"), "got: {msg}");
}

#[test]
fn pinned_version_reports_both_searched_manifests() {
    let tmp = TempDir::new().unwrap();
    let live_manifest = tmp.path().join("live.vsman");
    write_msvc_manifest(&live_manifest, &["14.50.18.0"]);
    let live_channel = tmp.path().join("channel.json");
    write_channel_manifest(&live_channel, &live_manifest);
    let archive_manifest = tmp.path().join("archive.vsman");
    write_msvc_manifest(&archive_manifest, &["14.38.33130.0"]);

    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let ctx = InstallCtx::new(cache);
    let provider = MsvcProvider::with_channel_url_template(file_url(&live_channel))
        .with_archive_manifest_url_template(file_url(&archive_manifest));

    let err = provider
        .resolve_manifest_and_candidate("17", Some("14.39.33519.0"), &ctx)
        .unwrap_err();
    let msg = format!("{err:#}");

    assert!(msg.contains("Microsoft live manifest"), "got: {msg}");
    assert!(msg.contains("archived manifest"), "got: {msg}");
    assert!(msg.contains("14.50.18.0"), "got: {msg}");
    assert!(msg.contains("14.38.33130.0"), "got: {msg}");
}

fn write_channel_manifest(path: &std::path::Path, vs_manifest_path: &std::path::Path) {
    let vs_url = file_url(vs_manifest_path);
    let json = format!(
        r#"{{
            "channelItems": [{{
                "type": "Manifest",
                "id": "Microsoft.VisualStudio.Manifests.VisualStudio",
                "payloads": [{{ "url": "{vs_url}" }}]
            }}]
        }}"#
    );
    std::fs::write(path, json).unwrap();
}

fn write_msvc_manifest(path: &std::path::Path, versions: &[&str]) {
    let packages = versions
        .iter()
        .map(|version| {
            format!(
                r#"{{
                    "id": "Microsoft.VC.{version}.Tools.HostX64.TargetX64.base",
                    "payloads": []
                }}"#
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    std::fs::write(path, format!(r#"{{ "packages": [{packages}] }}"#)).unwrap();
}

fn file_url(path: &std::path::Path) -> String {
    url::Url::from_file_path(path).unwrap().to_string()
}
