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
use std::sync::Mutex;
use std::sync::mpsc;

use crate::cli::resolve_config_path;
use crate::download;
use crate::extract;
use crate::providers::msvc;
use crate::providers::vs_manifest::{
    self, BuildIndexEntry, ManifestHistory, Package, VsVersion,
};
use crate::providers::InstallCtx;

/// Worker pool size for parallel package extraction. Each worker
/// downloads + unzips one package at a time.
const EXTRACT_PARALLELISM: usize = 16;

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
    let history = ManifestHistory::open(&vs_manifest::default_remote(), ctx.cache())?;
    match args.verb {
        MsvcVerb::Versions(a) => run_versions(&history, a, &mut std::io::stdout()),
        MsvcVerb::Packages(a) => run_packages(&history, a, &mut std::io::stdout()),
        MsvcVerb::Extract(a) => run_extract(&ctx, &history, a, &mut std::io::stdout()),
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
    history: &ManifestHistory,
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

        let entries = history.index(&[vs])?;
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
    history: &ManifestHistory,
    args: PackagesArgs,
    out: &mut dyn std::io::Write,
) -> Result<()> {
    let (entry, manifest) = resolve_and_load(history, &args.build_version)?;
    let compiler = msvc::find_primary_compiler(&manifest, entry.vs)?;
    let family = msvc::family_prefix(&compiler.id)?;
    let compiler_version = primary_version(compiler)?;

    // Scoping rules:
    //   in-family:     any package whose id starts with the primary
    //                  compiler's family prefix. NO version filter —
    //                  Microsoft sometimes patches the compiler without
    //                  bumping CRT/ATL/MFC versions, so the family is at
    //                  several versions inside a single snapshot.
    //   out-of-family: NOT in family AND version matches the primary
    //                  compiler's version. Catches `Microsoft.VC.Preview.*`
    //                  style escape-hatch packages that ship as part of
    //                  this release but live in a different id namespace.
    // Everything else (legacy compat toolsets at other family prefixes,
    // Android workloads, .NET tools, etc.) is excluded.
    let mut in_family: Vec<&Package> = Vec::new();
    let mut out_of_family: Vec<&Package> = Vec::new();
    for pkg in &manifest.packages {
        if vs_manifest::starts_with_ignore_ascii_case(&pkg.id, &family) {
            in_family.push(pkg);
        } else if pkg.version.as_deref() == Some(compiler_version) {
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
    history: &ManifestHistory,
    args: ExtractArgs,
    out: &mut dyn std::io::Write,
) -> Result<()> {
    let (entry, manifest) = resolve_and_load(history, &args.build_version)?;
    std::fs::create_dir_all(&args.out)
        .with_context(|| format!("creating {}", args.out.display()))?;

    let compiler = msvc::find_primary_compiler(&manifest, entry.vs)?;
    let family = msvc::family_prefix(&compiler.id)?;
    let compiler_version = primary_version(compiler)?;

    // Same scope as `packages`: in-family (any version) ∪ out-of-family
    // at the primary compiler's version. See run_packages above for the
    // rationale.
    let mut to_extract: Vec<&Package> = manifest
        .packages
        .iter()
        .filter(|p| {
            vs_manifest::starts_with_ignore_ascii_case(&p.id, &family)
                || p.version.as_deref() == Some(compiler_version)
        })
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

    // Skip-vs-work partition runs sequentially (just stat()s) so the
    // skip lines stay grouped at the top in stable order. Work goes to
    // the parallel pool.
    let mut skipped = 0usize;
    let mut work: Vec<(String, PathBuf, &Package)> = Vec::new();
    for pkg in to_extract {
        let dir_name = per_package_dir_name(&pkg.id, pkg.language.as_deref());
        let dest = args.out.join(&dir_name);
        if dir_has_content(&dest) {
            writeln!(out, "  skip   {dir_name} (already populated)")?;
            skipped += 1;
        } else {
            work.push((dir_name, dest, pkg));
        }
    }

    let extracted = extract_in_parallel(out, &work, &ctx.cache().downloads)?;

    writeln!(
        out,
        "\ndone: {extracted} extracted, {skipped} already present\noutput: {}",
        args.out.display(),
    )?;
    Ok(())
}

/// Run downloads + extractions across [`EXTRACT_PARALLELISM`] worker
/// threads. Workers pull job indices off a shared queue, do the
/// download + extract, and report completion (or the first error) via
/// an mpsc channel. The main thread drains the channel, printing each
/// `  ok` line as it arrives so progress is live, and bails on the
/// first error.
fn extract_in_parallel(
    out: &mut dyn std::io::Write,
    work: &[(String, PathBuf, &Package)],
    downloads: &Path,
) -> Result<usize> {
    let queue: Mutex<Vec<usize>> = Mutex::new((0..work.len()).rev().collect());
    let (tx, rx) = mpsc::channel::<Result<String>>();
    let worker_count = EXTRACT_PARALLELISM.min(work.len());

    std::thread::scope(|s| -> Result<usize> {
        for _ in 0..worker_count {
            let tx = tx.clone();
            // Rebind queue as a reference so the `move` closure captures
            // `&Mutex<_>` by Copy instead of trying to move the Mutex
            // itself (which would also conflict with cancel() below).
            let queue = &queue;
            s.spawn(move || loop {
                let idx = match queue.lock().unwrap().pop() {
                    Some(i) => i,
                    None => return,
                };
                let (dir_name, dest, pkg) = &work[idx];
                let result = extract_one(pkg, dest, downloads).map(|_| dir_name.clone());
                if tx.send(result).is_err() {
                    return;
                }
            });
        }
        drop(tx); // close the sender side so rx ends when workers finish

        // Cancel = drain the queue so in-flight workers exit after their
        // current task instead of grinding through every remaining job
        // before scope() can join.
        let cancel = || queue.lock().unwrap().clear();

        let mut count = 0usize;
        for msg in rx {
            match msg {
                Ok(name) => {
                    if let Err(e) = writeln!(out, "  ok     {name}") {
                        cancel();
                        return Err(e.into());
                    }
                    count += 1;
                }
                Err(e) => {
                    cancel();
                    return Err(e);
                }
            }
        }
        Ok(count)
    })
}

fn extract_one(pkg: &Package, dest: &Path, downloads: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)
        .with_context(|| format!("creating {}", dest.display()))?;
    for payload in &pkg.payloads {
        let filename = vs_manifest::payload_filename(payload);
        let archive = download::fetch(&payload.url, payload.sha256.as_deref(), downloads)
            .with_context(|| format!("downloading {filename} for {}", pkg.id))?;
        extract::extract_msvc_payload(&archive, &filename, dest)
            .with_context(|| format!("extracting {filename} for {}", pkg.id))?;
    }
    Ok(())
}

// ---- shared helpers --------------------------------------------------------

fn resolve_and_load(
    history: &ManifestHistory,
    build_version: &str,
) -> Result<(BuildIndexEntry, vs_manifest::VsManifest)> {
    let entry = history.resolve_build_version(build_version)?;
    let manifest = history.manifest(&entry.commit.sha)?;
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

#[cfg(test)]
#[path = "tests/msvc.rs"]
mod tests;
