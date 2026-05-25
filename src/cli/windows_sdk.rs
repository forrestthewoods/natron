//! `natron windows_sdk` — discovery + debug tooling for the windows_sdk
//! provider. Mirrors the three verbs offered by `natron msvc`.

use anyhow::{Context, Result, anyhow};
use clap::{Args, Subcommand};
use std::path::{Path, PathBuf};

use crate::cli::resolve_config_path;
use crate::extract;
use crate::fs_util;
use crate::providers::vs_manifest::{self, MirrorUrls};
use crate::providers::windows_sdk;
use crate::providers::InstallCtx;

#[derive(Debug, Args)]
pub struct WindowsSdkArgs {
    #[command(subcommand)]
    pub verb: WindowsSdkVerb,
}

#[derive(Debug, Subcommand)]
pub enum WindowsSdkVerb {
    /// List Windows SDK versions reachable via the mirror.
    Versions(VersionsArgs),
    /// List MSIs in one SDK's component meta-package, grouped by
    /// default-installed vs available-for-extras.
    Packages(PackagesArgs),
    /// Download + extract every MSI in one SDK into per-MSI dirs.
    Extract(ExtractArgs),
}

#[derive(Debug, Args)]
pub struct VersionsArgs {}

#[derive(Debug, Args)]
pub struct PackagesArgs {
    /// Exact Windows SDK version (e.g. 26100).
    #[arg(long = "sdk-version")]
    pub sdk_version: String,
}

#[derive(Debug, Args)]
pub struct ExtractArgs {
    /// Exact Windows SDK version.
    #[arg(long = "sdk-version")]
    pub sdk_version: String,
    /// Output directory. Each MSI extracts into its own subdirectory.
    /// You manage cleanup.
    #[arg(long, value_name = "PATH")]
    pub out: PathBuf,
}

pub fn run(
    config: &Option<PathBuf>,
    cache_dir_override: &Option<PathBuf>,
    args: WindowsSdkArgs,
) -> Result<()> {
    let ctx = build_ctx(config, cache_dir_override)?;
    let urls = MirrorUrls::default();
    match args.verb {
        WindowsSdkVerb::Versions(a) => run_versions(&ctx, &urls, a, &mut std::io::stdout()),
        WindowsSdkVerb::Packages(a) => run_packages(&ctx, &urls, a, &mut std::io::stdout()),
        WindowsSdkVerb::Extract(a) => run_extract(&ctx, &urls, a, &mut std::io::stdout()),
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

// ---- versions --------------------------------------------------------------

fn run_versions(
    ctx: &InstallCtx,
    urls: &MirrorUrls,
    _args: VersionsArgs,
    out: &mut dyn std::io::Write,
) -> Result<()> {
    let versions = windows_sdk::discover_sdk_versions(urls, ctx)?;
    if versions.is_empty() {
        writeln!(out, "(no Windows SDK versions discovered)")?;
        return Ok(());
    }
    writeln!(out, "Windows SDK versions ({}):", versions.len())?;
    for v in versions {
        writeln!(out, "  {v}")?;
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
    let resolved = windows_sdk::resolve_sdk_version(urls, &args.sdk_version, ctx)?;
    let msis = windows_sdk::enumerate_msis(&resolved.manifest, &resolved.sdk_pkg_id)?;

    let mut defaults: Vec<&String> = Vec::new();
    let mut extras: Vec<&String> = Vec::new();
    for (name, group) in &msis {
        if group == "default" {
            defaults.push(name);
        } else {
            extras.push(name);
        }
    }

    writeln!(
        out,
        "windows_sdk {} — {} MSIs total (from snapshot {} on {})",
        args.sdk_version,
        msis.len(),
        resolved.entry.info.build_version,
        resolved.entry.vs.as_str(),
    )?;
    writeln!(out)?;
    writeln!(out, "== installed by base_install=default ({}) ==", defaults.len())?;
    for n in &defaults {
        writeln!(out, "  {n}")?;
    }
    writeln!(out)?;
    writeln!(out, "== available for extras ({}) ==", extras.len())?;
    for n in &extras {
        writeln!(out, "  {n}")?;
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
    let resolved = windows_sdk::resolve_sdk_version(urls, &args.sdk_version, ctx)?;
    let component = manifest_lookup(&resolved.manifest, &resolved.sdk_pkg_id)
        .ok_or_else(|| anyhow!("SDK component {} not in manifest", resolved.sdk_pkg_id))?;
    let dep_ids: Vec<String> = component.dependencies.keys().cloned().collect();
    std::fs::create_dir_all(&args.out)
        .with_context(|| format!("creating {}", args.out.display()))?;

    // The pure-Rust extractor resolves each MSI's external sibling CABs
    // (referenced from the `Media` table) by basename. Our download cache
    // stores files with hash-prefixed names, so we first stage every SDK
    // payload (MSIs + CABs) flat into a scratch dir with original
    // basenames. The provider's install path uses the same trick.
    let staging = ctx.staging_dir()?.to_path_buf();
    let payloads_dir = staging.join("__sdk_payloads");
    let msi_scratch = staging.join("__sdk_scratch");
    std::fs::create_dir_all(&payloads_dir)
        .with_context(|| format!("creating {}", payloads_dir.display()))?;

    writeln!(out, "staging SDK payloads...")?;
    let mut msi_jobs: Vec<(String, PathBuf)> = Vec::new();
    for dep_id in &dep_ids {
        let Some(pkg) = manifest_lookup(&resolved.manifest, dep_id) else {
            continue;
        };
        for p in &pkg.payloads {
            let filename = payload_filename(p);
            let basename = windows_sdk::strip_installer_prefix(&filename);
            let downloaded = ctx
                .download(&p.url, p.sha256.as_deref())
                .with_context(|| format!("downloading {filename}"))?;
            let staged = payloads_dir.join(&basename);
            if !staged.exists() {
                std::fs::hard_link(&downloaded, &staged)
                    .or_else(|_| std::fs::copy(&downloaded, &staged).map(|_| ()))
                    .with_context(|| {
                        format!("staging {} -> {}", downloaded.display(), staged.display())
                    })?;
            }
            if basename.to_lowercase().ends_with(".msi") {
                msi_jobs.push((basename, staged));
            }
        }
    }

    writeln!(
        out,
        "extracting {} MSIs from Windows SDK {} -> {}",
        msi_jobs.len(),
        args.sdk_version,
        args.out.display(),
    )?;

    let mut extracted = 0usize;
    let mut skipped = 0usize;
    for (name, msi_path) in &msi_jobs {
        let dest = args.out.join(name);
        if dir_has_content(&dest) {
            writeln!(out, "  skip   {name} (already populated)")?;
            skipped += 1;
            continue;
        }
        // Fresh scratch per MSI: the extractor writes
        // `<scratch>/Windows Kits/10/*` per the MSI's Directory table.
        // We then move just that subtree into `dest`.
        let _ = fs_util::remove_dir_all_writable(&msi_scratch);
        std::fs::create_dir_all(&msi_scratch)
            .with_context(|| format!("creating {}", msi_scratch.display()))?;
        extract::extract_msi_pure(msi_path, &msi_scratch)
            .with_context(|| format!("extracting {name}"))?;
        windows_sdk::flatten_windows_kits_into(&msi_scratch, &dest)
            .with_context(|| format!("flattening {name}"))?;
        writeln!(out, "  ok     {name}")?;
        extracted += 1;
    }
    let _ = fs_util::remove_dir_all_writable(&payloads_dir);
    let _ = fs_util::remove_dir_all_writable(&msi_scratch);

    writeln!(
        out,
        "\ndone: {extracted} extracted, {skipped} already present\noutput: {}",
        args.out.display(),
    )?;
    Ok(())
}

// ---- helpers ---------------------------------------------------------------

fn manifest_lookup<'a>(
    manifest: &'a vs_manifest::VsManifest,
    id: &str,
) -> Option<&'a vs_manifest::Package> {
    manifest.packages.iter().find(|p| p.id.eq_ignore_ascii_case(id))
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

fn dir_has_content(p: &Path) -> bool {
    match std::fs::read_dir(p) {
        Ok(mut it) => it.next().is_some(),
        Err(_) => false,
    }
}

#[cfg(test)]
#[path = "tests/windows_sdk.rs"]
mod tests;
