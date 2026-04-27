//! `msvc` provider: downloads the MSVC compiler + CRT packages from a
//! Visual Studio channel manifest. Windows-specific install (uses
//! `msiexec.exe` / VSIX zip extraction).
//!
//! NOTE: this provider produces ONLY the compiler + CRT tree. The Windows
//! SDK is a separate provider (`windows_sdk`). The two are logically
//! independent.

use anyhow::{Result, anyhow};

use super::vs_manifest::{self, VsManifest};
use super::{InstallCtx, Installed, Provider};
use crate::cache::sanitize_fingerprint;
use crate::extract;

pub const ID: &str = "msvc";

const HOST: &str = "x64";
const TARGET: &str = "x64";

pub struct MsvcProvider {
    channel_url_template: String,
}

impl MsvcProvider {
    pub fn new() -> Self {
        Self {
            channel_url_template: vs_manifest::DEFAULT_CHANNEL_URL_TEMPLATE.to_string(),
        }
    }

    pub fn with_channel_url_template(template: impl Into<String>) -> Self {
        Self {
            channel_url_template: template.into(),
        }
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

        // Need the manifest.
        let manifest = vs_manifest::fetch_vs_manifest(
            &self.channel_url_template,
            vs_channel,
            ctx,
        )?;
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
        let target_lower = TARGET.to_lowercase();
        let companion_ids = vec![
            base_pkg_id,
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
mod tests {
    use super::*;
    use crate::cache::Cache;
    use tempfile::TempDir;

    #[test]
    fn msvc_provider_id() {
        assert_eq!(MsvcProvider::new().id(), "msvc");
    }

    #[test]
    fn msvc_provider_required_fields() {
        let tmp = TempDir::new().unwrap();
        let cache = Cache::at(tmp.path().join("c"));
        cache.ensure_layout().unwrap();
        let mut ctx = InstallCtx::new(cache);
        let opts = toml::Table::new();
        let err = MsvcProvider::new().install(&opts, &mut ctx).unwrap_err();
        assert!(err.to_string().contains("vs_channel"));
    }

    #[test]
    fn msvc_provider_pinned_version_fast_path_no_network() {
        // Pre-plant an install dir matching the deterministic fingerprint.
        // The provider should short-circuit without ever calling fetch.
        let tmp = TempDir::new().unwrap();
        let cache = Cache::at(tmp.path().join("c"));
        cache.ensure_layout().unwrap();
        let fp = sanitize_fingerprint("msvc-14.50.18.0-18");
        let install_dir = cache.install_dir(&fp);
        std::fs::create_dir_all(install_dir.join("tree")).unwrap();
        let md = crate::cache::InstallMetadata::new(
            "msvc",
            fp.clone(),
            "msvc 14.50.18.0 (vs18)",
            toml::Table::new(),
        );
        md.write(&cache.install_metadata_path(&fp)).unwrap();

        let mut ctx = InstallCtx::new(cache);
        let mut opts = toml::Table::new();
        opts.insert("vs_channel".into(), toml::Value::String("18".into()));
        opts.insert(
            "msvc_version".into(),
            toml::Value::String("14.50.18.0".into()),
        );

        // Use a deliberately invalid template — if the provider tries to
        // hit it, we'll see the failure.
        let provider =
            MsvcProvider::with_channel_url_template("file:///never/exists/{channel}");
        let installed = provider.install(&opts, &mut ctx).unwrap();
        assert!(!installed.freshly_extracted);
        assert_eq!(installed.fingerprint, fp);
    }
}
