//! `windows_sdk` provider: extracts the Universal CRT + Windows SDK headers
//! and libs from a Visual Studio channel manifest. Logically separate from
//! `msvc` (compiler/CRT). Windows-specific install (uses `msiexec.exe`).
//!
//! Selects only the "essential" MSIs needed for C/C++ development. The SDK
//! component package is a meta-package whose `dependencies` map names the
//! actual MSI-bearing packages.

use anyhow::{Result, anyhow};
use std::path::Path;

use super::vs_manifest::{self, VsManifest};
use super::{InstallCtx, Installed, Provider};
use crate::cache::sanitize_fingerprint;
use crate::extract;
use crate::fs_util;

pub const ID: &str = "windows_sdk";

const TARGET: &str = "x64";

/// Filename prefixes for the SDK MSIs we actually want extracted. Matches
/// the list in Anubis's `install_windows_sdk` (see install_toolchains.rs:1641).
const ESSENTIAL_MSIS: &[&str] = &[
    "Universal CRT Headers Libraries and Sources",
    "Windows SDK Desktop Headers x86", // contains extras like d3d10misc.h
    "Windows SDK Desktop Libs x64",
    "Windows SDK OnecoreUap Headers",
    "Windows SDK for Windows Store Apps Headers",
    "Windows SDK for Windows Store Apps Libs",
    "Windows SDK for Windows Store Apps Tools",
];

pub struct WindowsSdkProvider {
    channel_url_template: String,
}

impl WindowsSdkProvider {
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

impl Default for WindowsSdkProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for WindowsSdkProvider {
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
                anyhow!("`windows_sdk` provider requires options.vs_channel")
            })?;
        let pinned = options.get("sdk_version").and_then(|v| v.as_str());

        if let Some(ver) = pinned {
            let fp = sanitize_fingerprint(&format!("windows_sdk-{ver}-{vs_channel}"));
            if ctx.cache().install_present(&fp) {
                return Ok(Installed {
                    fingerprint: fp,
                    display: format!("windows_sdk {ver} (vs{vs_channel})"),
                    options: options.clone(),
                    freshly_extracted: false,
                });
            }
        }

        let manifest = vs_manifest::fetch_vs_manifest(
            &self.channel_url_template,
            vs_channel,
            ctx,
        )?;
        let candidates = manifest.find_sdk_candidates();
        if candidates.is_empty() {
            anyhow::bail!("no Windows SDK component packages found in VS manifest");
        }
        let (resolved_ver, sdk_pkg_id) = match pinned {
            Some(req) => candidates
                .iter()
                .find(|(v, _)| v == req)
                .ok_or_else(|| {
                    let avail: Vec<_> =
                        candidates.iter().map(|(v, _)| v.as_str()).collect();
                    anyhow!(
                        "sdk_version='{req}' not in manifest; available: {:?}",
                        avail
                    )
                })?
                .clone(),
            None => candidates[0].clone(),
        };

        let fp = sanitize_fingerprint(&format!("windows_sdk-{resolved_ver}-{vs_channel}"));
        if ctx.cache().install_present(&fp) {
            return Ok(Installed {
                fingerprint: fp,
                display: format!("windows_sdk {resolved_ver} (vs{vs_channel})"),
                options: resolved_options(options, &resolved_ver),
                freshly_extracted: false,
            });
        }

        // Find the SDK component meta-package; its `dependencies` map names
        // the real MSI-bearing packages.
        let component = manifest.find_package(&sdk_pkg_id).ok_or_else(|| {
            anyhow!("SDK component package {sdk_pkg_id} not in manifest")
        })?;
        let dep_ids: Vec<String> = component.dependencies.keys().cloned().collect();

        // Stage extraction into a temp area, then move "Windows Kits/10/*"
        // contents up to the staging root.
        let staging_raw = ctx.staging_dir()?;
        let temp_extract = staging_raw.join("__msi_extract");
        std::fs::create_dir_all(&temp_extract)?;

        // Download every payload (CABs + MSIs) and extract only the
        // essential MSIs. CABs are sibling files referenced by MSIs during
        // extraction; they must be in the same dir as the MSI when
        // msiexec runs.
        let downloads_dir = ctx.cache().downloads.clone();
        for dep_id in &dep_ids {
            let Some(pkg) = manifest.find_package(dep_id) else {
                tracing::warn!("SDK dep package {dep_id} not in manifest; skipping");
                continue;
            };
            // Track which files this package contributed; among them the
            // .msi files that match an essential prefix must be extracted.
            let mut staged_msis = Vec::new();
            for p in &pkg.payloads {
                let filename = p
                    .file_name
                    .clone()
                    .or_else(|| filename_from_url(&p.url))
                    .unwrap_or_else(|| "unknown.bin".to_string());
                let downloaded = ctx.download(&p.url, p.sha256.as_deref())?;
                // Stage CAB/MSI files together in a per-package extraction
                // dir so msiexec can find the CABs as siblings of the MSI.
                let pkg_tmp = temp_extract.join(sanitize_fingerprint(dep_id));
                std::fs::create_dir_all(&pkg_tmp)?;
                let dest = pkg_tmp.join(strip_installer_prefix(&filename));
                if !dest.exists() {
                    // hardlink from downloads/ if same volume; else copy.
                    if std::fs::hard_link(&downloaded, &dest).is_err() {
                        std::fs::copy(&downloaded, &dest)?;
                    }
                }
                let _ = downloads_dir; // silence unused on some cfgs
                if filename.to_lowercase().ends_with(".msi") && is_essential_msi(&filename) {
                    staged_msis.push(dest);
                }
            }
            for msi in staged_msis {
                tracing::debug!("extracting SDK MSI {}", msi.display());
                extract::extract_msi(&msi, &staging_raw)?;
            }
        }

        // After extraction, contents live under `staging_raw/Windows Kits/10/*`.
        // Flatten by moving each child of Windows Kits/10/ up to staging_raw/
        // and removing the Windows Kits skeleton.
        flatten_windows_kits(&staging_raw)?;

        // Clean up the per-package staging dirs.
        let _ = fs_util::remove_dir_all_writable(&temp_extract);

        Ok(Installed {
            fingerprint: fp,
            display: format!("windows_sdk {resolved_ver} (vs{vs_channel})"),
            options: resolved_options(options, &resolved_ver),
            freshly_extracted: true,
        })
    }
}

fn is_essential_msi(filename: &str) -> bool {
    let base = strip_installer_prefix(filename);
    let _ = TARGET;
    ESSENTIAL_MSIS.iter().any(|prefix| base.starts_with(prefix))
}

fn strip_installer_prefix(filename: &str) -> String {
    // VS manifest sometimes prefixes filenames with "Installers\".
    filename
        .strip_prefix("Installers\\")
        .or_else(|| filename.strip_prefix("Installers/"))
        .unwrap_or(filename)
        .to_string()
}

fn filename_from_url(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    parsed
        .path_segments()
        .and_then(|mut s| s.next_back())
        .map(|s| s.to_string())
}

/// MSIs extract to `<staging>/Windows Kits/10/*`. Move every direct child
/// of `Windows Kits/10/` up to `<staging>/` and remove the empty skeleton.
fn flatten_windows_kits(staging: &Path) -> Result<()> {
    let kits = staging.join("Windows Kits").join("10");
    if !kits.exists() {
        // Some hand-faked test inputs may not have this layout; that's
        // fine — leave staging as-is.
        return Ok(());
    }
    for entry in std::fs::read_dir(&kits)? {
        let entry = entry?;
        let dest = staging.join(entry.file_name());
        // If something already exists at dest (rare), merge by moving
        // children individually.
        if dest.exists() {
            merge_into(&entry.path(), &dest)?;
            fs_util::remove_dir_all_writable(&entry.path()).ok();
        } else {
            std::fs::rename(entry.path(), dest)?;
        }
    }
    fs_util::remove_dir_all_writable(&staging.join("Windows Kits")).ok();
    Ok(())
}

fn merge_into(src: &Path, dst: &Path) -> Result<()> {
    if !src.is_dir() {
        // Replace file at dst (rare).
        let _ = std::fs::remove_file(dst);
        std::fs::rename(src, dst)?;
        return Ok(());
    }
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let target = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            merge_into(&entry.path(), &target)?;
        } else if !target.exists() {
            std::fs::rename(entry.path(), target)?;
        }
    }
    Ok(())
}

fn resolved_options(options: &toml::Table, resolved_ver: &str) -> toml::Table {
    let mut o = options.clone();
    o.insert(
        "sdk_version".into(),
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
    fn windows_sdk_provider_id() {
        assert_eq!(WindowsSdkProvider::new().id(), "windows_sdk");
    }

    #[test]
    fn windows_sdk_provider_required_fields() {
        let tmp = TempDir::new().unwrap();
        let cache = Cache::at(tmp.path().join("c"));
        cache.ensure_layout().unwrap();
        let mut ctx = InstallCtx::new(cache);
        let opts = toml::Table::new();
        let err = WindowsSdkProvider::new().install(&opts, &mut ctx).unwrap_err();
        assert!(err.to_string().contains("vs_channel"));
    }

    #[test]
    fn windows_sdk_provider_pinned_fast_path_no_network() {
        let tmp = TempDir::new().unwrap();
        let cache = Cache::at(tmp.path().join("c"));
        cache.ensure_layout().unwrap();
        let fp = sanitize_fingerprint("windows_sdk-26100-18");
        let install_dir = cache.install_dir(&fp);
        std::fs::create_dir_all(install_dir.join("tree")).unwrap();
        let md = crate::cache::InstallMetadata::new(
            "windows_sdk",
            fp.clone(),
            "windows_sdk 26100 (vs18)",
            toml::Table::new(),
        );
        md.write(&cache.install_metadata_path(&fp)).unwrap();

        let mut ctx = InstallCtx::new(cache);
        let mut opts = toml::Table::new();
        opts.insert("vs_channel".into(), toml::Value::String("18".into()));
        opts.insert("sdk_version".into(), toml::Value::String("26100".into()));

        let provider = WindowsSdkProvider::with_channel_url_template(
            "file:///never/exists/{channel}",
        );
        let installed = provider.install(&opts, &mut ctx).unwrap();
        assert!(!installed.freshly_extracted);
    }

    #[test]
    fn essential_msi_match_works() {
        assert!(is_essential_msi(
            "Universal CRT Headers Libraries and Sources-x86_en-us.msi"
        ));
        assert!(is_essential_msi("Windows SDK Desktop Libs x64-x86_en-us.msi"));
        assert!(is_essential_msi(
            "Installers\\Windows SDK OnecoreUap Headers-x86_en-us.msi"
        ));
        assert!(!is_essential_msi("Random Other.msi"));
    }

    #[test]
    fn flatten_windows_kits_moves_children_up() {
        let tmp = TempDir::new().unwrap();
        let staging = tmp.path().join("staging");
        let kits = staging.join("Windows Kits").join("10");
        std::fs::create_dir_all(kits.join("Include").join("10.0.0")).unwrap();
        std::fs::write(
            kits.join("Include").join("10.0.0").join("foo.h"),
            b"#define FOO 1",
        )
        .unwrap();
        std::fs::create_dir_all(kits.join("Lib").join("10.0.0").join("um")).unwrap();
        std::fs::write(
            kits.join("Lib").join("10.0.0").join("um").join("foo.lib"),
            b"libdata",
        )
        .unwrap();

        flatten_windows_kits(&staging).unwrap();

        assert!(staging.join("Include").join("10.0.0").join("foo.h").is_file());
        assert!(
            staging
                .join("Lib")
                .join("10.0.0")
                .join("um")
                .join("foo.lib")
                .is_file()
        );
        assert!(!staging.join("Windows Kits").exists());
    }
}
