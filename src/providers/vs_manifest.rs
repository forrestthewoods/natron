//! Source of truth for both `msvc` and `windows_sdk` providers.
//!
//! All data comes from the `roblabla/msvc-manifest-history` GitHub mirror.
//! Each commit on a `release-{16,17,18}` branch is one Microsoft VS release
//! snapshot, uniquely identified by `info.buildVersion` in its small
//! `channel.json` header (~130 KB). The full package list lives in the
//! per-commit `manifest.json` (~15-25 MB).
//!
//! We read the mirror through a local **partial clone** rather than the
//! GitHub REST API (whose anonymous rate limit is 60 req/hr per IP). A bare
//! `git clone --filter=blob:limit=1m` pulls every commit + tree + the small
//! `channel.json` blobs, deferring the big `manifest.json` blobs to on-demand
//! `git cat-file` (the promisor remote fetches them lazily). `git` is a hard
//! requirement; there is no API fallback. See [`ManifestHistory`].
//!
//! Pinning a `buildVersion` resolves to one commit_sha → one immutable
//! manifest → one fixed set of CDN payload URLs. That's the only string
//! that guarantees a reproducible install.

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::cache::Cache;
use crate::fs_util;

const MIRROR_OWNER: &str = "roblabla";
const MIRROR_REPO: &str = "msvc-manifest-history";

// ---- types -----------------------------------------------------------------

/// User-facing VS product line. Maps 1-1 to a release branch on the mirror.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VsVersion {
    Vs2019,
    Vs2022,
    Vs2026,
}

impl VsVersion {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "vs2019" => Ok(Self::Vs2019),
            "vs2022" => Ok(Self::Vs2022),
            "vs2026" => Ok(Self::Vs2026),
            other => bail!("invalid vs value '{other}'; valid: vs2019, vs2022, vs2026"),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Vs2019 => "vs2019",
            Self::Vs2022 => "vs2022",
            Self::Vs2026 => "vs2026",
        }
    }

    pub fn channel(self) -> u8 {
        match self {
            Self::Vs2019 => 16,
            Self::Vs2022 => 17,
            Self::Vs2026 => 18,
        }
    }

    pub fn all() -> [Self; 3] {
        [Self::Vs2019, Self::Vs2022, Self::Vs2026]
    }

    /// Map a `buildVersion`'s major component (16/17/18) back to the VS series.
    pub fn from_channel(major: u8) -> Result<Self> {
        match major {
            16 => Ok(Self::Vs2019),
            17 => Ok(Self::Vs2022),
            18 => Ok(Self::Vs2026),
            other => bail!(
                "buildVersion major '{other}' has no matching VS series (expected 16, 17, or 18)"
            ),
        }
    }
}

/// Parsed `info` block from a `channel.json` snapshot.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelInfo {
    pub build_version: String,
    pub product_display_version: String,
    pub product_line_version: String,
}

#[derive(Debug, Clone)]
pub struct CommitRef {
    pub sha: String,
    pub date: String,
}

/// One Microsoft snapshot paired with the mirror commit that hosts it.
#[derive(Debug, Clone)]
pub struct BuildIndexEntry {
    pub vs: VsVersion,
    pub info: ChannelInfo,
    pub commit: CommitRef,
}

/// Subset of the full VS manifest we deserialize.
#[derive(Debug, Deserialize)]
pub struct VsManifest {
    #[serde(default)]
    pub packages: Vec<Package>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Package {
    pub id: String,
    /// Microsoft's package version (for MSVC, the toolset version like
    /// `14.52.36328`).
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub payloads: Vec<Payload>,
    /// Many packages exist in per-language variants (en-US, ja-JP, ...).
    /// Compiler-base packages are languageless.
    #[serde(default)]
    pub language: Option<String>,
    /// Declared dependency ids. Values are version constraints we don't read.
    #[serde(default)]
    pub dependencies: std::collections::HashMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Payload {
    pub url: String,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default, rename = "fileName")]
    pub file_name: Option<String>,
}

// ---- manifest history (local partial clone) --------------------------------

/// Default git remote for the manifest-history mirror.
pub fn default_remote() -> String {
    format!("https://github.com/{MIRROR_OWNER}/{MIRROR_REPO}")
}

/// A local bare partial clone of `roblabla/msvc-manifest-history`, opened for
/// reading. [`ManifestHistory::open`] clones (or `git fetch`es) once; the
/// query methods are then pure-local `git log` / `git cat-file`.
///
/// NOT general purpose: the `release-{channel}` branch layout and the
/// `channel.json` / `manifest.json` filenames are specific to that mirror.
pub struct ManifestHistory {
    git_dir: PathBuf,
}

impl ManifestHistory {
    /// Open the mirror under `<cache>/meta/msvc-manifest-history`, cloning it
    /// (`--filter=blob:limit=1m`: commits + trees + the small `channel.json`
    /// blobs, deferring the big `manifest.json` blobs) if absent, else
    /// `git fetch`ing to pick up newly shipped releases. The fetch is
    /// best-effort: a failure (e.g. a peer holds git's lock, or the network is
    /// down) leaves the existing clone usable. Requires `git` on PATH.
    pub fn open(remote: &str, cache: &Cache) -> Result<Self> {
        let git_dir = cache.meta.join(MIRROR_REPO);
        if git_dir.is_dir() {
            let _ = git_in(&git_dir, &["fetch", "--quiet", "origin"]);
        } else {
            clone_into(remote, &git_dir, &cache.meta)?;
        }
        Ok(Self { git_dir })
    }

    /// All builds across the requested series, newest-first within each.
    pub fn index(&self, series: &[VsVersion]) -> Result<Vec<BuildIndexEntry>> {
        let mut out = Vec::new();
        for vs in series {
            let mut commits = self.commits_on_branch(*vs)?;
            commits.sort_by(|a, b| b.date.cmp(&a.date));
            for commit in commits {
                match self.channel_info_at(&commit.sha) {
                    Ok(info) => out.push(BuildIndexEntry {
                        vs: *vs,
                        info,
                        commit,
                    }),
                    Err(err) => tracing::warn!(
                        "skipping commit {} on {}: {err:#}",
                        &commit.sha[..commit.sha.len().min(7)],
                        vs.as_str(),
                    ),
                }
            }
        }
        Ok(out)
    }

    /// Newest build for one series, if any.
    pub fn newest(&self, vs: VsVersion) -> Result<Option<BuildIndexEntry>> {
        Ok(self.index(&[vs])?.into_iter().next())
    }

    /// Resolve a buildVersion to its entry. Auto-detects the VS series from
    /// the version's major component. On miss, surfaces a few of the nearest
    /// available buildVersions for context.
    pub fn resolve_build_version(&self, build_version: &str) -> Result<BuildIndexEntry> {
        let major = build_version_major(build_version)?;
        let vs = VsVersion::from_channel(major)?;
        let entries = self.index(&[vs])?;
        if let Some(hit) = entries
            .iter()
            .find(|e| e.info.build_version == build_version)
        {
            return Ok(hit.clone());
        }
        let mut available: Vec<&str> = entries
            .iter()
            .map(|e| e.info.build_version.as_str())
            .collect();
        available.sort();
        // Show 5 nearest (lex-closest) so the error fits on a terminal.
        let suggestions: Vec<&&str> = available
            .iter()
            .filter(|v| v.starts_with(&format!("{major}.")))
            .take(5)
            .collect();
        bail!(
            "buildVersion '{build_version}' not found on {} (release-{major}); {} available; nearest: {}",
            vs.as_str(),
            available.len(),
            if suggestions.is_empty() {
                "(none)".to_string()
            } else {
                suggestions
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            },
        )
    }

    /// Read + parse `manifest.json` at a commit. The blob is excluded from the
    /// partial clone, so `git cat-file` lazily fetches it from the promisor
    /// remote on first access (cached in the clone thereafter).
    pub fn manifest(&self, sha: &str) -> Result<VsManifest> {
        let bytes = self
            .cat_blob(sha, "manifest.json")
            .with_context(|| format!("reading manifest.json at {sha}"))?;
        serde_json::from_slice(&bytes).with_context(|| format!("parsing manifest.json at {sha}"))
    }

    /// List the commits on one release branch that touched `channel.json`,
    /// skipping the mirror's initial CI-setup commits that lack it. Unsorted;
    /// callers sort by date.
    fn commits_on_branch(&self, vs: VsVersion) -> Result<Vec<CommitRef>> {
        let branch = format!("release-{}", vs.channel());
        let stdout = git_in(
            &self.git_dir,
            &["log", "--format=%H %aI", branch.as_str(), "--", "channel.json"],
        )
        .with_context(|| format!("listing commits on {branch}"))?;
        let text = String::from_utf8(stdout).context("git log output was not UTF-8")?;
        Ok(text
            .lines()
            .filter_map(|line| line.split_once(' '))
            .map(|(sha, date)| CommitRef {
                sha: sha.to_string(),
                date: date.to_string(),
            })
            .collect())
    }

    /// Read + parse `channel.json` at a commit straight from the local clone.
    fn channel_info_at(&self, sha: &str) -> Result<ChannelInfo> {
        let bytes = self
            .cat_blob(sha, "channel.json")
            .with_context(|| format!("reading channel.json at {sha}"))?;
        #[derive(Deserialize)]
        struct Wrapper {
            info: ChannelInfo,
        }
        let wrapper: Wrapper = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing channel.json at {sha}"))?;
        Ok(wrapper.info)
    }

    /// `git cat-file blob <sha>:<path>` — the bytes of a file at a commit.
    fn cat_blob(&self, sha: &str, path: &str) -> Result<Vec<u8>> {
        let spec = format!("{sha}:{path}");
        git_in(&self.git_dir, &["cat-file", "blob", spec.as_str()])
    }
}

/// Run `git <args>` and return stdout. Maps a missing `git` binary to a clear
/// error; treats a non-zero exit as a failure carrying git's stderr.
fn run_git(mut cmd: Command) -> Result<Vec<u8>> {
    let out = cmd.output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow!(
                "`git` was not found on PATH; natron needs git to read the MSVC / Windows SDK manifest mirror"
            )
        } else {
            anyhow!("spawning git: {e}")
        }
    })?;
    if !out.status.success() {
        bail!("git failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

/// `git -C <dir> <args>`.
fn git_in(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).args(args);
    run_git(cmd)
}

/// Bare partial clone of `remote` into `dest` (a subdir of `meta_dir`). Clones
/// into a unique temp dir under `meta_dir` then atomically renames it into
/// place, so two concurrent natron runs can't corrupt a shared clone (same
/// lock-free publish as the CAS).
fn clone_into(remote: &str, dest: &Path, meta_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(meta_dir)
        .with_context(|| format!("creating {}", meta_dir.display()))?;
    let tmp = meta_dir.join(format!(".tmp-{}", uuid::Uuid::new_v4()));
    let mut cmd = Command::new("git");
    cmd.args(["clone", "--bare", "--filter=blob:limit=1m"])
        .arg(remote)
        .arg(&tmp);
    if let Err(err) = run_git(cmd) {
        let _ = fs_util::remove_dir_all_writable(&tmp);
        return Err(err).with_context(|| format!("cloning manifest mirror from {remote}"));
    }
    if std::fs::rename(&tmp, dest).is_err() {
        // A peer published the clone first; keep theirs, drop ours.
        let _ = fs_util::remove_dir_all_writable(&tmp);
    }
    Ok(())
}

/// Parse the major component (first dot-segment) of a buildVersion.
pub fn build_version_major(build_version: &str) -> Result<u8> {
    let head = build_version
        .split('.')
        .next()
        .ok_or_else(|| anyhow!("buildVersion '{build_version}' is empty"))?;
    head.parse::<u8>()
        .map_err(|e| anyhow!("buildVersion '{build_version}' major is not numeric: {e}"))
}

#[cfg(test)]
#[path = "tests/vs_manifest.rs"]
pub(crate) mod tests;
