//! `natron msvc` — discovery + debug tooling for the MSVC provider.
//!
//! Three verbs, all backed by the same roblabla-mirror data path the
//! provider uses:
//!
//! - `versions [--vs ...]` — list every Microsoft VS build the mirror has,
//!   newest-first per series.
//! - `packages --build-version V` — list every package at one build.
//! - `extract  --build-version V --out PATH` — download + extract every
//!   package at one build into its own subdirectory, for grep/Explorer/etc.
//!
//! None of these write to `<cache>/installs/` or modify project state.

use anyhow::{Context, Result, anyhow};
use clap::{Args, Subcommand};
use std::path::{Path, PathBuf};

use crate::cli::resolve_config_path;
use crate::extract;
use crate::providers::msvc;
use crate::providers::vs_manifest::{
    self, BuildIndexEntry, MirrorUrls, Package, VsVersion,
};
use crate::providers::InstallCtx;

#[derive(Debug, Args)]
pub struct MsvcArgs {
    #[command(subcommand)]
    pub verb: MsvcVerb,
}

#[derive(Debug, Subcommand)]
pub enum MsvcVerb {
    /// List Microsoft VS builds available on the mirror.
    Versions(VersionsArgs),
    /// List every package in one VS build's snapshot manifest.
    Packages(PackagesArgs),
    /// Download + extract every package at one VS build into per-package dirs.
    Extract(ExtractArgs),
}

#[derive(Debug, Args)]
pub struct VersionsArgs {
    /// Limit to one VS series. Default: all three.
    #[arg(long)]
    pub vs: Option<String>,
}

#[derive(Debug, Args)]
pub struct PackagesArgs {
    /// Exact Microsoft VS build_version (e.g. 18.6.11819.183).
    #[arg(long = "build-version")]
    pub build_version: String,
}

#[derive(Debug, Args)]
pub struct ExtractArgs {
    /// Exact Microsoft VS build_version.
    #[arg(long = "build-version")]
    pub build_version: String,
    /// Output directory. Each package extracts into its own subdirectory.
    /// You manage cleanup.
    #[arg(long, value_name = "PATH")]
    pub out: PathBuf,
}

pub fn run(
    config: &Option<PathBuf>,
    cache_dir_override: &Option<PathBuf>,
    args: MsvcArgs,
) -> Result<()> {
    let ctx = build_ctx(config, cache_dir_override)?;
    let urls = MirrorUrls::default();
    match args.verb {
        MsvcVerb::Versions(a) => run_versions(&ctx, &urls, a, &mut std::io::stdout()),
        MsvcVerb::Packages(a) => run_packages(&ctx, &urls, a, &mut std::io::stdout()),
        MsvcVerb::Extract(a) => run_extract(&ctx, &urls, a, &mut std::io::stdout()),
    }
}

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

// ---- versions --------------------------------------------------------------

fn run_versions(
    ctx: &InstallCtx,
    urls: &MirrorUrls,
    args: VersionsArgs,
    out: &mut dyn std::io::Write,
) -> Result<()> {
    let series: Vec<VsVersion> = match args.vs.as_deref() {
        Some(v) => vec![parse_vs(v)?],
        None => VsVersion::all().to_vec(),
    };

    let mut first = true;
    for vs in series {
        if !first {
            writeln!(out)?;
        }
        first = false;
        writeln!(out, "{} (channel {})", vs.as_str(), vs.channel())?;

        let entries = vs_manifest::build_index(urls, &[vs], ctx)?;
        if entries.is_empty() {
            writeln!(out, "  (no builds found on mirror)")?;
            continue;
        }
        for entry in entries {
            writeln!(
                out,
                "  {bv:20}  VS {display:12}  ({date})",
                bv = entry.info.build_version,
                display = entry.info.product_display_version,
                date = entry.commit.date,
            )?;
        }
    }
    Ok(())
}

// ---- packages --------------------------------------------------------------

fn run_packages(
    ctx: &InstallCtx,
    urls: &MirrorUrls,
    args: PackagesArgs,
    out: &mut dyn std::io::Write,
) -> Result<()> {
    let (entry, manifest) = resolve_and_load(ctx, urls, &args.build_version)?;
    let compiler = msvc::find_primary_compiler(&manifest, entry.vs)?;
    let family = msvc::family_prefix(&compiler.id)?;
    let compiler_version = primary_version(compiler)?;

    // Scope to packages at the primary compiler's exact version. A snapshot
    // contains thousands of unrelated entries — Android workloads, .NET
    // tools, legacy compat toolsets at other versions — none of which a
    // user running `msvc packages` would expect to see.
    let mut in_family: Vec<&Package> = Vec::new();
    let mut out_of_family: Vec<&Package> = Vec::new();
    for pkg in &manifest.packages {
        if pkg.version.as_deref() != Some(compiler_version) {
            continue;
        }
        if starts_with_ignore_ascii_case(&pkg.id, &family) {
            in_family.push(pkg);
        } else {
            out_of_family.push(pkg);
        }
    }
    in_family.sort_by_key(|p| sort_key(p));
    out_of_family.sort_by_key(|p| sort_key(p));

    writeln!(
        out,
        "{} build {} (VS {}) — {} in family {}, {} outside",
        entry.vs.as_str(),
        entry.info.build_version,
        entry.info.product_display_version,
        in_family.len(),
        family.trim_end_matches('.'),
        out_of_family.len(),
    )?;
    writeln!(out)?;
    writeln!(out, "== family ==")?;
    print_packages(out, &in_family)?;
    writeln!(out)?;
    writeln!(out, "== other in snapshot ==")?;
    print_packages(out, &out_of_family)?;
    Ok(())
}

fn print_packages(out: &mut dyn std::io::Write, packages: &[&Package]) -> Result<()> {
    if packages.is_empty() {
        writeln!(out, "  (none)")?;
        return Ok(());
    }
    for pkg in packages {
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
    }
    Ok(())
}

// ---- extract ---------------------------------------------------------------

fn run_extract(
    ctx: &InstallCtx,
    urls: &MirrorUrls,
    args: ExtractArgs,
    out: &mut dyn std::io::Write,
) -> Result<()> {
    let (entry, manifest) = resolve_and_load(ctx, urls, &args.build_version)?;
    std::fs::create_dir_all(&args.out)
        .with_context(|| format!("creating {}", args.out.display()))?;

    let compiler = msvc::find_primary_compiler(&manifest, entry.vs)?;
    let compiler_version = primary_version(compiler)?;

    // Same scope as `packages`: filter to the primary compiler's exact
    // version. Excludes Android/Python/etc. workloads + legacy compat
    // toolsets at other versions that just happen to ship in the same
    // VS snapshot.
    let mut to_extract: Vec<&Package> = manifest
        .packages
        .iter()
        .filter(|p| p.version.as_deref() == Some(compiler_version))
        .collect();
    to_extract.sort_by_key(|p| sort_key(p));

    writeln!(
        out,
        "extracting {} packages from {} build {} (VS {}) -> {}",
        to_extract.len(),
        entry.vs.as_str(),
        entry.info.build_version,
        entry.info.product_display_version,
        args.out.display(),
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
            let filename = payload_filename(payload);
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
        args.out.display(),
    )?;
    Ok(())
}

// ---- shared helpers --------------------------------------------------------

fn resolve_and_load(
    ctx: &InstallCtx,
    urls: &MirrorUrls,
    build_version: &str,
) -> Result<(BuildIndexEntry, vs_manifest::VsManifest)> {
    let entry = vs_manifest::resolve_build_version(urls, build_version, ctx)?;
    let manifest = vs_manifest::fetch_manifest_at(&urls.raw_base, &entry.commit.sha, ctx)?;
    Ok((entry, manifest))
}

fn sort_key(p: &Package) -> (String, Option<String>) {
    (p.id.to_ascii_lowercase(), p.language.clone())
}

fn primary_version(pkg: &Package) -> Result<&str> {
    pkg.version.as_deref().ok_or_else(|| {
        anyhow!("primary compiler {} has no version field", pkg.id)
    })
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

fn payload_filename(payload: &vs_manifest::Payload) -> String {
    if let Some(name) = &payload.file_name {
        return name.clone();
    }
    if let Ok(parsed) = url::Url::parse(&payload.url) {
        if let Some(seg) = parsed.path_segments().and_then(|mut s| s.next_back()) {
            if !seg.is_empty() {
                return seg.to_string();
            }
        }
    }
    "unknown.bin".to_string()
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

fn starts_with_ignore_ascii_case(value: &str, prefix: &str) -> bool {
    value.len() >= prefix.len() && value[..prefix.len()].eq_ignore_ascii_case(prefix)
}

#[cfg(test)]
#[path = "msvc_tests.rs"]
mod tests;
