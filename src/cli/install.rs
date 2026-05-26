//! `natron install` subcommand.

use anyhow::{Context, Result};
use std::path::PathBuf;

use crate::cli::{InstallArgs, resolve_config_path};
use crate::engine::{Natron, SyncAction, SyncOptions};

pub fn run(
    config: &Option<PathBuf>,
    cache_dir_override: &Option<PathBuf>,
    args: InstallArgs,
) -> Result<()> {
    let cfg_path = resolve_config_path(config)?;
    let cfg = crate::config::Config::load(&cfg_path)?;
    let cache = crate::cache::Cache::resolve(
        cache_dir_override.as_deref(),
        cfg.settings.cache_dir.as_deref(),
    )?;
    let registry = crate::providers::ProviderRegistry::default();
    let mut opts = SyncOptions::default();
    opts.dry_run = args.dry_run;
    opts.mode_override = args.parse_mode()?;
    for n in &args.only {
        opts.only.insert(n.clone());
    }
    let n = Natron::new(cfg, cache, registry).with_options(opts);
    let report = n.sync().context("sync failed")?;

    // Print a summary.
    if report.entries.is_empty() && report.errors.is_empty() {
        println!("no toolchains in config; nothing to do");
        return Ok(());
    }
    println!("== natron sync ==");
    for e in &report.entries {
        let action = match e.action {
            SyncAction::UpToDate => "up-to-date",
            SyncAction::InstalledAndDeployed => "installed",
            SyncAction::Redeployed => "redeployed",
            SyncAction::DryRun => "[dry-run]",
            SyncAction::Skipped => "skipped",
        };
        println!(
            "  {:11} {:20} {} ({})",
            action, e.name, e.display, e.mode
        );
    }
    if !report.errors.is_empty() {
        println!();
        println!("errors:");
        for err in &report.errors {
            println!("  {}: {}", err.name, err.message);
        }
        anyhow::bail!("{} entry(ies) failed", report.errors.len());
    }
    Ok(())
}
