//! Tests for `src/providers/windows_sdk.rs`.

use super::*;
use crate::providers::vs_manifest::tests::{test_ctx, FxSnapshot, MirrorFixture};
use crate::providers::vs_manifest::VsVersion;
use tempfile::TempDir;

// ---- Options::parse --------------------------------------------------------

#[test]
fn options_require_build_version() {
    let opts = toml::Table::new();
    let err = Options::parse(&opts).unwrap_err();
    assert!(err.to_string().contains("build_version"), "got: {err}");
}

#[test]
fn options_reject_non_string_build_version() {
    let mut opts = toml::Table::new();
    opts.insert(
        "build_version".into(),
        toml::Value::Integer(18),
    );
    let err = Options::parse(&opts).unwrap_err();
    assert!(err.to_string().contains("string"), "got: {err}");
}

#[test]
fn options_reject_unknown_major() {
    let mut opts = toml::Table::new();
    opts.insert(
        "build_version".into(),
        toml::Value::String("15.0.0.0".into()),
    );
    let err = Options::parse(&opts).unwrap_err();
    assert!(err.to_string().contains("15"), "got: {err}");
}

// ---- find_sdk_candidates ---------------------------------------------------

#[test]
fn sdk_candidates_finds_both_win10_and_win11_prefixes_sorted_desc() {
    let m: VsManifest = serde_json::from_str(
        r#"{"packages":[
            {"id":"Microsoft.VisualStudio.Component.Windows11SDK.26100","version":"x","payloads":[]},
            {"id":"Microsoft.VisualStudio.Component.Windows10SDK.19041","version":"x","payloads":[]},
            {"id":"Microsoft.VisualStudio.Component.Windows11SDK.22000","version":"x","payloads":[]}
        ]}"#,
    )
    .unwrap();
    let cands = find_sdk_candidates(&m);
    let versions: Vec<_> = cands.iter().map(|(v, _)| v.as_str()).collect();
    assert_eq!(versions, vec!["26100", "22000", "19041"]);
}

#[test]
fn sdk_candidates_excludes_non_numeric_suffixes() {
    let m: VsManifest = serde_json::from_str(
        r#"{"packages":[
            {"id":"Microsoft.VisualStudio.Component.Windows11SDK.26100","version":"x","payloads":[]},
            {"id":"Microsoft.VisualStudio.Component.Windows11SDK.Bogus","version":"x","payloads":[]}
        ]}"#,
    )
    .unwrap();
    let cands = find_sdk_candidates(&m);
    assert_eq!(cands.len(), 1);
    assert_eq!(cands[0].0, "26100");
}

// ---- install errors --------------------------------------------------------

#[test]
fn install_errors_when_pinned_sdk_not_in_snapshot() {
    let tmp = TempDir::new().unwrap();
    let manifest_packages = format!(
        r#"{a}"#,
        a = r#"{"id":"Microsoft.VisualStudio.Component.Windows11SDK.26100","version":"x","payloads":[]}"#,
    );
    let fx = MirrorFixture::build(
        tmp.path(),
        &[(
            VsVersion::Vs2026,
            &[FxSnapshot {
                sha: "abc",
                date: "2026-05-01T00:00:00Z",
                build_version: "18.6.11819.183",
                display_version: "18.6.1",
                product_line_version: "18",
                manifest_packages_json: manifest_packages,
            }],
        )],
    );

    let mut ctx = test_ctx(&tmp);
    let provider = WindowsSdkProvider::with_urls(fx.urls);
    let mut opts = toml::Table::new();
    opts.insert(
        "build_version".into(),
        toml::Value::String("18.6.11819.183".into()),
    );
    opts.insert(
        "sdk_version".into(),
        toml::Value::String("99999".into()),
    );
    let err = provider.install(&opts, &mut ctx).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("99999"), "got: {msg}");
    assert!(msg.contains("26100"), "got: {msg}");
}

#[test]
fn install_cache_fast_path_skips_fetch() {
    let tmp = TempDir::new().unwrap();
    let cache = crate::cache::Cache::at(tmp.path().join("cache"));
    cache.ensure_layout().unwrap();
    let fp = sdk_fingerprint("18.6.11819.183", "26100");
    let install_dir = cache.install_dir(&fp);
    std::fs::create_dir_all(install_dir.join("tree")).unwrap();
    let md = crate::cache::InstallMetadata::new(
        "windows_sdk",
        fp.clone(),
        "cached",
        toml::Table::new(),
    );
    md.write(&cache.install_metadata_path(&fp)).unwrap();

    let mut ctx = InstallCtx::new(cache);
    let provider = WindowsSdkProvider::with_urls(MirrorUrls {
        raw_base: "file:///never".into(),
        commits_base: "file:///never/{branch}.json".into(),
    });
    let mut opts = toml::Table::new();
    opts.insert(
        "build_version".into(),
        toml::Value::String("18.6.11819.183".into()),
    );
    opts.insert(
        "sdk_version".into(),
        toml::Value::String("26100".into()),
    );
    let installed = provider.install(&opts, &mut ctx).unwrap();
    assert!(!installed.freshly_extracted);
    assert_eq!(installed.fingerprint, fp);
}

// ---- helper unit tests -----------------------------------------------------

#[test]
fn essential_msi_match_works() {
    assert!(is_essential_msi("Universal CRT Headers Libraries and Sources-x86_en-us.msi"));
    assert!(is_essential_msi("Windows SDK Desktop Libs x64-x86_en-us.msi"));
    assert!(is_essential_msi("Installers\\Windows SDK OnecoreUap Headers-x86_en-us.msi"));
    assert!(is_essential_msi("Redistributable\\10.1.0.0\\Windows SDK Desktop Libs x64-x86.msi"));
    assert!(!is_essential_msi("Random Other.msi"));
}

#[test]
fn strip_installer_prefix_flattens_subdirs() {
    assert_eq!(strip_installer_prefix("simple.msi"), "simple.msi");
    assert_eq!(strip_installer_prefix("Installers\\foo.msi"), "foo.msi");
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
    std::fs::write(src.join("Some-Random.msi"), b"msi-bytes").unwrap();

    flatten_windows_kits_into(&src, &dst).unwrap();

    assert!(dst.join("Include").join("10.0.0").join("foo.h").is_file());
    assert!(
        !dst.join("Some-Random.msi").exists(),
        "stray .msi must not leak into install"
    );
}
