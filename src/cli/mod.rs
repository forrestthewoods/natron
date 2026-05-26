//! CLI for natron. Wired up by `main.rs`.

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

use crate::config::DeployMode;

pub mod clean;
pub mod install;
pub mod list;
pub mod msvc;
pub mod windows_sdk;

#[derive(Debug, Parser)]
#[command(name = "natron", version, about = "Vendor compiler toolchains into source-controlled projects")]
pub struct Cli {
    /// Path to natron.toml. If omitted, search upward from cwd.
    #[arg(global = true, short = 'c', long = "config")]
    pub config: Option<PathBuf>,

    /// Override `[settings] cache_dir`.
    #[arg(global = true, long = "cache-dir")]
    pub cache_dir: Option<PathBuf>,

    /// Increase log verbosity (-v info, -vv debug, -vvv trace).
    #[arg(global = true, short = 'v', long, action = clap::ArgAction::Count)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Install + deploy every entry from natron.toml.
    Install(InstallArgs),
    /// List installed/deployed toolchains.
    List(ListArgs),
    /// Clean cached files.
    Clean(CleanArgs),
    /// MSVC debug + discovery tooling.
    Msvc(msvc::MsvcArgs),
    /// Windows SDK debug + discovery tooling.
    #[command(name = "windows_sdk")]
    WindowsSdk(windows_sdk::WindowsSdkArgs),
}

#[derive(Debug, Args)]
pub struct InstallArgs {
    /// Resolve fingerprints + print the diff against current state, but
    /// perform zero extraction or deploy mutation.
    #[arg(long)]
    pub dry_run: bool,

    /// Only sync these toolchain `name`s (repeatable).
    #[arg(long = "only", value_name = "NAME")]
    pub only: Vec<String>,

    /// Override deploy_mode for this run only. Does not modify config or
    /// state. Wins over both global default and per-toolchain
    /// `deploy_mode`.
    #[arg(long = "mode", value_name = "MODE")]
    pub mode: Option<String>,
}

impl InstallArgs {
    pub fn parse_mode(&self) -> Result<Option<DeployMode>> {
        match self.mode.as_deref() {
            None => Ok(None),
            Some("symlink") => Ok(Some(DeployMode::Symlink)),
            Some("copy") => Ok(Some(DeployMode::Copy)),
            Some(other) => anyhow::bail!(
                "unknown --mode '{other}' (expected: symlink | copy)"
            ),
        }
    }
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Show toolchains deployed in this project (reads .natron-state.toml).
    /// Default if neither flag is given.
    #[arg(long)]
    pub project: bool,

    /// Show every install in the global cache (walks <cache>/installs/).
    #[arg(long)]
    pub cache: bool,
}

#[derive(Debug, Args)]
pub struct CleanArgs {
    /// Empty `<cache>/downloads/`.
    #[arg(long)]
    pub downloads: bool,

    /// Wipe the entire cache directory (requires --yes).
    #[arg(long)]
    pub all: bool,

    /// Confirm a destructive operation.
    #[arg(long)]
    pub yes: bool,
}

/// Initialize tracing-subscriber with verbosity from `cli.verbose`. Idempotent.
pub fn init_logging(verbose: u8) -> Result<()> {
    use tracing_subscriber::{EnvFilter, fmt};
    let level = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("natron={level}")));
    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init()
        .ok(); // ok if already initialized (e.g., from tests)
    Ok(())
}

/// Top-level dispatch.
pub fn run(cli: Cli) -> Result<()> {
    init_logging(cli.verbose).ok();
    let command = cli.command.unwrap_or(Command::Install(InstallArgs {
        dry_run: false,
        only: Vec::new(),
        mode: None,
    }));
    match command {
        Command::Install(args) => install::run(&cli.config, &cli.cache_dir, args),
        Command::List(args) => list::run(&cli.config, &cli.cache_dir, args),
        Command::Clean(args) => clean::run(&cli.config, &cli.cache_dir, args),
        Command::Msvc(args) => msvc::run(&cli.config, &cli.cache_dir, args),
        Command::WindowsSdk(args) => windows_sdk::run(&cli.config, &cli.cache_dir, args),
    }
}

/// Resolve the config path: explicit `--config` if given, else walk upward
/// from cwd looking for `natron.toml`.
pub fn resolve_config_path(explicit: &Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p.clone());
    }
    let cwd = std::env::current_dir().context("getting current dir")?;
    Ok(cwd)
}
