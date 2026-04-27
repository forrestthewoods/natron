//! End-to-end demo: vendor a realistic Windows C/C++ toolchain set into
//! `examples/anubis_demo/toolchains/` using the natron LIBRARY API.
//! Output persists so you can `ls` the deploy tree and inspect what
//! landed where.
//!
//! This mirrors how Anubis (or any other consumer) would eventually
//! integrate natron: build a `Natron` from a `natron.toml` co-located
//! with the project, call `sync()`, point your build at the deployed
//! paths.
//!
//! Run:
//!   cargo run --release --example anubis_demo
//!
//! First run downloads ~3-5 GB. Subsequent runs hit the shared cache at
//! `~/.natron/` (default, shared across all projects on the machine) and
//! finish in seconds.

use std::path::{Path, PathBuf};

use natron::{Natron, SyncAction};

fn main() -> anyhow::Result<()> {
    init_logging();

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let demo_dir = manifest_dir.join("examples").join("anubis_demo");
    let cfg_path = demo_dir.join("natron.toml");
    if !cfg_path.is_file() {
        anyhow::bail!("missing {}", cfg_path.display());
    }

    let n = Natron::from_config_file(&cfg_path)?;
    let deploy_dir = n.config.resolved_deploy_dir();

    println!("== natron anubis_demo ==");
    println!("config:     {}", cfg_path.display());
    println!("cache root: {}", n.cache.root.display());
    println!("deploy_dir: {}", deploy_dir.display());
    println!();

    let report = n.sync()?;

    println!();
    println!("== sync outcomes ==");
    for entry in &report.entries {
        let action = match entry.action {
            SyncAction::UpToDate => "up-to-date",
            SyncAction::InstalledAndDeployed => "installed",
            SyncAction::Redeployed => "redeployed",
            SyncAction::DryRun => "[dry-run]",
            SyncAction::Skipped => "skipped",
        };
        println!(
            "  {action:11} {:14} {} ({})",
            entry.name, entry.display, entry.mode
        );
    }
    if !report.errors.is_empty() {
        println!();
        println!("ERRORS:");
        for e in &report.errors {
            println!("  {}: {}", e.name, e.message);
        }
        anyhow::bail!("{} entry(ies) failed", report.errors.len());
    }

    print_deploy_tree(&deploy_dir);
    print_known_binaries(&deploy_dir);

    println!();
    println!("Done. Inspect: {}", deploy_dir.display());
    Ok(())
}

fn init_logging() {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("natron=info"));
    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init()
        .ok();
}

/// Print the immediate children of the deploy_dir so the user can see the
/// shape at a glance (one line per [[toolchain]] entry).
fn print_deploy_tree(deploy_dir: &Path) {
    println!();
    println!("== deploy tree ==");
    let Ok(entries) = std::fs::read_dir(deploy_dir) else {
        println!("  (deploy dir does not exist)");
        return;
    };
    let mut rows: Vec<_> = entries.flatten().collect();
    rows.sort_by_key(|e| e.file_name());
    for entry in rows {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            // Skip the .natron-state.toml line in the tree view.
            continue;
        }
        let kind = entry
            .file_type()
            .ok()
            .map(|ft| {
                if ft.is_symlink() {
                    "symlink"
                } else if ft.is_dir() {
                    "dir"
                } else {
                    "file"
                }
            })
            .unwrap_or("?");
        let count = if kind == "dir" {
            std::fs::read_dir(entry.path())
                .map(|it| it.count())
                .unwrap_or(0)
        } else {
            0
        };
        if kind == "dir" {
            println!("  {name:14} ({kind}, {count} top-level entries)");
        } else {
            println!("  {name:14} ({kind})");
        }
    }
}

/// Probe for well-known executables in each vendored toolchain. Doesn't run
/// them — just confirms they exist where expected. Lets the user
/// eyeball-confirm that hardlinks/symlinks into the deploy tree resolve.
fn print_known_binaries(deploy_dir: &Path) {
    println!();
    println!("== well-known binaries ==");
    let probes: &[(&str, &str)] = &[
        ("llvm21", "bin/clang.exe"),
        ("zig",    "zig.exe"),
        ("nasm",   "nasm.exe"),
        // MSVC's compiler is buried under VC/Tools/MSVC/<ver>/. The version
        // dir name varies, so we glob.
        ("msvc",         "VC/Tools/MSVC"),
        ("windows_sdk",  "Include"),
    ];
    for (entry_name, rel) in probes {
        let path = deploy_dir.join(entry_name).join(rel);
        let exists = path.exists();
        let mark = if exists { "OK" } else { "MISSING" };
        println!("  {mark:8} {}", path.display());
    }
}
