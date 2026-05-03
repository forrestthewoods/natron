//! Internal helper for fetching + parsing the Visual Studio channel manifest.
//! Shared between the `msvc` and `windows_sdk` providers.

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

use super::InstallCtx;

/// Default URL template for the channel manifest. `{channel}` is substituted
/// with the user-provided VS channel (e.g. "18" for VS 2022 + 17 series).
pub const DEFAULT_CHANNEL_URL_TEMPLATE: &str = "https://aka.ms/vs/{channel}/stable/channel";

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
