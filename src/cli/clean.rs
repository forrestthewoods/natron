//! `natron clean` subcommand.

use anyhow::Result;
use std::path::PathBuf;

use crate::cli::{CleanArgs, resolve_config_path};
use crate::fs_util;

pub fn run(
    config: &Option<PathBuf>,
    cache_dir_override: &Option<PathBuf>,
    args: CleanArgs,
) -> Result<()> {
    if !args.downloads && !args.all {
        anyhow::bail!("specify one of: --downloads, --all");
    }

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

    if args.all {
        if !args.yes {
            anyhow::bail!(
                "--all is destructive and removes the entire cache at {}; pass --yes to confirm",
                cache.root.display()
            );
        }
        println!("removing {}", cache.root.display());
        fs_util::remove_dir_all_writable(&cache.root)?;
        return Ok(());
    }

    if args.downloads {
        println!("emptying {}", cache.downloads.display());
        fs_util::remove_dir_all_writable(&cache.downloads).ok();
        std::fs::create_dir_all(&cache.downloads).ok();
    }
    Ok(())
}
