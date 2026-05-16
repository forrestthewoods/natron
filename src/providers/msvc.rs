//! `msvc` provider: downloads the MSVC compiler + CRT packages from a
//! Visual Studio channel manifest. Windows-specific install (uses
//! `msiexec.exe` / VSIX zip extraction).
//!
//! NOTE: this provider produces ONLY the compiler + CRT tree. The Windows
//! SDK is a separate provider (`windows_sdk`). The two are logically
//! independent.

use anyhow::{Context, Result, anyhow, bail};

use super::vs_manifest::{self, VsManifest};
use super::{InstallCtx, Installed, Provider};
use crate::cache::sanitize_fingerprint;
use crate::extract;

pub const ID: &str = "msvc";

const HOST: &str = "x64";
const TARGET: &str = "x64";

/// Archive mirror for old MSVC manifests. This is an implementation detail
/// for pinned `msvc_version`: Microsoft live channel manifests are moving
/// targets and may stop listing older toolsets.
const ARCHIVE_MANIFEST_URL_TEMPLATE: &str =
    "https://raw.githubusercontent.com/roblabla/msvc-manifest-history/release-{channel}/manifest.json";

pub struct MsvcProvider {
    channel_url_template: String,
    archive_manifest_url_template: String,
}

impl MsvcProvider {
    pub fn new() -> Self {
        Self {
            channel_url_template: vs_manifest::DEFAULT_CHANNEL_URL_TEMPLATE.to_string(),
            archive_manifest_url_template: ARCHIVE_MANIFEST_URL_TEMPLATE.to_string(),
        }
    }

    pub fn with_channel_url_template(template: impl Into<String>) -> Self {
        Self {
            channel_url_template: template.into(),
            ..Self::new()
        }
    }

    #[cfg(test)]
    fn with_archive_manifest_url_template(mut self, template: impl Into<String>) -> Self {
        self.archive_manifest_url_template = template.into();
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

        let (manifest, resolved_ver, base_pkg_id) =
            self.resolve_manifest_and_candidate(vs_channel, pinned_version, ctx)?;

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

impl MsvcProvider {
    fn resolve_manifest_and_candidate(
        &self,
        vs_channel: &str,
        pinned_version: Option<&str>,
        ctx: &InstallCtx,
    ) -> Result<(VsManifest, String, String)> {
        let live = self.fetch_live_manifest(vs_channel, ctx);

        let Some(requested) = pinned_version else {
            let manifest = live?;
            let (resolved_ver, base_pkg_id) = select_latest_candidate(&manifest)?;
            return Ok((manifest, resolved_ver, base_pkg_id));
        };

        let mut notes = Vec::new();
        match live {
            Ok(manifest) => {
                if let Some((resolved_ver, base_pkg_id)) =
                    select_pinned_candidate(&manifest, requested)
                {
                    return Ok((manifest, resolved_ver, base_pkg_id));
                }
                notes.push(format!(
                    "Microsoft live manifest did not contain {requested}; available versions: {}",
                    format_available_versions(&manifest)
                ));
            }
            Err(err) => notes.push(format!("Microsoft live manifest failed: {err:#}")),
        }

        match self.fetch_archive_manifest(vs_channel, ctx) {
            Ok(manifest) => {
                if let Some((resolved_ver, base_pkg_id)) =
                    select_pinned_candidate(&manifest, requested)
                {
                    return Ok((manifest, resolved_ver, base_pkg_id));
                }
                notes.push(format!(
                    "archived manifest did not contain {requested}; available versions: {}",
                    format_available_versions(&manifest)
                ));
            }
            Err(err) => notes.push(format!("archived manifest failed: {err:#}")),
        }

        bail!(
            "could not resolve pinned msvc_version='{requested}' for vs_channel='{vs_channel}'; {}",
            notes.join("; ")
        )
    }

    fn fetch_live_manifest(&self, vs_channel: &str, ctx: &InstallCtx) -> Result<VsManifest> {
        vs_manifest::fetch_vs_manifest(&self.channel_url_template, vs_channel, ctx)
            .with_context(|| format!("resolving Microsoft live VS manifest for channel {vs_channel}"))
    }

    fn fetch_archive_manifest(&self, vs_channel: &str, ctx: &InstallCtx) -> Result<VsManifest> {
        let url = self
            .archive_manifest_url_template
            .replace("{channel}", vs_channel);
        let path = ctx
            .download(&url, None)
            .with_context(|| format!("fetching archived VS manifest from {url}"))?;
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading archived VS manifest {}", path.display()))?;
        serde_json::from_str(&text)
            .with_context(|| format!("parsing archived VS manifest {}", path.display()))
    }
}

fn select_latest_candidate(manifest: &VsManifest) -> Result<(String, String)> {
    let candidates = manifest.find_msvc_candidates(HOST, TARGET);
    if candidates.is_empty() {
        bail!("no MSVC packages found in VS manifest for host={HOST} target={TARGET}");
    }
    Ok(candidates[0].clone())
}

fn select_pinned_candidate(manifest: &VsManifest, requested: &str) -> Option<(String, String)> {
    manifest
        .find_msvc_candidates(HOST, TARGET)
        .into_iter()
        .find(|(version, _)| version == requested)
}

fn format_available_versions(manifest: &VsManifest) -> String {
    let versions: Vec<_> = manifest
        .find_msvc_candidates(HOST, TARGET)
        .into_iter()
        .map(|(version, _)| version)
        .collect();
    if versions.is_empty() {
        "<none>".to_string()
    } else {
        versions.join(", ")
    }
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
