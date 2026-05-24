//! Tests for `src/providers/vs_manifest.rs`. Also home to the shared
//! `MirrorFixture` helper that the msvc / windows_sdk / cli tests reuse.

use super::*;
use crate::cache::Cache;
use crate::providers::InstallCtx;
use std::io::Write;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

// ---- fixture builder -------------------------------------------------------

/// One synthetic Microsoft snapshot.
pub(crate) struct FxSnapshot {
    pub sha: &'static str,
    pub date: &'static str,
    pub build_version: &'static str,
    pub display_version: &'static str,
    pub product_line_version: &'static str,
    pub manifest_packages_json: String,
}

/// On-disk mirror layout that mimics the GitHub commits API + per-commit
/// `channel.json` / `manifest.json`. Returns URL bases the providers/CLI
/// can be pointed at.
pub(crate) struct MirrorFixture {
    pub urls: MirrorUrls,
    #[allow(dead_code)]
    pub root: PathBuf,
}

impl MirrorFixture {
    pub fn build(tmp: &Path, per_branch: &[(VsVersion, &[FxSnapshot])]) -> Self {
        let root = tmp.join("mirror");
        std::fs::create_dir_all(&root).unwrap();

        let raw_base = file_url(&root);
        let commits_base = format!("{}/commits-{{branch}}.json", file_url(&root));

        for (vs, snapshots) in per_branch {
            let commits_json: String = snapshots
                .iter()
                .map(|s| {
                    format!(
                        r#"{{"sha":"{sha}","commit":{{"author":{{"date":"{date}"}}}}}}"#,
                        sha = s.sha,
                        date = s.date,
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            std::fs::write(
                root.join(format!("commits-release-{}.json", vs.channel())),
                format!("[{commits_json}]"),
            )
            .unwrap();

            for s in *snapshots {
                let dir = root.join(s.sha);
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(
                    dir.join("channel.json"),
                    channel_json(
                        s.build_version,
                        s.display_version,
                        s.product_line_version,
                    ),
                )
                .unwrap();
                std::fs::write(
                    dir.join("manifest.json"),
                    format!(r#"{{"packages":[{}]}}"#, s.manifest_packages_json),
                )
                .unwrap();
            }
        }

        MirrorFixture {
            urls: MirrorUrls {
                raw_base,
                commits_base,
            },
            root,
        }
    }
}

pub(crate) fn channel_json(
    build_version: &str,
    display_version: &str,
    product_line_version: &str,
) -> String {
    format!(
        r#"{{"info":{{"buildVersion":"{build_version}","productDisplayVersion":"{display_version}","productLineVersion":"{product_line_version}"}}}}"#
    )
}

pub(crate) fn file_url(p: &Path) -> String {
    url::Url::from_file_path(p).unwrap().to_string()
}

pub(crate) fn test_ctx(tmp: &TempDir) -> InstallCtx {
    let cache = Cache::at(tmp.path().join("cache"));
    cache.ensure_layout().unwrap();
    InstallCtx::new(cache)
}

/// Build a minimal manifest package entry for fixtures.
pub(crate) fn pkg(id: &str, version: &str) -> String {
    format!(r#"{{"id":"{id}","version":"{version}","payloads":[]}}"#)
}

pub(crate) fn pkg_with_lang(id: &str, version: &str, language: &str) -> String {
    format!(
        r#"{{"id":"{id}","version":"{version}","language":"{language}","payloads":[]}}"#
    )
}

pub(crate) fn pkg_with_payload(
    id: &str,
    version: &str,
    payload_url: &str,
    payload_filename: &str,
) -> String {
    format!(
        r#"{{"id":"{id}","version":"{version}","payloads":[{{"url":"{payload_url}","fileName":"{payload_filename}"}}]}}"#
    )
}

pub(crate) fn build_vsix(path: &Path, entries: &[(&str, &[u8])]) {
    let file = std::fs::File::create(path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let opts = zip::write::FileOptions::default();
    for (name, bytes) in entries {
        zip.start_file(format!("Contents/{name}"), opts).unwrap();
        zip.write_all(bytes).unwrap();
    }
    zip.finish().unwrap();
}

// ---- tests -----------------------------------------------------------------

#[test]
fn vs_version_parse_roundtrip() {
    for v in VsVersion::all() {
        assert_eq!(VsVersion::parse(v.as_str()).unwrap(), v);
    }
    assert!(VsVersion::parse("release-18").is_err());
}

#[test]
fn vs_version_from_channel_maps_majors() {
    assert_eq!(VsVersion::from_channel(16).unwrap(), VsVersion::Vs2019);
    assert_eq!(VsVersion::from_channel(17).unwrap(), VsVersion::Vs2022);
    assert_eq!(VsVersion::from_channel(18).unwrap(), VsVersion::Vs2026);
    let err = VsVersion::from_channel(15).unwrap_err();
    assert!(err.to_string().contains("15"), "got: {err}");
}

#[test]
fn build_version_major_parses() {
    assert_eq!(build_version_major("18.6.11819.183").unwrap(), 18);
    assert_eq!(build_version_major("17.14.36322.0").unwrap(), 17);
    assert!(build_version_major("garbage").is_err());
    assert!(build_version_major("").is_err());
}

#[test]
fn build_index_sorts_commits_descending_by_date() {
    let tmp = TempDir::new().unwrap();
    let fx = MirrorFixture::build(
        tmp.path(),
        &[(
            VsVersion::Vs2026,
            &[
                FxSnapshot {
                    sha: "sha_old",
                    date: "2026-01-01T00:00:00Z",
                    build_version: "18.0.0.0",
                    display_version: "18.0",
                    product_line_version: "18",
                    manifest_packages_json: String::new(),
                },
                FxSnapshot {
                    sha: "sha_new",
                    date: "2026-05-01T00:00:00Z",
                    build_version: "18.6.0.0",
                    display_version: "18.6",
                    product_line_version: "18",
                    manifest_packages_json: String::new(),
                },
            ],
        )],
    );
    let ctx = test_ctx(&tmp);
    let entries = build_index(&fx.urls, &[VsVersion::Vs2026], &ctx).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].info.build_version, "18.6.0.0");
    assert_eq!(entries[1].info.build_version, "18.0.0.0");
}

#[test]
fn resolve_build_version_finds_exact_match() {
    let tmp = TempDir::new().unwrap();
    let fx = MirrorFixture::build(
        tmp.path(),
        &[(
            VsVersion::Vs2026,
            &[FxSnapshot {
                sha: "sha_a",
                date: "2026-05-01T00:00:00Z",
                build_version: "18.6.11819.183",
                display_version: "18.6.1",
                product_line_version: "18",
                manifest_packages_json: String::new(),
            }],
        )],
    );
    let ctx = test_ctx(&tmp);
    let entry = resolve_build_version(&fx.urls, "18.6.11819.183", &ctx).unwrap();
    assert_eq!(entry.commit.sha, "sha_a");
    assert_eq!(entry.vs, VsVersion::Vs2026);
}

#[test]
fn resolve_build_version_lists_alternatives_on_miss() {
    let tmp = TempDir::new().unwrap();
    let fx = MirrorFixture::build(
        tmp.path(),
        &[(
            VsVersion::Vs2026,
            &[FxSnapshot {
                sha: "sha_a",
                date: "2026-05-01T00:00:00Z",
                build_version: "18.6.11819.183",
                display_version: "18.6.1",
                product_line_version: "18",
                manifest_packages_json: String::new(),
            }],
        )],
    );
    let ctx = test_ctx(&tmp);
    let err = resolve_build_version(&fx.urls, "18.99.99.99", &ctx).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("18.99.99.99"), "got: {msg}");
    assert!(msg.contains("18.6.11819.183"), "got: {msg}");
}

#[test]
fn resolve_build_version_rejects_unknown_major() {
    let tmp = TempDir::new().unwrap();
    let fx = MirrorFixture::build(tmp.path(), &[]);
    let ctx = test_ctx(&tmp);
    let err = resolve_build_version(&fx.urls, "15.0.0.0", &ctx).unwrap_err();
    assert!(err.to_string().contains("15"), "got: {err}");
}

#[test]
fn default_commits_base_includes_path_filter() {
    // Regression guard: `?path=channel.json` filters out the mirror's
    // initial CI-setup commits which would otherwise show as 404 warnings
    // for users running `msvc versions`. Removing the filter is a regression.
    let url = default_commits_base();
    assert!(
        url.contains("path=channel.json"),
        "default_commits_base() lost the path filter: {url}"
    );
}

#[test]
fn parse_json_evicts_cache_on_corruption() {
    let tmp = TempDir::new().unwrap();
    let bad = tmp.path().join("bad.json");
    std::fs::write(&bad, "not json").unwrap();
    let _: Result<serde_json::Value> = parse_json_or_evict(&bad, "test");
    assert!(!bad.exists(), "corrupt file should have been evicted");
}
