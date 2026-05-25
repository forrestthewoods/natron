//! `windows_sdk` provider: install the Universal CRT + Windows SDK headers
//! and libs from a Windows SDK version (e.g. `26100`).
//!
//! SDK packages are independently versioned from MSVC and immutable per
//! `sdk_version`: Microsoft uploads each SDK's MSIs once to their CDN
//! with stable URLs, and multiple Microsoft VS installer snapshots all
//! reference the same payloads. So pinning `sdk_version` alone is fully
//! reproducible — we just need any snapshot whose manifest lists it.
//!
//! Resolution: scan the newest snapshot of each VS series in
//! vs2026 → vs2022 → vs2019 order, return on first hit.

use anyhow::{Context, Result, anyhow, bail};
use std::collections::BTreeSet;
use std::path::Path;
use xxhash_rust::xxh3::xxh3_64;

use super::vs_manifest::{
    self, BuildIndexEntry, MirrorUrls, Package, VsManifest, VsVersion,
};
use super::{InstallCtx, Installed, Provider};
use crate::cache::sanitize_fingerprint;
use crate::download;
use crate::extract;
use crate::fs_util;

pub const ID: &str = "windows_sdk";

/// MSI filename prefixes that constitute `base_install = "default"`.
/// The SDK ships many more MSIs (debuggers, ARM target libs, driver headers,
/// signing tools, etc.) — the user opts into those via `extras`.
pub const DEFAULT_ESSENTIAL_MSIS: &[&str] = &[
    "Universal CRT Headers Libraries and Sources",
    "Windows SDK Desktop Headers x86", // contains windows.h, kernel32.h, etc.
    "Windows SDK Desktop Libs x64",
    "Windows SDK OnecoreUap Headers",
    "Windows SDK for Windows Store Apps Headers",
    "Windows SDK for Windows Store Apps Libs",
    "Windows SDK for Windows Store Apps Tools",
];

// ---- option types ----------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BaseInstall {
    None,
    Default,
    Full,
}

impl BaseInstall {
    fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Default => "default",
            Self::Full => "full",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "none" => Ok(Self::None),
            "default" => Ok(Self::Default),
            "full" => Ok(Self::Full),
            other => bail!("invalid base_install '{other}'; valid: none, default, full"),
        }
    }
}

#[derive(Debug)]
struct Options {
    sdk_version: String,
    base: BaseInstall,
    extras: Vec<String>, // MSI filename prefixes
}

impl Options {
    fn parse(options: &toml::Table) -> Result<Self> {
        let sdk_version = required_str(options, "sdk_version")?.to_string();
        if !is_numeric_sdk_version(&sdk_version) {
            bail!(
                "`windows_sdk`: sdk_version '{sdk_version}' must be a numeric build number (e.g. 26100)"
            );
        }
        let base = match optional_str(options, "base_install")? {
            Some(v) => BaseInstall::parse(v)?,
            None => BaseInstall::Default,
        };
        let extras = optional_string_list(options, "extras")?;

        if base == BaseInstall::None && extras.is_empty() {
            bail!("`windows_sdk`: base_install='none' with empty extras would install nothing");
        }
        if base == BaseInstall::Full && !extras.is_empty() {
            bail!(
                "`windows_sdk`: base_install='full' already selects every MSI; remove extras"
            );
        }
        Ok(Self {
            sdk_version,
            base,
            extras,
        })
    }
}

fn is_numeric_sdk_version(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit() || c == '.')
}

// ---- provider --------------------------------------------------------------

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
        let fp = fingerprint(&opts);

        if ctx.cache().install_present(&fp) {
            return Ok(Installed {
                fingerprint: fp,
                display: display_for(&opts),
                options: resolved_options(&opts),
                freshly_extracted: false,
            });
        }

        let resolved = resolve_sdk_version(&self.urls, &opts.sdk_version, ctx)?;
        let component = lookup_exact(&resolved.manifest, &resolved.sdk_pkg_id)
            .ok_or_else(|| anyhow!("SDK component {} not in manifest", resolved.sdk_pkg_id))?;
        let dep_ids: Vec<String> = component.dependencies.keys().cloned().collect();

        // Validate extras BEFORE downloading anything — typo'd prefixes
        // should fail loud, matching msvc's behavior on a zero-match
        // glob. Otherwise the user silently gets a default install.
        check_extras_match(&resolved.manifest, &dep_ids, &opts)?;

        // Stage all CABs + MSIs in ONE flat directory so the pure-Rust
        // extractor can resolve external sibling CABs (referenced from
        // the MSI's `Media` table) by basename.
        let staging_raw = ctx.staging_dir()?.to_path_buf();
        let payloads_dir = staging_raw.join("__sdk_payloads");
        let extract_dir = staging_raw.join("__sdk_extract");
        std::fs::create_dir_all(&payloads_dir)
            .with_context(|| format!("creating {}", payloads_dir.display()))?;
        std::fs::create_dir_all(&extract_dir)
            .with_context(|| format!("creating {}", extract_dir.display()))?;

        let mut msis_to_extract: Vec<std::path::PathBuf> = Vec::new();
        let mut downloaded_count = 0usize;
        let mut cached_count = 0usize;
        for dep_id in &dep_ids {
            let Some(pkg) = lookup_exact(&resolved.manifest, dep_id) else {
                tracing::warn!("SDK dep package {dep_id} not in manifest; skipping");
                continue;
            };

            // Decide once per dep whether we'll extract any of its MSIs.
            // If not, skip the whole dep — don't download its CABs either,
            // since CABs are only useful as siblings of an MSI we're about
            // to extract. For the typical `default` install (7 essentials),
            // this skips ~85% of SDK component deps.
            let install_this_dep = pkg.payloads.iter().any(|p| {
                let filename = payload_filename(p);
                if !filename.to_lowercase().ends_with(".msi") {
                    return false;
                }
                msi_should_extract(&filename, &opts)
            });
            if !install_this_dep {
                continue;
            }

            for p in &pkg.payloads {
                let filename = payload_filename(p);
                let basename = strip_installer_prefix(&filename);
                let (downloaded, source) = ctx
                    .download_with_outcome(&p.url, p.sha256.as_deref())
                    .with_context(|| format!("downloading SDK payload {filename} for {dep_id}"))?;
                match source {
                    download::FetchSource::Cached => cached_count += 1,
                    download::FetchSource::Downloaded => downloaded_count += 1,
                }
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
                if filename.to_lowercase().ends_with(".msi")
                    && msi_should_extract(&filename, &opts)
                {
                    msis_to_extract.push(dest);
                }
            }
        }

        tracing::info!(
            "windows_sdk {}: {downloaded_count} payloads downloaded, \
             {cached_count} already cached",
            opts.sdk_version,
        );
        tracing::info!("extracting {} SDK MSIs", msis_to_extract.len());
        for msi in &msis_to_extract {
            extract::extract_msi(msi, &extract_dir)
                .with_context(|| format!("extracting MSI {}", msi.display()))?;
        }
        flatten_windows_kits_into(&extract_dir, &staging_raw)
            .context("flattening Windows Kits/10")?;
        let _ = fs_util::remove_dir_all_writable(&payloads_dir);
        let _ = fs_util::remove_dir_all_writable(&extract_dir);

        Ok(Installed {
            fingerprint: fp,
            display: display_for(&opts),
            options: resolved_options(&opts),
            freshly_extracted: true,
        })
    }
}

// ---- SDK resolution (pub helpers shared with the CLI) ----------------------

/// One resolved SDK: which snapshot we found it in, the snapshot's manifest,
/// and the SDK component meta-package id within that manifest.
#[derive(Debug)]
pub struct ResolvedSdk {
    pub entry: BuildIndexEntry,
    pub manifest: VsManifest,
    pub sdk_pkg_id: String,
}

/// Walk the newest snapshot of each VS series in vs2026 → vs2022 → vs2019
/// order; return the first snapshot whose manifest lists the requested
/// `sdk_version`. Errors with the union of SDKs available across all three
/// snapshots if nothing matches.
pub fn resolve_sdk_version(
    urls: &MirrorUrls,
    sdk_version: &str,
    ctx: &InstallCtx,
) -> Result<ResolvedSdk> {
    let mut available: BTreeSet<String> = BTreeSet::new();
    let mut last_err: Option<anyhow::Error> = None;
    for vs in VsVersion::all().iter().rev() {
        let entry = match newest_entry_for(urls, *vs, ctx) {
            Ok(e) => e,
            Err(err) => {
                last_err = Some(err);
                continue;
            }
        };
        let manifest = match vs_manifest::fetch_manifest_at(&urls.raw_base, &entry.commit.sha, ctx) {
            Ok(m) => m,
            Err(err) => {
                last_err = Some(err);
                continue;
            }
        };
        let cands = find_sdk_candidates(&manifest);
        for (v, _id) in &cands {
            available.insert(v.clone());
        }
        if let Some((_, sdk_pkg_id)) = cands.iter().find(|(v, _)| v == sdk_version) {
            return Ok(ResolvedSdk {
                entry,
                sdk_pkg_id: sdk_pkg_id.clone(),
                manifest,
            });
        }
    }
    if available.is_empty() {
        match last_err {
            Some(err) => bail!("could not enumerate SDKs from any VS series: {err:#}"),
            None => bail!("could not enumerate SDKs from any VS series"),
        }
    }
    let mut sorted: Vec<String> = available.into_iter().collect();
    sorted.sort_by_key(|v| std::cmp::Reverse(numeric_key(v)));
    bail!(
        "sdk_version '{sdk_version}' not found in any recent VS snapshot; available: {}",
        sorted.join(", "),
    )
}

/// Distinct SDK versions discovered across the newest snapshot of each VS
/// series. Sorted descending by numeric key.
pub fn discover_sdk_versions(urls: &MirrorUrls, ctx: &InstallCtx) -> Result<Vec<String>> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for vs in VsVersion::all() {
        let entry = match newest_entry_for(urls, vs, ctx) {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!("skipping {} for SDK discovery: {err:#}", vs.as_str());
                continue;
            }
        };
        let manifest = match vs_manifest::fetch_manifest_at(&urls.raw_base, &entry.commit.sha, ctx) {
            Ok(m) => m,
            Err(err) => {
                tracing::warn!(
                    "skipping {} manifest for SDK discovery: {err:#}",
                    vs.as_str()
                );
                continue;
            }
        };
        for (v, _id) in find_sdk_candidates(&manifest) {
            seen.insert(v);
        }
    }
    let mut out: Vec<String> = seen.into_iter().collect();
    out.sort_by_key(|v| std::cmp::Reverse(numeric_key(v)));
    Ok(out)
}

/// Find every Windows SDK component meta-package in a manifest. Returns
/// `(sdk_version, package_id)` sorted descending by numeric version.
pub fn find_sdk_candidates(manifest: &VsManifest) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for pkg in &manifest.packages {
        for prefix in [
            "Microsoft.VisualStudio.Component.Windows10SDK.",
            "Microsoft.VisualStudio.Component.Windows11SDK.",
        ] {
            if let Some(rest) = pkg.id.strip_prefix(prefix) {
                if is_numeric_sdk_version(rest) {
                    out.push((rest.to_string(), pkg.id.clone()));
                }
            }
        }
    }
    out.sort_by(|a, b| numeric_key(&b.0).cmp(&numeric_key(&a.0)));
    out
}

/// `(filename_prefix, group)` pairs for every MSI in the SDK component
/// meta-package's dep graph. Groups: `"default"` if the MSI is in the
/// essential set, `"extras"` otherwise.
///
/// Used by the `windows_sdk packages` CLI to show what an `extras = [...]`
/// list could pick up.
pub fn enumerate_msis(manifest: &VsManifest, sdk_pkg_id: &str) -> Result<Vec<(String, String)>> {
    let component = lookup_exact(manifest, sdk_pkg_id)
        .ok_or_else(|| anyhow!("SDK component {sdk_pkg_id} not in manifest"))?;
    let mut out: BTreeSet<(String, String)> = BTreeSet::new();
    for dep_id in component.dependencies.keys() {
        let Some(pkg) = lookup_exact(manifest, dep_id) else {
            continue;
        };
        for p in &pkg.payloads {
            let filename = payload_filename(p);
            if !filename.to_lowercase().ends_with(".msi") {
                continue;
            }
            let base = strip_installer_prefix(&filename);
            let group = if DEFAULT_ESSENTIAL_MSIS.iter().any(|p| base.starts_with(p)) {
                "default"
            } else {
                "extras"
            };
            out.insert((base, group.to_string()));
        }
    }
    Ok(out.into_iter().collect())
}

fn newest_entry_for(
    urls: &MirrorUrls,
    vs: VsVersion,
    ctx: &InstallCtx,
) -> Result<BuildIndexEntry> {
    let entries = vs_manifest::build_index(urls, &[vs], ctx)?;
    entries
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no commits on mirror for {}", vs.as_str()))
}

// ---- selection -------------------------------------------------------------

/// Verify every `opts.extras` prefix matches at least one MSI in the SDK's
/// dep graph. A typo should error rather than silently produce a default
/// install, matching `msvc`'s zero-match semantics.
fn check_extras_match(
    manifest: &VsManifest,
    dep_ids: &[String],
    opts: &Options,
) -> Result<()> {
    if opts.extras.is_empty() {
        return Ok(());
    }
    let mut hits = vec![false; opts.extras.len()];
    for dep_id in dep_ids {
        let Some(pkg) = lookup_exact(manifest, dep_id) else {
            continue;
        };
        for p in &pkg.payloads {
            let filename = payload_filename(p);
            if !filename.to_lowercase().ends_with(".msi") {
                continue;
            }
            let base = strip_installer_prefix(&filename);
            for (i, extra) in opts.extras.iter().enumerate() {
                if base.starts_with(extra) {
                    hits[i] = true;
                }
            }
        }
    }
    let unmatched: Vec<&str> = opts
        .extras
        .iter()
        .enumerate()
        .filter(|(i, _)| !hits[*i])
        .map(|(_, e)| e.as_str())
        .collect();
    if !unmatched.is_empty() {
        bail!(
            "`windows_sdk`: extras matched no MSIs in SDK {}: [{}]",
            opts.sdk_version,
            unmatched.join(", "),
        );
    }
    Ok(())
}

fn msi_should_extract(filename: &str, opts: &Options) -> bool {
    let base = strip_installer_prefix(filename);
    let matches_default = DEFAULT_ESSENTIAL_MSIS.iter().any(|p| base.starts_with(p));
    let matches_extras = opts.extras.iter().any(|p| base.starts_with(p));
    match opts.base {
        BaseInstall::Full => true,
        BaseInstall::Default => matches_default || matches_extras,
        BaseInstall::None => matches_extras,
    }
}

// ---- helpers ---------------------------------------------------------------

fn lookup_exact<'a>(manifest: &'a VsManifest, id: &str) -> Option<&'a Package> {
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

/// VS manifest filenames sometimes embed install-subdirectory components
/// (`Installers\foo.msi`, `Redistributable\10.1.0.0\UAPSDKAddOn-x86.msi`).
/// Flatten to just the basename so each MSI's external sibling CABs
/// resolve correctly during extraction.
pub fn strip_installer_prefix(filename: &str) -> String {
    let normalized = filename.replace('\\', "/");
    normalized
        .rsplit('/')
        .next()
        .unwrap_or(&normalized)
        .to_string()
}

/// MSIs extract under `<src>/Windows Kits/10/*`. Move each child up to
/// `<dst>/` so the staged tree starts at Include/Lib/etc. Merges when names
/// collide. `pub` so the CLI's per-MSI extract can reuse it.
pub fn flatten_windows_kits_into(src: &Path, dst: &Path) -> Result<()> {
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

fn numeric_key(v: &str) -> Vec<u64> {
    v.split('.').map(|s| s.parse::<u64>().unwrap_or(0)).collect()
}

// ---- option helpers --------------------------------------------------------

fn required_str<'a>(options: &'a toml::Table, key: &str) -> Result<&'a str> {
    options
        .get(key)
        .ok_or_else(|| anyhow!("`windows_sdk` provider requires options.{key}"))?
        .as_str()
        .ok_or_else(|| anyhow!("`windows_sdk` option '{key}' must be a string"))
}

fn optional_str<'a>(options: &'a toml::Table, key: &str) -> Result<Option<&'a str>> {
    match options.get(key) {
        None => Ok(None),
        Some(v) => v
            .as_str()
            .map(Some)
            .ok_or_else(|| anyhow!("`windows_sdk` option '{key}' must be a string")),
    }
}

fn optional_string_list(options: &toml::Table, key: &str) -> Result<Vec<String>> {
    let Some(v) = options.get(key) else {
        return Ok(Vec::new());
    };
    let arr = v
        .as_array()
        .ok_or_else(|| anyhow!("`windows_sdk` option '{key}' must be an array of strings"))?;
    let mut out = Vec::new();
    for item in arr {
        let s = item
            .as_str()
            .ok_or_else(|| anyhow!("`windows_sdk` option '{key}' entries must be strings"))?;
        if s.is_empty() {
            bail!("`windows_sdk` option '{key}' entries may not be empty");
        }
        if !out.iter().any(|x: &String| x == s) {
            out.push(s.to_string());
        }
    }
    Ok(out)
}

// ---- fingerprint + display -------------------------------------------------

fn fingerprint(opts: &Options) -> String {
    let mut key = String::new();
    key.push_str(&opts.sdk_version);
    key.push('\n');
    key.push_str(opts.base.as_str());
    key.push('\n');
    let mut extras = opts.extras.clone();
    extras.sort();
    for extra in extras {
        key.push_str("extra\t");
        key.push_str(&extra);
        key.push('\n');
    }
    let hash = xxh3_64(key.as_bytes());
    sanitize_fingerprint(&format!("windows_sdk-{}-{hash:016x}", opts.sdk_version))
}

fn display_for(opts: &Options) -> String {
    format!("windows_sdk {} (base={})", opts.sdk_version, opts.base.as_str())
}

fn resolved_options(opts: &Options) -> toml::Table {
    let mut o = toml::Table::new();
    o.insert(
        "sdk_version".into(),
        toml::Value::String(opts.sdk_version.clone()),
    );
    o.insert(
        "base_install".into(),
        toml::Value::String(opts.base.as_str().to_string()),
    );
    if !opts.extras.is_empty() {
        o.insert(
            "extras".into(),
            toml::Value::Array(
                opts.extras
                    .iter()
                    .map(|s| toml::Value::String(s.clone()))
                    .collect(),
            ),
        );
    }
    o
}

#[cfg(test)]
#[path = "tests/windows_sdk.rs"]
mod tests;
