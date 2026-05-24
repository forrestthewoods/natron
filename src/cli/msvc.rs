//! `natron msvc` — debug + discovery tooling for the MSVC provider.
//!
//! Three verbs:
//! - `versions`: list MSVC toolset versions across VS series (live + archive).
//! - `packages`: list every package at a given MSVC version.
//! - `extract`:  download + extract every package at a version into its own
//!   subdirectory on disk, for browsing with external tools.
//!
//! None of these mutate `<cache>/installs/` or the project's state.

use anyhow::{Context, Result, anyhow};
use clap::{Args, Subcommand};
use std::path::{Path, PathBuf};

use crate::cli::resolve_config_path;
use crate::extract;
use crate::providers::vs_manifest::{self, Package, VsManifest, VsVersion};
use crate::providers::{InstallCtx, msvc};

const HOST: &str = "x64";
const TARGET: &str = "x64";

#[derive(Debug, Args)]
pub struct MsvcArgs {
    #[command(subcommand)]
    pub verb: MsvcVerb,
}

#[derive(Debug, Subcommand)]
pub enum MsvcVerb {
    /// List MSVC toolset versions across live + archived manifests.
    Versions(VersionsArgs),
    /// List every package at a specific MSVC version.
    Packages(PackagesArgs),
    /// Download + extract every package at a version into per-package dirs.
    Extract(ExtractArgs),
}

#[derive(Debug, Args)]
pub struct VersionsArgs {
    /// Limit to one VS series (vs2019, vs2022, or vs2026). Default: all three.
    #[arg(long)]
    pub vs: Option<String>,
}

#[derive(Debug, Args)]
pub struct PackagesArgs {
    /// VS series (vs2019, vs2022, or vs2026).
    #[arg(long)]
    pub vs: String,
    /// Exact MSVC package version (e.g. 14.51.36243). Default: latest in
    /// the live manifest.
    #[arg(long)]
    pub version: Option<String>,
}

#[derive(Debug, Args)]
pub struct ExtractArgs {
    /// VS series (vs2019, vs2022, or vs2026).
    #[arg(long)]
    pub vs: String,
    /// Exact MSVC package version (e.g. 14.51.36243).
    #[arg(long)]
    pub version: String,
    /// Output directory. Each package extracts into a subdirectory here.
    /// The user owns cleanup.
    #[arg(long, value_name = "PATH")]
    pub out: PathBuf,
}

pub fn run(
    config: &Option<PathBuf>,
    cache_dir_override: &Option<PathBuf>,
    args: MsvcArgs,
) -> Result<()> {
    let ctx = build_ctx(config, cache_dir_override)?;
    let urls = ChannelUrls::default();
    match args.verb {
        MsvcVerb::Versions(a) => run_versions(&ctx, &urls, a, &mut std::io::stdout()),
        MsvcVerb::Packages(a) => run_packages(&ctx, &urls, a, &mut std::io::stdout()),
        MsvcVerb::Extract(a) => run_extract(&ctx, &urls, a, &mut std::io::stdout()),
    }
}

/// Pair of URL templates for the live channel + archive mirror. Pulled into a
/// struct so tests can swap both at once with `file://` fixtures.
#[derive(Debug, Clone)]
struct ChannelUrls {
    live: String,
    archive: String,
}

impl Default for ChannelUrls {
    fn default() -> Self {
        Self {
            live: vs_manifest::DEFAULT_CHANNEL_URL_TEMPLATE.to_string(),
            archive: msvc::ARCHIVE_MANIFEST_URL_TEMPLATE.to_string(),
        }
    }
}

/// Cache + InstallCtx wiring shared by all three verbs.
fn build_ctx(
    config: &Option<PathBuf>,
    cache_dir_override: &Option<PathBuf>,
) -> Result<InstallCtx> {
    let cfg = resolve_config_path(config)
        .ok()
        .and_then(|p| crate::config::Config::load(&p).ok());
    let cfg_cache_setting = cfg.as_ref().and_then(|c| c.settings.cache_dir.clone());
    let cache = crate::cache::Cache::resolve(
        cache_dir_override.as_deref(),
        cfg_cache_setting.as_deref(),
    )?;
    cache.ensure_layout()?;
    Ok(InstallCtx::new(cache))
}

fn parse_vs(value: &str) -> Result<VsVersion> {
    VsVersion::parse(value).map_err(|e| anyhow!("{e}"))
}

// ---- versions ---------------------------------------------------------------

fn run_versions(
    ctx: &InstallCtx,
    urls: &ChannelUrls,
    args: VersionsArgs,
    out: &mut dyn std::io::Write,
) -> Result<()> {
    let series: Vec<VsVersion> = match args.vs.as_deref() {
        Some(v) => vec![parse_vs(v)?],
        None => vec![VsVersion::Vs2019, VsVersion::Vs2022, VsVersion::Vs2026],
    };

    let mut first = true;
    for vs in series {
        if !first {
            writeln!(out)?;
        }
        first = false;
        writeln!(out, "{} (channel {})", vs.as_str(), vs.channel())?;

        let live_versions = match vs_manifest::fetch_vs_manifest(&urls.live, vs.channel(), ctx) {
            Ok(m) => candidate_versions(&m),
            Err(err) => {
                writeln!(out, "  live: <error: {err:#}>")?;
                Vec::new()
            }
        };
        let archive_versions =
            match vs_manifest::fetch_archive_manifest(&urls.archive, vs.channel(), ctx) {
                Ok(m) => candidate_versions(&m),
                Err(err) => {
                    writeln!(out, "  archive: <error: {err:#}>")?;
                    Vec::new()
                }
            };

        let mut merged: Vec<(String, bool, bool)> = Vec::new();
        for v in &live_versions {
            merged.push((v.clone(), true, archive_versions.contains(v)));
        }
        for v in &archive_versions {
            if !live_versions.contains(v) {
                merged.push((v.clone(), false, true));
            }
        }
        merged.sort_by(|a, b| version_key(&b.0).cmp(&version_key(&a.0)));

        if merged.is_empty() {
            writeln!(out, "  (no versions discovered)")?;
        } else {
            for (ver, in_live, in_archive) in merged {
                let tag = match (in_live, in_archive) {
                    (true, true) => "live, archive",
                    (true, false) => "live",
                    (false, true) => "archive-only",
                    (false, false) => unreachable!(),
                };
                writeln!(out, "  {ver:20} {tag}")?;
            }
        }
    }
    Ok(())
}

fn candidate_versions(manifest: &VsManifest) -> Vec<String> {
    manifest
        .find_msvc_candidates(HOST, TARGET)
        .into_iter()
        .map(|c| c.package_version)
        .collect()
}

fn version_key(v: &str) -> Vec<u64> {
    v.split('.')
        .map(|s| s.parse::<u64>().unwrap_or(0))
        .collect()
}

// ---- packages ---------------------------------------------------------------

fn run_packages(
    ctx: &InstallCtx,
    urls: &ChannelUrls,
    args: PackagesArgs,
    out: &mut dyn std::io::Write,
) -> Result<()> {
    let vs = parse_vs(&args.vs)?;
    let resolved = resolve_version_and_family(ctx, urls, vs, args.version.as_deref())?;

    let mut in_family: Vec<&Package> = Vec::new();
    let mut out_of_family: Vec<&Package> = Vec::new();
    for pkg in &resolved.manifest.packages {
        if pkg.version.as_deref() != Some(resolved.package_version.as_str()) {
            continue;
        }
        if starts_with_ignore_ascii_case(&pkg.id, &resolved.family_prefix) {
            in_family.push(pkg);
        } else {
            out_of_family.push(pkg);
        }
    }
    in_family.sort_by(|a, b| {
        a.id.to_ascii_lowercase()
            .cmp(&b.id.to_ascii_lowercase())
            .then_with(|| a.language.cmp(&b.language))
    });
    out_of_family.sort_by(|a, b| {
        a.id.to_ascii_lowercase()
            .cmp(&b.id.to_ascii_lowercase())
            .then_with(|| a.language.cmp(&b.language))
    });

    writeln!(
        out,
        "{} ({}) — resolved {} packages in family {}, {} outside",
        vs.as_str(),
        resolved.package_version,
        in_family.len(),
        resolved.family_prefix.trim_end_matches('.'),
        out_of_family.len()
    )?;
    writeln!(out)?;
    writeln!(out, "== family ==")?;
    if in_family.is_empty() {
        writeln!(out, "  (none)")?;
    } else {
        for pkg in &in_family {
            print_package(out, pkg)?;
        }
    }
    writeln!(out)?;
    writeln!(out, "== other Microsoft.VC.* at same version ==")?;
    if out_of_family.is_empty() {
        writeln!(out, "  (none)")?;
    } else {
        for pkg in &out_of_family {
            print_package(out, pkg)?;
        }
    }
    Ok(())
}

fn print_package(out: &mut dyn std::io::Write, pkg: &Package) -> Result<()> {
    let lang = pkg
        .language
        .as_deref()
        .map(|l| format!(" [{l}]"))
        .unwrap_or_default();
    writeln!(
        out,
        "  {}{}  ({} payload{})",
        pkg.id,
        lang,
        pkg.payloads.len(),
        if pkg.payloads.len() == 1 { "" } else { "s" }
    )?;
    Ok(())
}

fn starts_with_ignore_ascii_case(value: &str, prefix: &str) -> bool {
    value.len() >= prefix.len() && value[..prefix.len()].eq_ignore_ascii_case(prefix)
}

// ---- extract ----------------------------------------------------------------

fn run_extract(
    ctx: &InstallCtx,
    urls: &ChannelUrls,
    args: ExtractArgs,
    out: &mut dyn std::io::Write,
) -> Result<()> {
    let vs = parse_vs(&args.vs)?;
    let resolved = resolve_version_and_family(ctx, urls, vs, Some(&args.version))?;
    std::fs::create_dir_all(&args.out)
        .with_context(|| format!("creating {}", args.out.display()))?;

    let mut to_extract: Vec<&Package> = resolved
        .manifest
        .packages
        .iter()
        .filter(|p| p.version.as_deref() == Some(resolved.package_version.as_str()))
        .collect();
    to_extract.sort_by(|a, b| {
        a.id.to_ascii_lowercase()
            .cmp(&b.id.to_ascii_lowercase())
            .then_with(|| a.language.cmp(&b.language))
    });

    writeln!(
        out,
        "extracting {} packages from msvc {} ({}) -> {}",
        to_extract.len(),
        resolved.package_version,
        vs.as_str(),
        args.out.display()
    )?;

    let mut extracted = 0usize;
    let mut skipped = 0usize;
    for pkg in to_extract {
        let dir_name = per_package_dir_name(&pkg.id, pkg.language.as_deref());
        let dest = args.out.join(&dir_name);
        if dir_has_content(&dest) {
            writeln!(out, "  skip   {dir_name} (already populated)")?;
            skipped += 1;
            continue;
        }
        std::fs::create_dir_all(&dest)
            .with_context(|| format!("creating {}", dest.display()))?;
        for payload in &pkg.payloads {
            let filename = payload
                .file_name
                .clone()
                .or_else(|| filename_from_url(&payload.url))
                .unwrap_or_else(|| "unknown.bin".to_string());
            let archive = ctx
                .download(&payload.url, payload.sha256.as_deref())
                .with_context(|| format!("downloading {filename} for {}", pkg.id))?;
            extract_payload(&archive, &filename, &dest)
                .with_context(|| format!("extracting {filename} for {}", pkg.id))?;
        }
        writeln!(out, "  ok     {dir_name}")?;
        extracted += 1;
    }

    writeln!(
        out,
        "\ndone: {extracted} extracted, {skipped} already present\noutput: {}",
        args.out.display()
    )?;
    Ok(())
}

fn per_package_dir_name(id: &str, language: Option<&str>) -> String {
    match language {
        Some(lang) => format!("{id}+{lang}"),
        None => id.to_string(),
    }
}

fn dir_has_content(p: &Path) -> bool {
    match std::fs::read_dir(p) {
        Ok(mut it) => it.next().is_some(),
        Err(_) => false,
    }
}

fn extract_payload(archive: &Path, filename: &str, dest: &Path) -> Result<()> {
    let lower = filename.to_lowercase();
    if lower.ends_with(".vsix") || lower.ends_with(".zip") {
        extract::extract_vsix(archive, dest)?;
    } else if lower.ends_with(".msi") {
        extract::extract_msi(archive, dest)?;
    } else {
        tracing::warn!("skipping payload with unknown extension: {filename}");
    }
    Ok(())
}

fn filename_from_url(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    parsed
        .path_segments()
        .and_then(|mut s| s.next_back())
        .map(|s| s.to_string())
}

// ---- shared resolution ------------------------------------------------------

/// Mirror of msvc::ResolvedMsvcToolset, kept private to the CLI so the
/// provider doesn't have to expose its internal shape.
struct CliResolvedToolset {
    manifest: VsManifest,
    package_version: String,
    family_prefix: String,
}

/// Resolve a VS channel + optional pinned version to a concrete toolset.
/// Tries live first, then archive, matching the production provider's order.
fn resolve_version_and_family(
    ctx: &InstallCtx,
    urls: &ChannelUrls,
    vs: VsVersion,
    pinned: Option<&str>,
) -> Result<CliResolvedToolset> {
    let live = vs_manifest::fetch_vs_manifest(&urls.live, vs.channel(), ctx);
    let archive = || vs_manifest::fetch_archive_manifest(&urls.archive, vs.channel(), ctx);

    // Try live first.
    if let Ok(manifest) = live.as_ref() {
        if let Some(candidate) = pick_candidate(manifest, pinned) {
            return Ok(CliResolvedToolset {
                family_prefix: msvc::family_prefix_from_compiler_package(&candidate.package_id)?,
                package_version: candidate.package_version,
                manifest: clone_manifest(manifest),
            });
        }
    }

    // Fall back to archive.
    match archive() {
        Ok(manifest) => {
            if let Some(candidate) = pick_candidate(&manifest, pinned) {
                return Ok(CliResolvedToolset {
                    family_prefix: msvc::family_prefix_from_compiler_package(
                        &candidate.package_id,
                    )?,
                    package_version: candidate.package_version,
                    manifest,
                });
            }
            let pinned_disp = pinned.unwrap_or("<latest>");
            let live_versions = live
                .as_ref()
                .map(|m| candidate_versions(m).join(", "))
                .unwrap_or_else(|err| format!("<error: {err:#}>"));
            let archive_versions = candidate_versions(&manifest).join(", ");
            anyhow::bail!(
                "could not resolve msvc version='{pinned_disp}' for {} (channel {}); live versions: {live_versions}; archive versions: {archive_versions}",
                vs.as_str(),
                vs.channel(),
            );
        }
        Err(archive_err) => {
            let live_err = live.err().map(|e| format!("{e:#}")).unwrap_or_default();
            anyhow::bail!(
                "could not resolve msvc version for {} (channel {}); live: {live_err}; archive: {archive_err:#}",
                vs.as_str(),
                vs.channel(),
            );
        }
    }
}

fn pick_candidate(
    manifest: &VsManifest,
    pinned: Option<&str>,
) -> Option<vs_manifest::MsvcCandidate> {
    let candidates = manifest.find_msvc_candidates(HOST, TARGET);
    match pinned {
        Some(req) => candidates
            .into_iter()
            .find(|c| c.package_version == req),
        None => candidates.into_iter().next(),
    }
}

/// VsManifest doesn't derive Clone (and serde's Value inside dependencies
/// would make it expensive). Re-serialize/deserialize for the rare case
/// the live resolution succeeded but we still want the manifest after the
/// borrow ends.
fn clone_manifest(m: &VsManifest) -> VsManifest {
    // Cheaper alternative: just refetch. The live fetch is already cached
    // in <cache>/downloads/ via the staging URL, so the disk + parse cost
    // is what we pay. For now, refetch by clone of packages.
    let packages = m
        .packages
        .iter()
        .map(|p| Package {
            id: p.id.clone(),
            version: p.version.clone(),
            payloads: p.payloads.clone(),
            language: p.language.clone(),
            dependencies: p.dependencies.clone(),
        })
        .collect();
    VsManifest { packages }
}

#[cfg(test)]
#[path = "msvc_tests.rs"]
mod tests;
