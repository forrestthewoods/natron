//! Source of truth for both `msvc` and `windows_sdk` providers.
//!
//! All data comes from the `roblabla/msvc-manifest-history` GitHub mirror.
//! Each commit on a `release-{16,17,18}` branch is one Microsoft VS release
//! snapshot, uniquely identified by `info.buildVersion` in its small
//! `channel.json` header (~130 KB). The full package list lives in the
//! per-commit `manifest.json` (~15-25 MB).
//!
//! Pinning a `buildVersion` resolves to one commit_sha → one immutable
//! manifest → one fixed set of CDN payload URLs. That's the only string
//! that guarantees a reproducible install.

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

use super::InstallCtx;

const MIRROR_OWNER: &str = "roblabla";
const MIRROR_REPO: &str = "msvc-manifest-history";
const USER_AGENT: &str = concat!("natron/", env!("CARGO_PKG_VERSION"));

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

// ---- URL builders ----------------------------------------------------------

/// raw.githubusercontent.com URL pattern. The mirror parameter exists for
/// test fixtures (`file://` URLs); production uses [`default_raw_base`].
pub fn raw_url(base: &str, sha_or_branch: &str, filename: &str) -> String {
    format!("{base}/{sha_or_branch}/{filename}")
}

pub fn default_raw_base() -> String {
    format!("https://raw.githubusercontent.com/{MIRROR_OWNER}/{MIRROR_REPO}")
}

/// GitHub commits API for one release branch. Test fixtures override the
/// base to point at a local `commits.json` file via `{branch}`.
pub fn commits_url(base: &str, vs: VsVersion, page: u32) -> String {
    base.replace("{branch}", &format!("release-{}", vs.channel()))
        .replace("{page}", &page.to_string())
}

pub fn default_commits_base() -> String {
    format!(
        "https://api.github.com/repos/{MIRROR_OWNER}/{MIRROR_REPO}/commits?sha={{branch}}&per_page=100&page={{page}}"
    )
}

// ---- fetchers --------------------------------------------------------------

/// Enumerate every commit on one release branch via the GitHub commits API.
/// NOT cached — the response is tiny (~10 KB/page) and freshness matters
/// when a new Microsoft release has just shipped.
pub fn fetch_commits(commits_base: &str, vs: VsVersion) -> Result<Vec<CommitRef>> {
    let mut out = Vec::new();
    for page in 1u32.. {
        let url = commits_url(commits_base, vs, page);
        let body = http_get(&url)
            .with_context(|| format!("fetching commits page {page} for {}", vs.as_str()))?;
        let page_commits: Vec<GhCommit> = serde_json::from_str(&body)
            .with_context(|| format!("parsing commits page {page} for {}", vs.as_str()))?;
        if page_commits.is_empty() {
            break;
        }
        let was_full = page_commits.len() == 100;
        for c in page_commits {
            out.push(CommitRef {
                sha: c.sha,
                date: c.commit.author.date,
            });
        }
        if !was_full {
            break;
        }
    }
    Ok(out)
}

#[derive(Deserialize)]
struct GhCommit {
    sha: String,
    commit: GhCommitInner,
}

#[derive(Deserialize)]
struct GhCommitInner {
    author: GhAuthor,
}

#[derive(Deserialize)]
struct GhAuthor {
    date: String,
}

/// Fetch + parse `channel.json` at a specific commit. Cached forever via
/// the download layer (commit-sha-keyed URLs are immutable).
pub fn fetch_channel_info(
    raw_base: &str,
    sha_or_branch: &str,
    ctx: &InstallCtx,
) -> Result<ChannelInfo> {
    let url = raw_url(raw_base, sha_or_branch, "channel.json");
    let path = ctx
        .download(&url, None)
        .with_context(|| format!("fetching channel.json at {sha_or_branch}"))?;
    #[derive(Deserialize)]
    struct Wrapper {
        info: ChannelInfo,
    }
    parse_json_or_evict::<Wrapper>(&path, "channel.json").map(|w| w.info)
}

/// Fetch + parse `manifest.json` at a specific commit. Cached forever.
pub fn fetch_manifest_at(
    raw_base: &str,
    sha_or_branch: &str,
    ctx: &InstallCtx,
) -> Result<VsManifest> {
    let url = raw_url(raw_base, sha_or_branch, "manifest.json");
    let path = ctx
        .download(&url, None)
        .with_context(|| format!("fetching manifest.json at {sha_or_branch}"))?;
    parse_json_or_evict(&path, "manifest.json")
}

/// Read a JSON file from disk and parse it. On parse failure, delete the
/// cached file so the next call re-fetches (defensive against upstream
/// briefly serving HTML or a truncated response that the download layer
/// happened to cache).
fn parse_json_or_evict<T: serde::de::DeserializeOwned>(
    path: &std::path::Path,
    label: &str,
) -> Result<T> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading {label} at {}", path.display()))?;
    match serde_json::from_str(&text) {
        Ok(v) => Ok(v),
        Err(err) => {
            let _ = std::fs::remove_file(path);
            Err(anyhow!(
                "parsing {label} at {} (cached file deleted; re-run will refetch): {err}",
                path.display(),
            ))
        }
    }
}

// ---- index builder ---------------------------------------------------------

/// Configuration for the fetchers — bundled so tests can swap both URL
/// bases at once.
#[derive(Debug, Clone)]
pub struct MirrorUrls {
    pub raw_base: String,
    pub commits_base: String,
}

impl Default for MirrorUrls {
    fn default() -> Self {
        Self {
            raw_base: default_raw_base(),
            commits_base: default_commits_base(),
        }
    }
}

/// Build the in-memory index across the requested VS series. For each
/// series: list commits, fetch every commit's channel.json (cached after
/// first run). Sorted descending by commit date within each VS series.
pub fn build_index(
    urls: &MirrorUrls,
    series: &[VsVersion],
    ctx: &InstallCtx,
) -> Result<Vec<BuildIndexEntry>> {
    let mut out = Vec::new();
    for vs in series {
        let mut commits = fetch_commits(&urls.commits_base, *vs)?;
        commits.sort_by(|a, b| b.date.cmp(&a.date));
        for commit in commits {
            match fetch_channel_info(&urls.raw_base, &commit.sha, ctx) {
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

/// Resolve a buildVersion to its mirror entry. Auto-detects the VS series
/// from the version's major component. On miss, surfaces a few of the
/// nearest available buildVersions for context.
pub fn resolve_build_version(
    urls: &MirrorUrls,
    build_version: &str,
    ctx: &InstallCtx,
) -> Result<BuildIndexEntry> {
    let major = build_version_major(build_version)?;
    let vs = VsVersion::from_channel(major)?;
    let entries = build_index(urls, &[vs], ctx)?;
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

/// Parse the major component (first dot-segment) of a buildVersion.
pub fn build_version_major(build_version: &str) -> Result<u8> {
    let head = build_version
        .split('.')
        .next()
        .ok_or_else(|| anyhow!("buildVersion '{build_version}' is empty"))?;
    head.parse::<u8>()
        .map_err(|e| anyhow!("buildVersion '{build_version}' major is not numeric: {e}"))
}

// ---- HTTP helper -----------------------------------------------------------

/// Plain GET. Used for the GitHub commits API — small JSON, no caching,
/// must set a User-Agent (GitHub requires it).
pub fn http_get(url: &str) -> Result<String> {
    if let Some(rest) = url.strip_prefix("file://") {
        // For test fixtures.
        let p = url::Url::parse(url)
            .ok()
            .and_then(|u| u.to_file_path().ok())
            .unwrap_or_else(|| std::path::PathBuf::from(rest));
        return std::fs::read_to_string(&p)
            .with_context(|| format!("reading file URL {}", p.display()));
    }
    let resp = ureq::get(url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/json")
        .call()
        .with_context(|| format!("GET {url}"))?;
    Ok(resp.into_body().read_to_string()?)
}

#[cfg(test)]
#[path = "vs_manifest_tests.rs"]
pub(crate) mod tests;
