//! Tests for `src/providers\windows_sdk.rs` (split out so the production
//! file shows only the implementation).

use super::*;
use crate::cache::Cache;
use tempfile::TempDir;

#[test]
fn windows_sdk_provider_id() {
    assert_eq!(WindowsSdkProvider::new().id(), "windows_sdk");
}

#[test]
fn windows_sdk_provider_required_fields() {
    let tmp = TempDir::new().unwrap();
    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let mut ctx = InstallCtx::new(cache);
    let opts = toml::Table::new();
    let err = WindowsSdkProvider::new().install(&opts, &mut ctx).unwrap_err();
    assert!(err.to_string().contains("options.vs"));
}

#[test]
fn windows_sdk_provider_rejects_old_vs_channel() {
    let tmp = TempDir::new().unwrap();
    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let mut ctx = InstallCtx::new(cache);
    let mut opts = toml::Table::new();
    opts.insert("vs_channel".into(), toml::Value::String("18".into()));
    let err = WindowsSdkProvider::new().install(&opts, &mut ctx).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("vs_channel"), "got: {msg}");
    assert!(msg.contains("'vs'"), "got: {msg}");
}

#[test]
fn windows_sdk_provider_pinned_fast_path_no_network() {
    let tmp = TempDir::new().unwrap();
    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let fp = sanitize_fingerprint("windows_sdk-26100-vs2026");
    let install_dir = cache.install_dir(&fp);
    std::fs::create_dir_all(install_dir.join("tree")).unwrap();
    let md = crate::cache::InstallMetadata::new(
        "windows_sdk",
        fp.clone(),
        "windows_sdk 26100 (vs2026)",
        toml::Table::new(),
    );
    md.write(&cache.install_metadata_path(&fp)).unwrap();

    let mut ctx = InstallCtx::new(cache);
    let mut opts = toml::Table::new();
    opts.insert("vs".into(), toml::Value::String("vs2026".into()));
    opts.insert("sdk_version".into(), toml::Value::String("26100".into()));

    let provider = WindowsSdkProvider::with_channel_url_template(
        "file:///never/exists/{channel}",
    );
    let installed = provider.install(&opts, &mut ctx).unwrap();
    assert!(!installed.freshly_extracted);
}

#[test]
fn essential_msi_match_works() {
    assert!(is_essential_msi(
        "Universal CRT Headers Libraries and Sources-x86_en-us.msi"
    ));
    assert!(is_essential_msi("Windows SDK Desktop Libs x64-x86_en-us.msi"));
    assert!(is_essential_msi(
        "Installers\\Windows SDK OnecoreUap Headers-x86_en-us.msi"
    ));
    assert!(is_essential_msi(
        "Redistributable\\10.1.0.0\\Windows SDK Desktop Libs x64-x86.msi"
    ));
    assert!(!is_essential_msi("Random Other.msi"));
}

#[test]
fn strip_installer_prefix_flattens_subdirs() {
    assert_eq!(strip_installer_prefix("simple.msi"), "simple.msi");
    assert_eq!(
        strip_installer_prefix("Installers\\foo.msi"),
        "foo.msi"
    );
    assert_eq!(
        strip_installer_prefix("Redistributable\\10.1.0.0\\UAPSDKAddOn-x86.msi"),
        "UAPSDKAddOn-x86.msi"
    );
    assert_eq!(strip_installer_prefix("a/b/c.cab"), "c.cab");
}

#[test]
fn flatten_windows_kits_moves_children_to_dst() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("extract");
    let dst = tmp.path().join("install");
    let kits = src.join("Windows Kits").join("10");
    std::fs::create_dir_all(kits.join("Include").join("10.0.0")).unwrap();
    std::fs::write(
        kits.join("Include").join("10.0.0").join("foo.h"),
        b"#define FOO 1",
    )
    .unwrap();
    std::fs::create_dir_all(kits.join("Lib").join("10.0.0").join("um")).unwrap();
    std::fs::write(
        kits.join("Lib").join("10.0.0").join("um").join("foo.lib"),
        b"libdata",
    )
    .unwrap();
    // Stray files in src that should NOT be carried into dst.
    std::fs::write(src.join("Some-Random.msi"), b"msi-bytes").unwrap();

    flatten_windows_kits_into(&src, &dst).unwrap();

    assert!(dst.join("Include").join("10.0.0").join("foo.h").is_file());
    assert!(dst.join("Lib").join("10.0.0").join("um").join("foo.lib").is_file());
    assert!(!dst.join("Some-Random.msi").exists(), "stray .msi must not leak into install");
}
