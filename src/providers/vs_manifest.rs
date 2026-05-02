//! Internal helper for fetching + parsing the Visual Studio channel manifest.
//! Shared between the `msvc` and `windows_sdk` providers.

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

use super::InstallCtx;

/// Default URL template for the channel manifest. `{channel}` is substituted
/// with the user-provided VS channel (e.g. "18" for VS 2022 + 17 series).
pub const DEFAULT_CHANNEL_URL_TEMPLATE: &str = "https://aka.ms/vs/{channel}/stable/channel";

/// Default URL template for listing commits on a branch of the
/// `roblabla/msvc-manifest-history` mirror. `{channel}` substitutes the VS
/// channel (e.g. "17"); `{page}` is the 1-indexed page number.
///
/// The mirror's branch convention is `release-<channel>` for stable releases.
pub const DEFAULT_HISTORY_COMMITS_URL_TEMPLATE: &str =
    "https://api.github.com/repos/roblabla/msvc-manifest-history/commits?sha=release-{channel}&per_page=100&page={page}";

/// Default URL template for fetching a single historical `manifest.json` blob
/// at a specific commit SHA. `{sha}` substitutes the commit hash.
pub const DEFAULT_HISTORY_RAW_URL_TEMPLATE: &str =
    "https://raw.githubusercontent.com/roblabla/msvc-manifest-history/{sha}/manifest.json";

/// How many pages of commits (100 each) to scan before giving up. The mirror
/// updates roughly once per upstream channel bump, so 5 pages comfortably
/// covers more than a year of releases.
pub const DEFAULT_HISTORY_MAX_PAGES: u32 = 5;

/// Channel manifest (the small JSON returned from the aka.ms URL).
#[derive(Debug, Deserialize)]
pub struct ChannelManifest {
    #[serde(default, rename = "channelItems")]
    pub channel_items: Vec<ChannelItem>,
}

#[derive(Debug, Deserialize)]
pub struct ChannelItem {
    #[serde(default)]
    #[serde(rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub payloads: Vec<Payload>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Payload {
    pub url: String,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default, rename = "fileName")]
    pub file_name: Option<String>,
}

/// Full VS manifest (the much larger JSON the channel manifest points at).
#[derive(Debug, Deserialize)]
pub struct VsManifest {
    #[serde(default)]
    pub packages: Vec<Package>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Package {
    pub id: String,
    #[serde(default)]
    pub payloads: Vec<Payload>,
    /// Many VS packages exist in multiple variants distinguished only by
    /// `language` (e.g. en-US, cs-CZ, ja-JP). When picking by id alone we
    /// must filter by language or we'll get the first one alphabetically
    /// (cs-CZ).
    #[serde(default)]
    pub language: Option<String>,
    /// VS package dependencies. The KEYS of this map are the dependent
    /// package ids; the VALUES are version constraints we don't currently
    /// inspect.
    #[serde(default)]
    pub dependencies: std::collections::HashMap<String, serde_json::Value>,
}

/// Fetch the channel manifest, follow it to the VS manifest, and return the
/// fully parsed package list.
pub fn fetch_vs_manifest(
    channel_url_template: &str,
    vs_channel: &str,
    ctx: &InstallCtx,
) -> Result<VsManifest> {
    let channel_url = channel_url_template.replace("{channel}", vs_channel);
    let channel_path = ctx
        .download(&channel_url, None)
        .with_context(|| format!("fetching VS channel manifest from {channel_url}"))?;
    let channel_text = std::fs::read_to_string(&channel_path)
        .with_context(|| format!("reading {}", channel_path.display()))?;
    let channel: ChannelManifest = serde_json::from_str(&channel_text)
        .with_context(|| format!("parsing channel manifest from {}", channel_path.display()))?;

    // Find the VS manifest URL.
    let vs_url = channel
        .channel_items
        .iter()
        .find(|item| {
            item.kind.as_deref() == Some("Manifest")
                && item.id.as_deref()
                    == Some("Microsoft.VisualStudio.Manifests.VisualStudio")
        })
        .ok_or_else(|| anyhow!("VS channel manifest has no VS manifest item"))?
        .payloads
        .first()
        .ok_or_else(|| anyhow!("VS channel manifest item has no payloads"))?
        .url
        .clone();

    let vs_path = ctx
        .download(&vs_url, None)
        .with_context(|| format!("fetching VS manifest from {vs_url}"))?;
    let vs_text = std::fs::read_to_string(&vs_path)
        .with_context(|| format!("reading {}", vs_path.display()))?;
    let vs: VsManifest = serde_json::from_str(&vs_text)
        .with_context(|| format!("parsing VS manifest from {}", vs_path.display()))?;
    Ok(vs)
}

/// One entry from the GitHub `/commits` listing — we only need `sha`.
#[derive(Debug, Deserialize)]
struct CommitEntry {
    sha: String,
}

/// Walk the commit history of `roblabla/msvc-manifest-history` for the given
/// VS channel, fetching `manifest.json` at each commit until `is_match`
/// accepts it. Workaround for the lack of a public "exact MSVC version"
/// download API (issue #1).
///
/// Stops after `max_pages` of 100 commits each, or when a page comes back
/// empty (end of branch). Each fetched commit's manifest is cached via the
/// shared download cache, so a repeated lookup for an already-seen version
/// is a local-disk parse pass with no network.
pub fn find_vs_manifest_in_history(
    commits_url_template: &str,
    raw_url_template: &str,
    vs_channel: &str,
    max_pages: u32,
    ctx: &InstallCtx,
    mut is_match: impl FnMut(&VsManifest) -> bool,
) -> Result<VsManifest> {
    let mut scanned = 0usize;
    for page in 1..=max_pages {
        let commits_url = commits_url_template
            .replace("{channel}", vs_channel)
            .replace("{page}", &page.to_string());
        let path = ctx.download(&commits_url, None).with_context(|| {
            format!("listing manifest-history commits ({commits_url})")
        })?;
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let commits: Vec<CommitEntry> = serde_json::from_str(&text)
            .with_context(|| format!("parsing commits JSON from {}", path.display()))?;
        if commits.is_empty() {
            break;
        }
        for commit in &commits {
            scanned += 1;
            let raw_url = raw_url_template.replace("{sha}", &commit.sha);
            let path = match ctx.download(&raw_url, None) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(
                        "skipping manifest-history commit {}: {e}",
                        commit.sha
                    );
                    continue;
                }
            };
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!(
                        "reading manifest at commit {}: {e}",
                        commit.sha
                    );
                    continue;
                }
            };
            let manifest: VsManifest = match serde_json::from_str(&text) {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(
                        "parsing manifest at commit {}: {e}",
                        commit.sha
                    );
                    continue;
                }
            };
            if is_match(&manifest) {
                tracing::info!(
                    "manifest-history: matched at commit {} (channel={vs_channel}, scanned={scanned})",
                    commit.sha
                );
                return Ok(manifest);
            }
        }
    }
    bail!(
        "no matching manifest found in roblabla/msvc-manifest-history for channel {vs_channel} (scanned {scanned} commits across {max_pages} pages)"
    );
}

/// Sort version strings as a list of dot-separated integer components, so
/// "14.50.18" > "14.49.99". Falls back to lexicographic for non-numeric.
fn version_key(v: &str) -> Vec<u64> {
    v.split('.')
        .map(|s| s.parse::<u64>().unwrap_or(0))
        .collect()
}

impl VsManifest {
    /// Find a package by exact id (case-insensitive). When multiple variants
    /// exist (different `language` attribute), prefer en-US, then no
    /// language at all, then any.
    pub fn find_package(&self, id: &str) -> Option<&Package> {
        let lower = id.to_lowercase();
        let matches: Vec<&Package> = self
            .packages
            .iter()
            .filter(|p| p.id.to_lowercase() == lower)
            .collect();
        if matches.is_empty() {
            return None;
        }
        // Prefer en-US.
        if let Some(p) = matches
            .iter()
            .copied()
            .find(|p| p.language.as_deref() == Some("en-US"))
        {
            return Some(p);
        }
        // Then language-less.
        if let Some(p) = matches.iter().copied().find(|p| p.language.is_none()) {
            return Some(p);
        }
        // Fall back to first match.
        Some(matches[0])
    }

    /// Find every MSVC compiler+CRT package matching `microsoft.vc.{ver}.tools.host{host}.target{target}.base`.
    /// Returns `(version_string, package_id)` pairs, sorted descending by
    /// version. "Premium" variants are excluded (we want the base toolchain).
    pub fn find_msvc_candidates(&self, host: &str, target: &str) -> Vec<(String, String)> {
        let host = host.to_lowercase();
        let target = target.to_lowercase();
        let needle = format!(".tools.host{host}.target{target}.base");
        let mut out = Vec::new();
        for pkg in &self.packages {
            let id_lower = pkg.id.to_lowercase();
            if !id_lower.starts_with("microsoft.vc.") {
                continue;
            }
            if !id_lower.contains(&needle) {
                continue;
            }
            if id_lower.contains(".premium.") {
                continue;
            }
            // Extract version: between "microsoft.vc." and ".tools."
            let after = &pkg.id["microsoft.vc.".len()..];
            let Some(end) = after.to_lowercase().find(".tools.") else {
                continue;
            };
            let version = &after[..end];
            out.push((version.to_string(), pkg.id.clone()));
        }
        out.sort_by(|a, b| version_key(&b.0).cmp(&version_key(&a.0)));
        out
    }

    /// Find every Windows SDK component package matching
    /// `Microsoft.VisualStudio.Component.Windows{10|11}SDK.{numeric_version}`.
    pub fn find_sdk_candidates(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for pkg in &self.packages {
            for prefix in [
                "Microsoft.VisualStudio.Component.Windows10SDK.",
                "Microsoft.VisualStudio.Component.Windows11SDK.",
            ] {
                if let Some(rest) = pkg.id.strip_prefix(prefix) {
                    if !rest.is_empty()
                        && rest.chars().all(|c| c.is_ascii_digit() || c == '.')
                    {
                        out.push((rest.to_string(), pkg.id.clone()));
                    }
                }
            }
        }
        out.sort_by(|a, b| version_key(&b.0).cmp(&version_key(&a.0)));
        out
    }

    /// Find a payload by exact filename within a package id.
    #[allow(dead_code)]
    pub fn find_payload<'a>(
        &'a self,
        package_id: &str,
        file_name: &str,
    ) -> Option<&'a Payload> {
        self.find_package(package_id)?
            .payloads
            .iter()
            .find(|p| p.file_name.as_deref() == Some(file_name))
    }
}
#[cfg(test)]
#[path = "vs_manifest_tests.rs"]
mod tests;
