//! `windows_sdk` provider: install the Universal CRT + Windows SDK headers
//! and libs from one Microsoft VS build snapshot.
//!
//! Manifest source + reproducibility model are identical to `msvc` (both
//! pin a `build_version` → one mirror commit → one immutable manifest).
//! `windows_sdk` selects the SDK component meta-package and follows its
//! declared dependencies to the MSI-bearing packages.

use anyhow::{Context, Result, anyhow, bail};
use std::path::Path;
use xxhash_rust::xxh3::xxh3_64;

use super::vs_manifest::{self, MirrorUrls, Package, VsManifest};
use super::{InstallCtx, Installed, Provider};
use crate::cache::sanitize_fingerprint;
use crate::extract;
use crate::fs_util;

pub const ID: &str = "windows_sdk";

/// Filename prefixes of the SDK MSIs we actually need for C/C++ dev. The
/// SDK ships many more (debuggers, ARM target libs, telemetry shims) we
/// skip.
const ESSENTIAL_MSIS: &[&str] = &[
    "Universal CRT Headers Libraries and Sources",
    "Windows SDK Desktop Headers x86", // includes d3d10misc.h etc.
    "Windows SDK Desktop Libs x64",
    "Windows SDK OnecoreUap Headers",
    "Windows SDK for Windows Store Apps Headers",
    "Windows SDK for Windows Store Apps Libs",
    "Windows SDK for Windows Store Apps Tools",
];

#[derive(Debug)]
struct Options {
    build_version: String,
    sdk_version: Option<String>,
}

impl Options {
    fn parse(options: &toml::Table) -> Result<Self> {
        let build_version = options
            .get("build_version")
            .ok_or_else(|| anyhow!("`windows_sdk` provider requires options.build_version"))?
            .as_str()
            .ok_or_else(|| anyhow!("`windows_sdk` option 'build_version' must be a string"))?
            .to_string();
        let major = vs_manifest::build_version_major(&build_version)
            .map_err(|e| anyhow!("`windows_sdk` provider: {e}"))?;
        vs_manifest::VsVersion::from_channel(major)
            .map_err(|e| anyhow!("`windows_sdk` provider: {e}"))?;
        let sdk_version = match options.get("sdk_version") {
            None => None,
            Some(v) => Some(
                v.as_str()
                    .ok_or_else(|| anyhow!("`windows_sdk` option 'sdk_version' must be a string"))?
                    .to_string(),
            ),
        };
        Ok(Self {
            build_version,
            sdk_version,
        })
    }
}

pub struct WindowsSdkProvider {
    urls: MirrorUrls,
}

impl WindowsSdkProvider {
    pub fn new() -> Self {
        Self {
            urls: MirrorUrls::default(),
        }
    }

    pub fn with_urls(urls: MirrorUrls) -> Self {
        Self { urls }
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

    fn install(&self, options: &toml::Table, ctx: &mut InstallCtx) -> Result<Installed> {
        let opts = Options::parse(options)?;

        // Pinned-sdk_version cache fast path: no need to fetch anything if
        // the install tree's already on disk.
        if let Some(ver) = &opts.sdk_version {
            let fp = sdk_fingerprint(&opts.build_version, ver);
            if ctx.cache().install_present(&fp) {
                return Ok(Installed {
                    fingerprint: fp,
                    display: display_for(&opts.build_version, ver),
                    options: resolved_options(&opts, ver),
                    freshly_extracted: false,
                });
            }
        }

        let entry = vs_manifest::resolve_build_version(&self.urls, &opts.build_version, ctx)?;
        let manifest = vs_manifest::fetch_manifest_at(&self.urls.raw_base, &entry.commit.sha, ctx)?;
        let candidates = find_sdk_candidates(&manifest);
        if candidates.is_empty() {
            bail!("no Windows SDK component packages in snapshot {}", opts.build_version);
        }
        let (resolved_ver, sdk_pkg_id) = match &opts.sdk_version {
            Some(req) => candidates
                .iter()
                .find(|(v, _)| v == req)
                .ok_or_else(|| {
                    let avail: Vec<_> = candidates.iter().map(|(v, _)| v.as_str()).collect();
                    anyhow!(
                        "sdk_version='{req}' not in snapshot {}; available: {avail:?}",
                        opts.build_version,
                    )
                })?
                .clone(),
            None => candidates[0].clone(),
        };

        let fp = sdk_fingerprint(&opts.build_version, &resolved_ver);
        if ctx.cache().install_present(&fp) {
            return Ok(Installed {
                fingerprint: fp,
                display: display_for(&opts.build_version, &resolved_ver),
                options: resolved_options(&opts, &resolved_ver),
                freshly_extracted: false,
            });
        }

        let component = find_exact(&manifest, &sdk_pkg_id)
            .ok_or_else(|| anyhow!("SDK component package {sdk_pkg_id} not in manifest"))?;
        let dep_ids: Vec<String> = component.dependencies.keys().cloned().collect();

        // Stage all CABs + MSIs in ONE flat directory so msiexec /a can
        // resolve sibling CAB references by basename.
        let staging_raw = ctx.staging_dir()?.to_path_buf();
        let payloads_dir = staging_raw.join("__sdk_payloads");
        let extract_dir = staging_raw.join("__sdk_extract");
        std::fs::create_dir_all(&payloads_dir)
            .with_context(|| format!("creating {}", payloads_dir.display()))?;
        std::fs::create_dir_all(&extract_dir)
            .with_context(|| format!("creating {}", extract_dir.display()))?;

        let mut essential_msis: Vec<std::path::PathBuf> = Vec::new();
        for dep_id in &dep_ids {
            let Some(pkg) = find_exact(&manifest, dep_id) else {
                tracing::warn!("SDK dep package {dep_id} not in manifest; skipping");
                continue;
            };
            for p in &pkg.payloads {
                let filename = payload_filename(p);
                let basename = strip_installer_prefix(&filename);
                let downloaded = ctx
                    .download(&p.url, p.sha256.as_deref())
                    .with_context(|| format!("downloading SDK payload {filename} for {dep_id}"))?;
                let dest = payloads_dir.join(&basename);
                if !dest.exists() {
                    let r = std::fs::hard_link(&downloaded, &dest)
                        .or_else(|_| std::fs::copy(&downloaded, &dest).map(|_| ()));
                    r.with_context(|| {
                        format!(
                            "staging SDK payload {} -> {}",
                            downloaded.display(),
                            dest.display()
                        )
                    })?;
                }
                if filename.to_lowercase().ends_with(".msi") && is_essential_msi(&filename) {
                    essential_msis.push(dest);
                }
            }
        }

        tracing::info!("extracting {} essential SDK MSIs", essential_msis.len());
        for msi in &essential_msis {
            extract::extract_msi(msi, &extract_dir)
                .with_context(|| format!("extracting MSI {}", msi.display()))?;
        }
        flatten_windows_kits_into(&extract_dir, &staging_raw)
            .context("flattening Windows Kits/10")?;
        let _ = fs_util::remove_dir_all_writable(&payloads_dir);
        let _ = fs_util::remove_dir_all_writable(&extract_dir);

        Ok(Installed {
            fingerprint: fp,
            display: display_for(&opts.build_version, &resolved_ver),
            options: resolved_options(&opts, &resolved_ver),
            freshly_extracted: true,
        })
    }
}

// ---- SDK package detection -------------------------------------------------

/// Find every Windows SDK component meta-package in the manifest. Returns
/// `(sdk_version, package_id)` sorted descending by numeric version.
fn find_sdk_candidates(manifest: &VsManifest) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for pkg in &manifest.packages {
        for prefix in [
            "Microsoft.VisualStudio.Component.Windows10SDK.",
            "Microsoft.VisualStudio.Component.Windows11SDK.",
        ] {
            if let Some(rest) = pkg.id.strip_prefix(prefix) {
                if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit() || c == '.') {
                    out.push((rest.to_string(), pkg.id.clone()));
                }
            }
        }
    }
    out.sort_by(|a, b| numeric_key(&b.0).cmp(&numeric_key(&a.0)));
    out
}

fn numeric_key(v: &str) -> Vec<u64> {
    v.split('.').map(|s| s.parse::<u64>().unwrap_or(0)).collect()
}

// ---- helpers ---------------------------------------------------------------

fn find_exact<'a>(manifest: &'a VsManifest, id: &str) -> Option<&'a Package> {
    let lower = id.to_lowercase();
    let matches: Vec<&Package> = manifest
        .packages
        .iter()
        .filter(|p| p.id.to_lowercase() == lower)
        .collect();
    matches
        .iter()
        .copied()
        .find(|p| p.language.as_deref() == Some("en-US"))
        .or_else(|| matches.iter().copied().find(|p| p.language.is_none()))
        .or_else(|| matches.first().copied())
}

fn payload_filename(p: &vs_manifest::Payload) -> String {
    if let Some(name) = &p.file_name {
        return name.clone();
    }
    if let Ok(parsed) = url::Url::parse(&p.url) {
        if let Some(seg) = parsed.path_segments().and_then(|mut s| s.next_back()) {
            if !seg.is_empty() {
                return seg.to_string();
            }
        }
    }
    "unknown.bin".to_string()
}

fn is_essential_msi(filename: &str) -> bool {
    let base = strip_installer_prefix(filename);
    ESSENTIAL_MSIS.iter().any(|prefix| base.starts_with(prefix))
}

/// VS manifest filenames sometimes embed install-subdirectory components
/// (`Installers\foo.msi`, `Redistributable\10.1.0.0\UAPSDKAddOn-x86.msi`).
/// Flatten to just the basename so msiexec sees CABs as siblings.
fn strip_installer_prefix(filename: &str) -> String {
    let normalized = filename.replace('\\', "/");
    normalized
        .rsplit('/')
        .next()
        .unwrap_or(&normalized)
        .to_string()
}

/// MSIs extract under `<extract>/Windows Kits/10/*`. Move each child up to
/// `<dst>/` so the staged tree starts at Include/Lib/etc. Merges
/// when names collide; leaves stray `.msi` siblings in the scratch dir
/// (where they'll be cleaned up).
fn flatten_windows_kits_into(src: &Path, dst: &Path) -> Result<()> {
    let kits = src.join("Windows Kits").join("10");
    if !kits.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(&kits)? {
        let entry = entry?;
        let target = dst.join(entry.file_name());
        if target.exists() {
            merge_into(&entry.path(), &target)?;
            fs_util::remove_dir_all_writable(&entry.path()).ok();
        } else {
            std::fs::rename(entry.path(), target)?;
        }
    }
    Ok(())
}

fn merge_into(src: &Path, dst: &Path) -> Result<()> {
    if !src.is_dir() {
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

// ---- fingerprint + display -------------------------------------------------

fn sdk_fingerprint(build_version: &str, sdk_version: &str) -> String {
    let key = format!("{build_version}\n{sdk_version}");
    let hash = xxh3_64(key.as_bytes());
    sanitize_fingerprint(&format!("windows_sdk-{build_version}-{sdk_version}-{hash:016x}"))
}

fn display_for(build_version: &str, sdk_version: &str) -> String {
    format!("windows_sdk {sdk_version} (build {build_version})")
}

fn resolved_options(opts: &Options, resolved_sdk: &str) -> toml::Table {
    let mut o = toml::Table::new();
    o.insert(
        "build_version".into(),
        toml::Value::String(opts.build_version.clone()),
    );
    o.insert(
        "sdk_version".into(),
        toml::Value::String(resolved_sdk.to_string()),
    );
    o
}

#[cfg(test)]
#[path = "windows_sdk_tests.rs"]
mod tests;
