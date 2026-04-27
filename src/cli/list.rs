//! `natron list` subcommand.
//!
//! With no flags, prints both project state (what's deployed in this
//! project) and global cache state (every install in <cache>/installs/),
//! along with the resolved cache + deploy paths so users can find them.
//!
//! `--project` / `--cache` restrict to one section.

use anyhow::Result;
use std::path::PathBuf;

use crate::cli::{ListArgs, resolve_config_path};

pub fn run(
    config: &Option<PathBuf>,
    cache_dir_override: &Option<PathBuf>,
    args: ListArgs,
) -> Result<()> {
    // No flags = show both. Either flag = show only that section.
    let show_project = args.project || (!args.project && !args.cache);
    let show_cache = args.cache || (!args.project && !args.cache);

    let cfg = resolve_config_path(config)
        .ok()
        .and_then(|p| crate::config::Config::load(&p).ok());
    let cfg_cache_setting = cfg.as_ref().and_then(|c| c.settings.cache_dir.clone());
    let cache = crate::cache::Cache::resolve(
        cache_dir_override.as_deref(),
        cfg_cache_setting.as_deref(),
    )?;

    if show_project {
        list_project(cfg.as_ref(), &cache)?;
    }
    if show_project && show_cache {
        println!();
    }
    if show_cache {
        list_cache(&cache)?;
    }
    Ok(())
}

fn list_project(
    cfg: Option<&crate::config::Config>,
    cache: &crate::cache::Cache,
) -> Result<()> {
    let Some(cfg) = cfg else {
        println!("== project ==");
        println!("  (no natron.toml found; pass --config <path> to point at one)");
        println!("  cache root: {}", cache.root.display());
        return Ok(());
    };
    let deploy_dir = cfg.resolved_deploy_dir();
    println!("== project ==");
    println!("  config:     {}", cfg.config_dir.join("natron.toml").display());
    println!("  deploy_dir: {}", deploy_dir.display());
    println!("  cache root: {}", cache.root.display());
    let state = match crate::state::DeployState::read(&deploy_dir) {
        Ok(s) => s,
        Err(err) => {
            println!("  (could not read state: {err})");
            return Ok(());
        }
    };
    if state.deployed.is_empty() {
        println!("  no toolchains deployed");
    } else {
        println!("  deployed:");
        for (name, e) in &state.deployed {
            println!(
                "    {:20} {:10} {:10} -> {}",
                name, e.mode, e.deploy_dir, e.target
            );
        }
    }
    Ok(())
}

fn list_cache(cache: &crate::cache::Cache) -> Result<()> {
    println!("== cache ==");
    println!("  root:      {}", cache.root.display());
    println!("  installs:  {}", cache.installs.display());
    println!("  cas:       {}", cache.cas.display());
    println!("  downloads: {}", cache.downloads.display());
    let entries = match std::fs::read_dir(&cache.installs) {
        Ok(it) => it,
        Err(_) => {
            println!("  (no installs)");
            return Ok(());
        }
    };
    let mut rows: Vec<(String, String, String)> = Vec::new();
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let metadata_path = dir.join("metadata.toml");
        if let Ok(md) = crate::cache::InstallMetadata::read(&metadata_path) {
            rows.push((
                dir.file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                md.display,
                md.provider,
            ));
        }
    }
    if rows.is_empty() {
        println!("  (no installs)");
    } else {
        println!("  installs ({}):", rows.len());
        for (fp, display, provider) in &rows {
            println!("    {:60} {} ({})", fp, display, provider);
        }
    }
    Ok(())
}
