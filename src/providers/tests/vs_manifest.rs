//! Tests for `src/providers/vs_manifest.rs`. Also home to the shared
//! `MirrorFixture` helper that the msvc / windows_sdk / cli tests reuse.

use super::*;
use crate::cache::Cache;
use crate::providers::InstallCtx;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

// ---- fixture builder -------------------------------------------------------

/// One synthetic Microsoft snapshot. `sha` is a human label used as the
/// commit message; git assigns the real commit hash.
pub(crate) struct FxSnapshot {
    pub sha: &'static str,
    pub date: &'static str,
    pub build_version: &'static str,
    pub display_version: &'static str,
    pub product_line_version: &'static str,
    pub manifest_packages_json: String,
}

/// A throwaway git repo mirroring the manifest-history layout: one
/// `release-{channel}` branch per VS series, one commit per snapshot carrying
/// that snapshot's `channel.json` + `manifest.json`. Returns a `remote` the
/// providers/CLI clone via `file://`.
pub(crate) struct MirrorFixture {
    pub remote: String,
    pub root: PathBuf,
}

impl MirrorFixture {
    pub fn build(tmp: &Path, per_branch: &[(VsVersion, &[FxSnapshot])]) -> Self {
        let root = tmp.join("mirror-src");
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "--quiet"]);
        for (vs, snapshots) in per_branch {
            let branch = format!("release-{}", vs.channel());
            git(&root, &["checkout", "--quiet", "--orphan", branch.as_str()]);
            for s in *snapshots {
                std::fs::write(
                    root.join("channel.json"),
                    channel_json(s.build_version, s.display_version, s.product_line_version),
                )
                .unwrap();
                std::fs::write(
                    root.join("manifest.json"),
                    format!(r#"{{"packages":[{}]}}"#, s.manifest_packages_json),
                )
                .unwrap();
                git(&root, &["add", "channel.json", "manifest.json"]);
                git_commit(&root, s.sha, s.date);
            }
        }
        MirrorFixture {
            remote: file_url(&root),
            root,
        }
    }

    /// Open a `ManifestHistory` against this fixture's clone.
    pub fn history(&self, ctx: &InstallCtx) -> ManifestHistory {
        ManifestHistory::open(&self.remote, ctx.cache()).expect("open mirror")
    }
}

/// Run a git subcommand in `repo`, asserting success.
fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(repo)
        .args(args)
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed");
}

/// Commit the staged tree with a fixed identity, signing disabled, and the
/// author/committer date set to `date` so `git log` ordering is deterministic.
fn git_commit(repo: &Path, message: &str, date: &str) {
    let status = Command::new("git")
        .current_dir(repo)
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .args([
            "-c",
            "user.email=test@example.com",
            "-c",
            "user.name=test",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            message,
        ])
        .status()
        .expect("spawn git commit");
    assert!(status.success(), "git commit failed");
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
    let entries = fx.history(&ctx).index(&[VsVersion::Vs2026]).unwrap();
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
    let entry = fx.history(&ctx).resolve_build_version("18.6.11819.183").unwrap();
    assert_eq!(entry.info.build_version, "18.6.11819.183");
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
    let err = fx.history(&ctx).resolve_build_version("18.99.99.99").unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("18.99.99.99"), "got: {msg}");
    assert!(msg.contains("18.6.11819.183"), "got: {msg}");
}

#[test]
fn resolve_build_version_rejects_unknown_major() {
    let tmp = TempDir::new().unwrap();
    let fx = MirrorFixture::build(tmp.path(), &[]);
    let ctx = test_ctx(&tmp);
    let err = fx.history(&ctx).resolve_build_version("15.0.0.0").unwrap_err();
    assert!(err.to_string().contains("15"), "got: {err}");
}

#[test]
fn commits_filtered_to_channel_json_touching() {
    // Regression guard: enumeration must skip the mirror's initial CI-setup
    // commits that lack channel.json. We commit one such bare commit, then a
    // real snapshot; only the snapshot should appear in the index.
    let tmp = TempDir::new().unwrap();
    let fx = MirrorFixture::build(
        tmp.path(),
        &[(
            VsVersion::Vs2026,
            &[FxSnapshot {
                sha: "real",
                date: "2026-05-01T00:00:00Z",
                build_version: "18.6.0.0",
                display_version: "18.6",
                product_line_version: "18",
                manifest_packages_json: String::new(),
            }],
        )],
    );
    // Add a commit on release-18 that does NOT touch channel.json.
    std::fs::write(fx.root.join("README.md"), b"ci setup").unwrap();
    git(&fx.root, &["add", "README.md"]);
    git_commit(&fx.root, "ci setup", "2026-05-02T00:00:00Z");

    let ctx = test_ctx(&tmp);
    let entries = fx.history(&ctx).index(&[VsVersion::Vs2026]).unwrap();
    assert_eq!(entries.len(), 1, "only channel.json commits should appear");
    assert_eq!(entries[0].info.build_version, "18.6.0.0");
}
