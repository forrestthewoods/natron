//! `natron list` subcommand. Two modes: `--project` (read
//! `.natron-state.toml`) and `--cache` (walk `<cache>/installs/`).

use anyhow::Result;
use std::path::PathBuf;

use crate::cli::{ListArgs, resolve_config_path};

pub fn run(
    config: &Option<PathBuf>,
    cache_dir_override: &Option<PathBuf>,
    args: ListArgs,
) -> Result<()> {
    // If no flag given, default to --project.
    let show_project = args.project || !args.cache;
    let show_cache = args.cache;

    if show_project {
        list_project(config)?;
    }
    if show_cache {
        list_cache(config, cache_dir_override)?;
    }
    Ok(())
}

fn list_project(config: &Option<PathBuf>) -> Result<()> {
    let cfg_path = resolve_config_path(config)?;
    let cfg = match crate::config::Config::load(&cfg_path) {
        Ok(c) => c,
        Err(err) => {
            println!("(no config: {err})");
            return Ok(());
        }
    };
    let deploy_dir = cfg.resolved_deploy_dir();
    let state = match crate::state::DeployState::read(&deploy_dir) {
        Ok(s) => s,
        Err(err) => {
            println!("(could not read state: {err})");
            return Ok(());
        }
    };
    println!("== project ({}) ==", deploy_dir.display());
    if state.deployed.is_empty() {
        println!("  no toolchains deployed");
    } else {
        for (name, e) in &state.deployed {
            println!(
                "  {:20} {:10} {:10} -> {}",
                name, e.mode, e.deploy_dir, e.target
            );
        }
    }
    Ok(())
}

fn list_cache(
    config: &Option<PathBuf>,
    cache_dir_override: &Option<PathBuf>,
) -> Result<()> {
    // We need to know cache_dir. Try to read config's cache_dir; fall back
    // to the platform default.
    let cfg_setting = match resolve_config_path(config) {
        Ok(p) => crate::config::Config::load(&p)
            .ok()
            .and_then(|c| c.settings.cache_dir.clone()),
        Err(_) => None,
    };
    let cache = crate::cache::Cache::resolve(
        cache_dir_override.as_deref(),
        cfg_setting.as_deref(),
    )?;
    println!("== cache ({}) ==", cache.installs.display());
    let entries = match std::fs::read_dir(&cache.installs) {
        Ok(it) => it,
        Err(_) => {
            println!("  (no installs)");
            return Ok(());
        }
    };
    let mut any = false;
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let metadata_path = dir.join("metadata.toml");
        if let Ok(md) = crate::cache::InstallMetadata::read(&metadata_path) {
            println!(
                "  {:50} {} ({})",
                dir.file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                md.display,
                md.provider,
            );
            any = true;
        }
    }
    if !any {
        println!("  (no installs)");
    }
    Ok(())
}
