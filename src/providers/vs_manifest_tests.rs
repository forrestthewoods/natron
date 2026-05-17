//! Tests for `src/providers\vs_manifest.rs` (split out so the production
//! file shows only the implementation).

use super::*;

fn sample_manifest() -> VsManifest {
    // Tiny canned subset of a real VS manifest. Covers MSVC + SDK
    // package selection logic.
    let json = r#"{
        "packages": [
            {
                "id": "Microsoft.VC.14.50.18.0.Tools.HostX64.TargetX64.base",
                "version": "14.50.35731",
                "payloads": [
                    {"url": "https://example.com/vc-14.50.18.0.vsix",
                     "sha256": "aaaa",
                     "fileName": "vc.vsix"}
                ]
            },
            {
                "id": "Microsoft.VC.14.49.99.0.Tools.HostX64.TargetX64.base",
                "version": "14.49.34567",
                "payloads": []
            },
            {
                "id": "Microsoft.VC.14.50.18.0.Tools.HostX64.TargetX64.Premium.base",
                "version": "14.50.35731",
                "payloads": []
            },
            {
                "id": "Microsoft.VisualStudio.Component.Windows11SDK.26100",
                "payloads": []
            },
            {
                "id": "Microsoft.VisualStudio.Component.Windows10SDK.19041",
                "payloads": []
            },
            {
                "id": "Microsoft.VC.14.50.18.0.CRT.Headers.base",
                "payloads": [
                    {"url": "https://example.com/headers.vsix",
                     "sha256": "bbbb",
                     "fileName": "headers.vsix"}
                ]
            }
        ]
    }"#;
    serde_json::from_str(json).unwrap()
}

#[test]
fn parses_packages() {
    let m = sample_manifest();
    assert_eq!(m.packages.len(), 6);
}

#[test]
fn find_msvc_candidates_picks_base_only_and_sorts_descending() {
    let m = sample_manifest();
    let cands = m.find_msvc_candidates("X64", "X64");
    let versions: Vec<_> = cands
        .iter()
        .map(|candidate| candidate.package_version.as_str())
        .collect();
    assert_eq!(versions, vec!["14.50.35731", "14.49.34567"]);
    assert_eq!(cands[0].package_id_version, "14.50.18.0");
    // Premium variant is excluded.
    for candidate in &cands {
        assert!(!candidate.package_id.to_lowercase().contains(".premium."));
    }
}

#[test]
fn find_msvc_candidates_respects_host_target_filter() {
    let m = sample_manifest();
    let none = m.find_msvc_candidates("arm64", "arm64");
    assert!(none.is_empty());
}

#[test]
fn find_sdk_candidates_includes_both_win10_and_win11() {
    let m = sample_manifest();
    let cands = m.find_sdk_candidates();
    let versions: Vec<_> = cands.iter().map(|(v, _)| v.as_str()).collect();
    // Sorted descending by numeric components.
    assert_eq!(versions, vec!["26100", "19041"]);
}

#[test]
fn find_package_is_case_insensitive() {
    let m = sample_manifest();
    let p = m.find_package("MICROSOFT.VC.14.50.18.0.CRT.HEADERS.BASE");
    assert!(p.is_some());
}

#[test]
fn find_package_prefers_en_us_among_language_variants() {
    let json = r#"{
        "packages": [
            {"id": "Foo.Bar", "language": "cs-CZ", "payloads": []},
            {"id": "Foo.Bar", "language": "en-US", "payloads": []},
            {"id": "Foo.Bar", "language": "ja-JP", "payloads": []}
        ]
    }"#;
    let m: VsManifest = serde_json::from_str(json).unwrap();
    let p = m.find_package("Foo.Bar").unwrap();
    assert_eq!(p.language.as_deref(), Some("en-US"));
}

#[test]
fn find_package_falls_back_to_languageless_then_first() {
    // No en-US: prefer the languageless one.
    let json = r#"{
        "packages": [
            {"id": "Foo.Bar", "language": "cs-CZ", "payloads": []},
            {"id": "Foo.Bar", "payloads": []}
        ]
    }"#;
    let m: VsManifest = serde_json::from_str(json).unwrap();
    let p = m.find_package("Foo.Bar").unwrap();
    assert!(p.language.is_none());

    // Only language variants, no en-US: take the first.
    let json2 = r#"{
        "packages": [
            {"id": "Foo.Bar", "language": "ja-JP", "payloads": []},
            {"id": "Foo.Bar", "language": "de-DE", "payloads": []}
        ]
    }"#;
    let m2: VsManifest = serde_json::from_str(json2).unwrap();
    let p2 = m2.find_package("Foo.Bar").unwrap();
    assert_eq!(p2.language.as_deref(), Some("ja-JP"));
}

#[test]
fn version_key_orders_numeric_components() {
    // Plain lexicographic would put "14.50.5" > "14.50.18" because
    // '5' > '1'. Verify our impl is numeric.
    assert!(version_key("14.50.18") > version_key("14.50.5"));
    assert!(version_key("14.50.18.0") > version_key("14.49.99.0"));
    assert!(version_key("14.50.18.0") > version_key("14.50.17.0"));
    // 14.10 > 14.9 numerically (lex says the opposite).
    assert!(version_key("14.10.0") > version_key("14.9.0"));
}
