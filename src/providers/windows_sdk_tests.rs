//! Tests for `src/providers/windows_sdk.rs`.

use super::*;
use crate::providers::vs_manifest::tests::{
    pkg, test_ctx, FxSnapshot, MirrorFixture,
};
use crate::providers::vs_manifest::VsVersion;
use tempfile::TempDir;

// ---- Options::parse --------------------------------------------------------

#[test]
fn options_require_sdk_version() {
    let opts = toml::Table::new();
    let err = Options::parse(&opts).unwrap_err();
    assert!(err.to_string().contains("sdk_version"), "got: {err}");
}

#[test]
fn options_reject_non_numeric_sdk_version() {
    let mut opts = toml::Table::new();
    opts.insert("sdk_version".into(), toml::Value::String("latest".into()));
    let err = Options::parse(&opts).unwrap_err();
    assert!(err.to_string().contains("numeric"), "got: {err}");
}

#[test]
fn options_default_base_is_default() {
    let mut opts = toml::Table::new();
    opts.insert("sdk_version".into(), toml::Value::String("26100".into()));
    let parsed = Options::parse(&opts).unwrap();
    assert_eq!(parsed.base, BaseInstall::Default);
}

#[test]
fn options_none_with_empty_extras_rejected() {
    let mut opts = toml::Table::new();
    opts.insert("sdk_version".into(), toml::Value::String("26100".into()));
    opts.insert(
        "base_install".into(),
        toml::Value::String("none".into()),
    );
    let err = Options::parse(&opts).unwrap_err();
    assert!(err.to_string().contains("nothing"), "got: {err}");
}

#[test]
fn options_full_with_extras_rejected() {
    let mut opts = toml::Table::new();
    opts.insert("sdk_version".into(), toml::Value::String("26100".into()));
    opts.insert(
        "base_install".into(),
        toml::Value::String("full".into()),
    );
    opts.insert(
        "extras".into(),
        toml::Value::Array(vec![toml::Value::String("Windows SDK Signing".into())]),
    );
    let err = Options::parse(&opts).unwrap_err();
    assert!(err.to_string().contains("full"), "got: {err}");
}

// ---- msi_should_extract ----------------------------------------------------

fn opts_with(base: &str, extras: &[&str]) -> Options {
    let mut t = toml::Table::new();
    t.insert("sdk_version".into(), toml::Value::String("26100".into()));
    t.insert("base_install".into(), toml::Value::String(base.into()));
    if !extras.is_empty() {
        t.insert(
            "extras".into(),
            toml::Value::Array(
                extras
                    .iter()
                    .map(|s| toml::Value::String((*s).into()))
                    .collect(),
            ),
        );
    }
    Options::parse(&t).unwrap()
}

#[test]
fn full_extracts_every_msi() {
    let o = opts_with("full", &[]);
    assert!(msi_should_extract("Universal CRT Headers Libraries and Sources-x86_en-us.msi", &o));
    assert!(msi_should_extract("Windows SDK Signing Tools-x86_en-us.msi", &o));
    assert!(msi_should_extract("Random Microsoft Telemetry-x86_en-us.msi", &o));
}

#[test]
fn default_extracts_essentials_only() {
    let o = opts_with("default", &[]);
    assert!(msi_should_extract("Universal CRT Headers Libraries and Sources-x86_en-us.msi", &o));
    assert!(msi_should_extract("Windows SDK Desktop Headers x86-x86_en-us.msi", &o));
    assert!(!msi_should_extract("Windows SDK Signing Tools-x86_en-us.msi", &o));
    assert!(!msi_should_extract("Random Telemetry-x86_en-us.msi", &o));
}

#[test]
fn default_plus_extras_extracts_both() {
    let o = opts_with("default", &["Windows SDK Signing Tools"]);
    assert!(msi_should_extract("Universal CRT Headers Libraries and Sources-x86_en-us.msi", &o));
    assert!(msi_should_extract("Windows SDK Signing Tools-x86_en-us.msi", &o));
    assert!(!msi_should_extract("Random Telemetry-x86_en-us.msi", &o));
}

#[test]
fn none_plus_extras_extracts_only_extras() {
    let o = opts_with("none", &["Windows SDK Signing Tools"]);
    assert!(!msi_should_extract("Universal CRT Headers Libraries and Sources-x86_en-us.msi", &o));
    assert!(msi_should_extract("Windows SDK Signing Tools-x86_en-us.msi", &o));
}

#[test]
fn nested_msi_paths_are_flattened_before_matching() {
    // strip_installer_prefix should yield the same basename regardless of
    // path nesting in the manifest's fileName.
    let o = opts_with("default", &[]);
    assert!(msi_should_extract(
        "Installers\\Universal CRT Headers Libraries and Sources-x86_en-us.msi",
        &o
    ));
    assert!(msi_should_extract(
        "Redistributable/10.1.0.0/Windows SDK Desktop Libs x64-x86_en-us.msi",
        &o
    ));
}

// ---- find_sdk_candidates ---------------------------------------------------

#[test]
fn find_sdk_candidates_sorts_desc_and_keeps_both_prefixes() {
    let m: VsManifest = serde_json::from_str(
        r#"{"packages":[
            {"id":"Microsoft.VisualStudio.Component.Windows11SDK.22000","version":"x","payloads":[]},
            {"id":"Microsoft.VisualStudio.Component.Windows10SDK.19041","version":"x","payloads":[]},
            {"id":"Microsoft.VisualStudio.Component.Windows11SDK.26100","version":"x","payloads":[]}
        ]}"#,
    )
    .unwrap();
    let cands = find_sdk_candidates(&m);
    let versions: Vec<_> = cands.iter().map(|(v, _)| v.as_str()).collect();
    assert_eq!(versions, vec!["26100", "22000", "19041"]);
}

#[test]
fn find_sdk_candidates_excludes_non_numeric_suffixes() {
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

// ---- discover_sdk_versions / resolve_sdk_version ---------------------------

fn snapshot_with_sdks(
    sha: &'static str,
    date: &'static str,
    build_version: &'static str,
    sdks: &[&str],
) -> FxSnapshot {
    let packages = sdks
        .iter()
        .map(|v| {
            pkg(
                &format!("Microsoft.VisualStudio.Component.Windows11SDK.{v}"),
                "1.0.0.0",
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let parts: Vec<&str> = build_version.splitn(2, '.').collect();
    let line = parts.first().copied().unwrap_or("18");
    FxSnapshot {
        sha,
        date,
        build_version,
        display_version: build_version,
        product_line_version: line,
        manifest_packages_json: packages,
    }
}

#[test]
fn discover_sdk_versions_dedupes_across_series_sorted_desc() {
    let tmp = TempDir::new().unwrap();
    let fx = MirrorFixture::build(
        tmp.path(),
        &[
            (
                VsVersion::Vs2026,
                &[snapshot_with_sdks(
                    "s18",
                    "2026-05-01T00:00:00Z",
                    "18.6.11819.183",
                    &["26100", "22621"],
                )],
            ),
            (
                VsVersion::Vs2022,
                &[snapshot_with_sdks(
                    "s17",
                    "2026-05-01T00:00:00Z",
                    "17.14.0.0",
                    &["22621", "22000", "19041"],
                )],
            ),
            (
                VsVersion::Vs2019,
                &[snapshot_with_sdks(
                    "s16",
                    "2026-05-01T00:00:00Z",
                    "16.11.0.0",
                    &["19041", "17763"],
                )],
            ),
        ],
    );
    let ctx = test_ctx(&tmp);
    let versions = discover_sdk_versions(&fx.urls, &ctx).unwrap();
    assert_eq!(versions, vec!["26100", "22621", "22000", "19041", "17763"]);
}

#[test]
fn resolve_finds_sdk_in_newest_snapshot_containing_it() {
    let tmp = TempDir::new().unwrap();
    let fx = MirrorFixture::build(
        tmp.path(),
        &[
            (
                VsVersion::Vs2026,
                &[snapshot_with_sdks(
                    "s18",
                    "2026-05-01T00:00:00Z",
                    "18.6.0.0",
                    &["26100"],
                )],
            ),
            (
                VsVersion::Vs2022,
                &[snapshot_with_sdks(
                    "s17",
                    "2026-05-01T00:00:00Z",
                    "17.14.0.0",
                    &["22000"],
                )],
            ),
        ],
    );
    let ctx = test_ctx(&tmp);
    // 22000 is only in vs2022; resolve should walk down to find it.
    let resolved = resolve_sdk_version(&fx.urls, "22000", &ctx).unwrap();
    assert_eq!(resolved.entry.commit.sha, "s17");
    assert!(resolved.sdk_pkg_id.ends_with(".22000"));
}

#[test]
fn resolve_errors_with_available_list_on_miss() {
    let tmp = TempDir::new().unwrap();
    let fx = MirrorFixture::build(
        tmp.path(),
        &[(
            VsVersion::Vs2026,
            &[snapshot_with_sdks(
                "s18",
                "2026-05-01T00:00:00Z",
                "18.6.0.0",
                &["26100", "22621"],
            )],
        )],
    );
    let ctx = test_ctx(&tmp);
    let err = resolve_sdk_version(&fx.urls, "99999", &ctx).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("99999"), "got: {msg}");
    assert!(msg.contains("26100"), "got: {msg}");
    assert!(msg.contains("22621"), "got: {msg}");
}

// ---- fingerprint -----------------------------------------------------------

#[test]
fn fingerprint_stable_across_reordered_extras() {
    let a = opts_with("default", &["Signing Tools", "Debugging Tools"]);
    let b = opts_with("default", &["Debugging Tools", "Signing Tools"]);
    assert_eq!(fingerprint(&a), fingerprint(&b));
}

#[test]
fn fingerprint_changes_with_base_install() {
    let d = opts_with("default", &[]);
    let f = opts_with("full", &[]);
    assert_ne!(fingerprint(&d), fingerprint(&f));
}

// ---- cache fast path -------------------------------------------------------

#[test]
fn install_cache_fast_path_skips_fetch() {
    let tmp = TempDir::new().unwrap();
    let cache = crate::cache::Cache::at(tmp.path().join("cache"));
    cache.ensure_layout().unwrap();
    let opts = opts_with("default", &[]);
    let fp = fingerprint(&opts);
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
    let mut topts = toml::Table::new();
    topts.insert("sdk_version".into(), toml::Value::String("26100".into()));
    let installed = provider.install(&topts, &mut ctx).unwrap();
    assert!(!installed.freshly_extracted);
    assert_eq!(installed.fingerprint, fp);
}

// ---- helpers ---------------------------------------------------------------

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
