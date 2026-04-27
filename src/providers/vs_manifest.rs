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
mod tests {
    use super::*;

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
}
