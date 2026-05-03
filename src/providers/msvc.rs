//! `msvc` provider: downloads the MSVC compiler + CRT packages from a
//! Visual Studio channel manifest. Windows-specific install (uses
//! `msiexec.exe` / VSIX zip extraction).
//!
//! NOTE: this provider produces ONLY the compiler + CRT tree. The Windows
//! SDK is a separate provider (`windows_sdk`). The two are logically
//! independent.

use anyhow::{Result, anyhow, bail};
use serde::Deserialize;

use super::vs_manifest::{self, VsManifest};
use super::{InstallCtx, Installed, Provider};
use crate::cache::sanitize_fingerprint;
use crate::extract;

pub const ID: &str = "msvc";

const HOST: &str = "x64";
const TARGET: &str = "x64";

/// URL templates for the `roblabla/msvc-manifest-history` mirror, our
/// workaround for issue #1: Microsoft only publishes the latest channel
/// manifest, so older MSVC versions need to be looked up in this history.
/// The mirror's branch convention is `release-<channel>` for stable releases.
/// `{channel}` substitutes the VS channel; `{page}` is 1-indexed; `{sha}` is
/// a commit hash.
const HISTORY_COMMITS_URL_TEMPLATE: &str =
    "https://api.github.com/repos/roblabla/msvc-manifest-history/commits?sha=release-{channel}&per_page=100&page={page}";
const HISTORY_RAW_URL_TEMPLATE: &str =
    "https://raw.githubusercontent.com/roblabla/msvc-manifest-history/{sha}/manifest.json";

/// Cap on commit pages (100 each) scanned during a manifest-history walk.
/// The mirror updates roughly once per upstream channel bump, so 5 pages
/// comfortably covers more than a year of releases.
const HISTORY_MAX_PAGES: u32 = 5;

pub struct MsvcProvider {
    channel_url_template: String,
    history_commits_url_template: String,
    history_raw_url_template: String,
}

impl MsvcProvider {
    pub fn new() -> Self {
        Self {
            channel_url_template: vs_manifest::DEFAULT_CHANNEL_URL_TEMPLATE.to_string(),
            history_commits_url_template: HISTORY_COMMITS_URL_TEMPLATE.to_string(),
            history_raw_url_template: HISTORY_RAW_URL_TEMPLATE.to_string(),
        }
    }

    pub fn with_channel_url_template(template: impl Into<String>) -> Self {
        Self {
            channel_url_template: template.into(),
            ..Self::new()
        }
    }

    /// Override the manifest-history URL templates. `commits_template` honors
    /// `{channel}` and `{page}`; `raw_template` honors `{sha}`. Used by tests
    /// to point at `file://` fixtures. Chainable so a test can override both
    /// the channel URL and the history URLs in one expression.
    pub fn with_history_urls(
        mut self,
        commits_template: impl Into<String>,
        raw_template: impl Into<String>,
    ) -> Self {
        self.history_commits_url_template = commits_template.into();
        self.history_raw_url_template = raw_template.into();
        self
    }
}

impl Default for MsvcProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for MsvcProvider {
    fn id(&self) -> &'static str {
        ID
    }

    fn install(
        &self,
        options: &toml::Table,
        ctx: &mut InstallCtx,
    ) -> Result<Installed> {
        let vs_channel = options
            .get("vs_channel")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                anyhow!("`msvc` provider requires options.vs_channel (string)")
            })?;
        let pinned_version = options.get("msvc_version").and_then(|v| v.as_str());
        let manifest_history = options
            .get("manifest_history")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // Fast-path when version is pinned: deterministic fingerprint, no
        // network needed.
        if let Some(ver) = pinned_version {
            let fp = sanitize_fingerprint(&format!("msvc-{ver}-{vs_channel}"));
            if ctx.cache().install_present(&fp) {
                return Ok(Installed {
                    fingerprint: fp,
                    display: format!("msvc {ver} (vs{vs_channel})"),
                    options: options.clone(),
                    freshly_extracted: false,
                });
            }
        }

        // Need the manifest. With manifest_history we walk
        // roblabla/msvc-manifest-history for the commit whose manifest still
        // contains the requested version; otherwise fetch the live channel
        // manifest from aka.ms.
        let manifest = if manifest_history {
            let want = pinned_version.ok_or_else(|| {
                anyhow!(
                    "`msvc` provider: `manifest_history = true` requires `msvc_version` to be pinned"
                )
            })?;
            find_manifest_in_history(
                &self.history_commits_url_template,
                &self.history_raw_url_template,
                vs_channel,
                want,
                ctx,
            )?
        } else {
            vs_manifest::fetch_vs_manifest(
                &self.channel_url_template,
                vs_channel,
                ctx,
            )?
        };
        let candidates = manifest.find_msvc_candidates(HOST, TARGET);
        if candidates.is_empty() {
            anyhow::bail!(
                "no MSVC packages found in VS manifest for host={HOST} target={TARGET}"
            );
        }

        let (resolved_ver, base_pkg_id) = match pinned_version {
            Some(req) => candidates
                .iter()
                .find(|(v, _)| v == req)
                .ok_or_else(|| {
                    let avail: Vec<_> = candidates.iter().map(|(v, _)| v.as_str()).collect();
                    anyhow!(
                        "msvc_version='{req}' not in manifest; available: {:?}",
                        avail
                    )
                })?
                .clone(),
            None => candidates[0].clone(),
        };

        let fp = sanitize_fingerprint(&format!("msvc-{resolved_ver}-{vs_channel}"));
        if ctx.cache().install_present(&fp) {
            return Ok(Installed {
                fingerprint: fp,
                display: format!("msvc {resolved_ver} (vs{vs_channel})"),
                options: resolved_options(options, &resolved_ver),
                freshly_extracted: false,
            });
        }

        // Collect all payloads from MSVC base + companion packages.
        // The Res.base package carries language resource DLLs (clui.dll
        // etc.) that cl.exe needs to even start up.
        let host_lower = HOST.to_lowercase();
        let target_lower = TARGET.to_lowercase();
        let companion_ids = vec![
            base_pkg_id,
            format!(
                "Microsoft.VC.{resolved_ver}.Tools.Host{host_lower}.Target{target_lower}.Res.base"
            ),
            format!("Microsoft.VC.{resolved_ver}.CRT.Headers.base"),
            format!("Microsoft.VC.{resolved_ver}.CRT.{target_lower}.Desktop.base"),
            format!("Microsoft.VC.{resolved_ver}.CRT.{target_lower}.Store.base"),
        ];

        let payloads = collect_payloads(&manifest, &companion_ids)?;

        // Download + extract into staging.
        let staging_raw = ctx.staging_dir()?;
        for (url, sha, filename) in &payloads {
            let archive_path = ctx.download(url, sha.as_deref())?;
            extract_payload(&archive_path, filename, &staging_raw)?;
        }

        Ok(Installed {
            fingerprint: fp,
            display: format!("msvc {resolved_ver} (vs{vs_channel})"),
            options: resolved_options(options, &resolved_ver),
            freshly_extracted: true,
        })
    }
}

/// Walk the `roblabla/msvc-manifest-history` mirror newest-first for a
/// snapshot whose manifest still lists `msvc_version`.
fn find_manifest_in_history(
    commits_url_template: &str,
    raw_url_template: &str,
    vs_channel: &str,
    msvc_version: &str,
    ctx: &InstallCtx,
) -> Result<VsManifest> {
    #[derive(Deserialize)]
    struct Commit {
        sha: String,
    }
    for page in 1..=HISTORY_MAX_PAGES {
        let url = commits_url_template
            .replace("{channel}", vs_channel)
            .replace("{page}", &page.to_string());
        let path = ctx.download(&url, None)?;
        let commits: Vec<Commit> = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
        if commits.is_empty() {
            break;
        }
        for c in &commits {
            let url = raw_url_template.replace("{sha}", &c.sha);
            let path = ctx.download(&url, None)?;
            let m: VsManifest = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
            if m.find_msvc_candidates(HOST, TARGET)
                .iter()
                .any(|(v, _)| v == msvc_version)
            {
                return Ok(m);
            }
        }
    }
    bail!(
        "no snapshot in roblabla/msvc-manifest-history lists MSVC {msvc_version} (vs_channel={vs_channel})"
    )
}

/// Look up each package id in the manifest and gather all its payloads.
fn collect_payloads(
    manifest: &VsManifest,
    package_ids: &[String],
) -> Result<Vec<(String, Option<String>, String)>> {
    let mut out = Vec::new();
    for id in package_ids {
        let pkg = manifest
            .find_package(id)
            .ok_or_else(|| anyhow!("package not found in manifest: {id}"))?;
        for p in &pkg.payloads {
            let filename = p
                .file_name
                .clone()
                .or_else(|| filename_from_url(&p.url))
                .unwrap_or_else(|| "unknown.bin".to_string());
            out.push((p.url.clone(), p.sha256.clone(), filename));
        }
    }
    Ok(out)
}

fn filename_from_url(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    parsed
        .path_segments()
        .and_then(|mut s| s.next_back())
        .map(|s| s.to_string())
}

/// Extract a single MSVC payload (vsix or msi) into `dest`. VSIX = zip with
/// a `Contents/` prefix to strip; MSI = msiexec /a (Windows-only).
fn extract_payload(
    archive: &std::path::Path,
    filename: &str,
    dest: &std::path::Path,
) -> Result<()> {
    let lower = filename.to_lowercase();
    if lower.ends_with(".vsix") || lower.ends_with(".zip") {
        extract::extract_vsix(archive, dest)?;
    } else if lower.ends_with(".msi") {
        extract::extract_msi(archive, dest)?;
    } else {
        tracing::warn!("skipping MSVC payload of unknown type: {filename}");
    }
    Ok(())
}

fn resolved_options(options: &toml::Table, resolved_ver: &str) -> toml::Table {
    let mut o = options.clone();
    // Make sure msvc_version is recorded even if user omitted it.
    o.insert(
        "msvc_version".into(),
        toml::Value::String(resolved_ver.to_string()),
    );
    o
}
#[cfg(test)]
#[path = "msvc_tests.rs"]
mod tests;
