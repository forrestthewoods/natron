//! Tests for `src/providers\vs_manifest.rs` (split out so the production
//! file shows only the implementation).

use super::*;
use crate::cache::Cache;
use std::path::Path;
use tempfile::TempDir;

fn sample_manifest() -> VsManifest {
    // Tiny canned subset of a real VS manifest. Covers MSVC + SDK
    // package selection logic.
    let json = r#"{
        "packages": [
            {
                "id": "Microsoft.VC.14.50.18.0.Tools.HostX64.TargetX64.base",
                "payloads": [
                    {"url": "https://example.com/vc-14.50.18.0.vsix",
                     "sha256": "aaaa",
                     "fileName": "vc.vsix"}
                ]
            },
            {
                "id": "Microsoft.VC.14.49.99.0.Tools.HostX64.TargetX64.base",
                "payloads": []
            },
            {
                "id": "Microsoft.VC.14.50.18.0.Tools.HostX64.TargetX64.Premium.base",
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
    let versions: Vec<_> = cands.iter().map(|(v, _)| v.as_str()).collect();
    assert_eq!(versions, vec!["14.50.18.0", "14.49.99.0"]);
    // Premium variant is excluded.
    for (_, id) in &cands {
        assert!(!id.to_lowercase().contains(".premium."));
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

/// Manifest fixture containing a single MSVC base package at `version`.
fn write_history_manifest(path: &Path, version: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let json = format!(
        r#"{{"packages":[{{"id":"Microsoft.VC.{version}.Tools.HostX64.TargetX64.base","payloads":[]}}]}}"#
    );
    std::fs::write(path, json).unwrap();
}

/// Commits-list JSON in the shape returned by GitHub's `/commits` endpoint
/// (we read only the `sha` field).
fn write_commits_list(path: &Path, shas: &[&str]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let entries: Vec<String> = shas
        .iter()
        .map(|s| format!(r#"{{"sha":"{s}"}}"#))
        .collect();
    std::fs::write(path, format!("[{}]", entries.join(","))).unwrap();
}

#[test]
fn find_in_history_walks_until_match() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    // Two-page commit listing. Newest page (page-1) lists shas that point at
    // manifests for newer MSVC versions; the version we want shows up only on
    // page-2 — the walker must keep going.
    write_commits_list(&root.join("commits/release-17/page-1"), &["aaa", "bbb"]);
    write_commits_list(&root.join("commits/release-17/page-2"), &["ccc", "ddd"]);
    // page-3 is empty → loop terminates if we reach it.
    write_commits_list(&root.join("commits/release-17/page-3"), &[]);
    write_history_manifest(&root.join("raw/aaa/manifest.json"), "14.50.18.0");
    write_history_manifest(&root.join("raw/bbb/manifest.json"), "14.49.99.0");
    write_history_manifest(&root.join("raw/ccc/manifest.json"), "14.42.34433.0");
    write_history_manifest(&root.join("raw/ddd/manifest.json"), "14.39.33519.0");

    let base = url::Url::from_directory_path(root).unwrap().to_string();
    let base = base.trim_end_matches('/').to_string();
    let commits_template =
        format!("{base}/commits/release-{{channel}}/page-{{page}}");
    let raw_template = format!("{base}/raw/{{sha}}/manifest.json");

    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let ctx = InstallCtx::new(cache);

    // Want the version that lives at the third commit (ccc, page-2 first
    // entry). Scanning order is newest-first, so we'll touch aaa, bbb, then
    // ccc — and stop.
    let want = "14.42.34433.0";
    let manifest = find_vs_manifest_in_history(
        &commits_template,
        &raw_template,
        "17",
        5,
        &ctx,
        |m| {
            m.find_msvc_candidates("X64", "X64")
                .iter()
                .any(|(v, _)| v == want)
        },
    )
    .unwrap();
    assert_eq!(manifest.find_msvc_candidates("X64", "X64")[0].0, want);
}

#[test]
fn find_in_history_errors_when_version_absent() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write_commits_list(&root.join("commits/release-17/page-1"), &["aaa"]);
    write_commits_list(&root.join("commits/release-17/page-2"), &[]);
    write_history_manifest(&root.join("raw/aaa/manifest.json"), "14.50.18.0");

    let base = url::Url::from_directory_path(root).unwrap().to_string();
    let base = base.trim_end_matches('/').to_string();
    let commits_template =
        format!("{base}/commits/release-{{channel}}/page-{{page}}");
    let raw_template = format!("{base}/raw/{{sha}}/manifest.json");

    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let ctx = InstallCtx::new(cache);

    let err = find_vs_manifest_in_history(
        &commits_template,
        &raw_template,
        "17",
        2,
        &ctx,
        |_| false,
    )
    .unwrap_err();
    assert!(err.to_string().contains("no matching manifest"));
}

#[test]
fn find_in_history_skips_unparseable_commits() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write_commits_list(
        &root.join("commits/release-17/page-1"),
        &["bad", "good"],
    );
    write_commits_list(&root.join("commits/release-17/page-2"), &[]);
    // "bad" has malformed JSON — provider should warn + continue, not abort.
    std::fs::create_dir_all(root.join("raw/bad")).unwrap();
    std::fs::write(root.join("raw/bad/manifest.json"), b"{not json").unwrap();
    write_history_manifest(&root.join("raw/good/manifest.json"), "14.42.34433.0");

    let base = url::Url::from_directory_path(root).unwrap().to_string();
    let base = base.trim_end_matches('/').to_string();
    let commits_template =
        format!("{base}/commits/release-{{channel}}/page-{{page}}");
    let raw_template = format!("{base}/raw/{{sha}}/manifest.json");

    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let ctx = InstallCtx::new(cache);

    let manifest = find_vs_manifest_in_history(
        &commits_template,
        &raw_template,
        "17",
        2,
        &ctx,
        |m| !m.find_msvc_candidates("X64", "X64").is_empty(),
    )
    .unwrap();
    assert_eq!(
        manifest.find_msvc_candidates("X64", "X64")[0].0,
        "14.42.34433.0"
    );
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
