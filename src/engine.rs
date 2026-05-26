//! `Natron` orchestrator. Owns the config + cache + provider registry and
//! drives `sync()` (install + deploy + state update for every entry).

use anyhow::{Result, anyhow};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Mutex;
use std::time::SystemTime;

use crate::cache::{Cache, InstallMetadata, sanitize_fingerprint};
use crate::cas;
use crate::config::{Config, DeployMode, ToolchainEntry};
use crate::deploy;
use crate::fs_util;
use crate::providers::{InstallCtx, ProviderRegistry};
use crate::state::{DeployState, DeployedEntry, StateDiff};

/// Maximum age of a `staging/<uuid>/` dir, after which we GC it on `sync`
/// startup. 60 minutes covers slow LLVM downloads (~1.5 GB).
const STAGING_GC_THRESHOLD_SECS: u64 = 60 * 60;

/// Upper bound on toolchains processed concurrently. Each entry already uses a
/// core-sized worker pool internally (xz decode, CAB extraction, CAS), so this
/// is deliberately small: enough to overlap one entry's network/git phase with
/// another's CPU-bound decode, without multiplying the live thread count by the
/// toolchain count.
const MAX_CONCURRENT_TOOLCHAINS: usize = 4;

/// Toplevel orchestrator. Holds the parsed config, resolved cache layout, a
/// provider registry, and run-time options (dry-run, no-cas, mode override).
pub struct Natron {
    pub config: Config,
    pub cache: Cache,
    pub registry: ProviderRegistry,
    pub options: SyncOptions,
}

/// Options that affect a single `sync()` call. Set by the CLI; library
/// consumers can construct directly.
#[derive(Debug, Clone, Default)]
pub struct SyncOptions {
    pub dry_run: bool,
    pub no_cas: bool,
    /// Override the deploy mode for every entry this run.
    pub mode_override: Option<DeployMode>,
    /// If non-empty, only sync entries whose `name` is in this set.
    pub only: HashSet<String>,
    /// Keep the `downloads/` cache at the end of the run (currently we
    /// never auto-purge; this is a placeholder for future `--purge-downloads`
    /// or similar).
    #[allow(dead_code)]
    pub keep_downloads: bool,
}

/// Per-entry sync outcome, surfaced to the CLI for human-readable output.
#[derive(Debug, Clone)]
pub struct EntryOutcome {
    pub name: String,
    pub fingerprint: String,
    pub display: String,
    pub mode: DeployMode,
    pub action: SyncAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncAction {
    /// Cache hit + deploy already current; nothing was done.
    UpToDate,
    /// Provider extracted, engine committed to cache, then deployed.
    InstalledAndDeployed,
    /// Cache hit; deploy was redone (mode/target changed or deploy_dir
    /// missing).
    Redeployed,
    /// `--dry-run`: no changes were made.
    DryRun,
    /// Entry was skipped (e.g., `--only` filter excluded it).
    Skipped,
}

#[derive(Debug, Clone, Default)]
pub struct SyncReport {
    pub entries: Vec<EntryOutcome>,
    /// Errors encountered during the sync. `sync()` is best-effort: a
    /// failure on one entry is logged here and we continue to the next.
    pub errors: Vec<EntryError>,
}

#[derive(Debug, Clone)]
pub struct EntryError {
    pub name: String,
    pub message: String,
}

impl Natron {
    pub fn new(config: Config, cache: Cache, registry: ProviderRegistry) -> Self {
        Self {
            config,
            cache,
            registry,
            options: SyncOptions::default(),
        }
    }

    pub fn with_options(mut self, opts: SyncOptions) -> Self {
        self.options = opts;
        self
    }

    /// Build with default cache (from settings + platform default) and
    /// default provider registry.
    pub fn from_config(cfg: Config) -> Result<Self> {
        Self::from_config_with_registry(cfg, ProviderRegistry::default())
    }

    pub fn from_config_with_registry(
        cfg: Config,
        registry: ProviderRegistry,
    ) -> Result<Self> {
        let cache = Cache::resolve(None, cfg.settings.cache_dir.as_deref())?;
        Ok(Self::new(cfg, cache, registry))
    }

    /// Walk-upward + load. Convenience wrapper over `Config::load`.
    pub fn from_config_file(path: &Path) -> Result<Self> {
        let cfg = Config::load(path)?;
        Self::from_config(cfg)
    }

    /// Sync every entry in config (filtered by `options.only` if set).
    pub fn sync(&self) -> Result<SyncReport> {
        self.run(&self.options)
    }

    /// Sync one entry by name.
    pub fn sync_one(&self, name: &str) -> Result<SyncReport> {
        let mut opts = self.options.clone();
        opts.only.clear();
        opts.only.insert(name.to_string());
        self.run(&opts)
    }

    fn run(&self, opts: &SyncOptions) -> Result<SyncReport> {
        self.cache.ensure_layout()?;
        self.gc_stale_staging();

        let mut state = DeployState::read(&self.config.resolved_deploy_dir())?;

        // Entries in state but not in config: clean their deploy dirs.
        let in_config: HashSet<String> = self
            .config
            .toolchains
            .iter()
            .map(|t| t.name.clone())
            .collect();
        let stale_names: Vec<String> = state
            .deployed
            .keys()
            .filter(|k| !in_config.contains(*k))
            .cloned()
            .collect();
        for name in &stale_names {
            if let Some(existing) = state.get(name).cloned() {
                let dest = self
                    .config
                    .resolved_deploy_dir()
                    .join(&existing.deploy_dir);
                if !opts.dry_run {
                    deploy::undeploy(&dest).ok();
                    state.remove(name);
                }
                tracing::info!(
                    "removed stale deploy entry '{name}' (was at {})",
                    dest.display()
                );
            }
        }

        // Partition entries into per-position outcome slots (filtered entries
        // resolve immediately as Skipped) and a work list of the rest. Slots
        // keep the report in config order regardless of completion order.
        let mut report = SyncReport::default();
        let mut slots: Vec<Option<EntryOutcome>> = vec![None; self.config.toolchains.len()];
        let mut work: Vec<(usize, &ToolchainEntry)> = Vec::new();
        for (i, entry) in self.config.toolchains.iter().enumerate() {
            if !opts.only.is_empty() && !opts.only.contains(&entry.name) {
                slots[i] = Some(EntryOutcome {
                    name: entry.name.clone(),
                    fingerprint: String::new(),
                    display: String::new(),
                    mode: self.effective_mode(entry, opts),
                    action: SyncAction::Skipped,
                });
            } else {
                work.push((i, entry));
            }
        }

        // Run toolchains concurrently so one's download/git phase overlaps
        // another's CPU-bound decode. Each entry already saturates cores
        // internally (parallel xz / CAB / CAS), so we cap toolchain-level
        // concurrency low to overlap I/O without massively oversubscribing.
        let state = Mutex::new(state);
        let errors: Mutex<Vec<EntryError>> = Mutex::new(Vec::new());
        let outcomes: Mutex<Vec<(usize, EntryOutcome)>> = Mutex::new(Vec::new());
        let concurrency = MAX_CONCURRENT_TOOLCHAINS.min(work.len()).max(1);
        let queue: Mutex<Vec<usize>> = Mutex::new((0..work.len()).rev().collect());
        std::thread::scope(|s| {
            for _ in 0..concurrency {
                let queue = &queue;
                let work = &work;
                let state = &state;
                let errors = &errors;
                let outcomes = &outcomes;
                s.spawn(move || loop {
                    let wi = match queue.lock().unwrap().pop() {
                        Some(i) => i,
                        None => return,
                    };
                    let (idx, entry) = work[wi];
                    match self.sync_entry(entry, state, opts) {
                        Ok(outcome) => outcomes.lock().unwrap().push((idx, outcome)),
                        Err(err) => {
                            let msg = format!("{err:#}");
                            tracing::error!("entry '{}' failed: {msg}", entry.name);
                            errors.lock().unwrap().push(EntryError {
                                name: entry.name.clone(),
                                message: msg,
                            });
                        }
                    }
                });
            }
        });

        for (idx, outcome) in outcomes.into_inner().unwrap() {
            slots[idx] = Some(outcome);
        }
        report.entries = slots.into_iter().flatten().collect();
        report.errors = errors.into_inner().unwrap();
        let state = state.into_inner().unwrap();

        if !opts.dry_run {
            state.write(&self.config.resolved_deploy_dir())?;
        }

        if !report.errors.is_empty() {
            tracing::warn!(
                "{} of {} entries failed",
                report.errors.len(),
                report.entries.len() + report.errors.len()
            );
        }

        Ok(report)
    }

    fn effective_mode(&self, entry: &ToolchainEntry, opts: &SyncOptions) -> DeployMode {
        if let Some(m) = opts.mode_override {
            m
        } else {
            self.config.effective_mode(entry)
        }
    }

    fn sync_entry(
        &self,
        entry: &ToolchainEntry,
        state: &Mutex<DeployState>,
        opts: &SyncOptions,
    ) -> Result<EntryOutcome> {
        let provider = self.registry.require(&entry.provider)?;
        let mode = self.effective_mode(entry, opts);
        let mut ctx = InstallCtx::new(self.cache.clone());
        let t_install = std::time::Instant::now();
        let installed = provider.install(&entry.options, &mut ctx)?;
        tracing::info!(
            "[timing] {} provider.install (download+extract) took {:.2}s",
            entry.name,
            t_install.elapsed().as_secs_f64()
        );
        let raw_fp = installed.fingerprint;
        let fingerprint = sanitize_fingerprint(&raw_fp);

        if installed.freshly_extracted {
            let staging_root = ctx.staging_root().ok_or_else(|| {
                anyhow!(
                    "provider '{}' returned freshly_extracted=true but allocated no staging dir",
                    entry.provider
                )
            })?;
            let staging_raw = staging_root.join("raw");
            let staging_tree = staging_root.join("tree");

            let t_cas = std::time::Instant::now();
            let cas_report = if opts.no_cas {
                cas::run_no_cas(&staging_raw, &staging_tree)?
            } else {
                cas::run(&self.cache, &staging_raw, &staging_tree)?
            };
            let cas_elapsed = t_cas.elapsed().as_secs_f64();
            // files_processed already counts every regular file (dedupe hits
            // included); dedupe_hits is the subset that matched an existing
            // blob.
            let total_files = cas_report.files_processed;
            tracing::info!(
                "[timing] {} cas pass: {} files in {:.2}s ({:.0} files/s), dedupe_hits={} bytes_freed={}",
                entry.name,
                total_files,
                cas_elapsed,
                (total_files as f64) / cas_elapsed.max(0.0001),
                cas_report.dedupe_hits,
                cas_report.bytes_freed
            );

            // Note: we do NOT walk staging_tree to mark files readonly here.
            // CAS-managed files are marked readonly at insertion time inside
            // cas::run; --no-cas mode leaves files writable, which is the
            // correct semantic for that opt-out (FAT32 / cross-volume).
            let _ = std::fs::remove_dir_all(&staging_raw);

            let metadata = InstallMetadata::new(
                provider.id(),
                &fingerprint,
                &installed.display,
                installed.options.clone(),
            );
            let metadata_path = staging_root.join("metadata.toml");
            metadata.write(&metadata_path)?;

            let install_dir = self.cache.install_dir(&fingerprint);
            let renamed = fs_util::try_rename(&staging_root, &install_dir)?;
            if !renamed {
                tracing::debug!(
                    "concurrent-install collision on {}; using existing cache install",
                    fingerprint
                );
                fs_util::remove_dir_all_writable(&staging_root).ok();
            }
        }

        if !self.cache.install_present(&fingerprint) {
            anyhow::bail!(
                "cache install missing after provider returned: {}",
                self.cache.install_dir(&fingerprint).display()
            );
        }

        let deploy_root = self.config.resolved_deploy_dir();
        let deploy_path = deploy_root.join(&entry.deploy_dir);
        let install_tree = self.cache.install_tree(&fingerprint);

        let diff = state.lock().unwrap().diff(
            &entry.name,
            &fingerprint,
            &entry.deploy_dir,
            mode,
            &deploy_path,
        );

        if opts.dry_run {
            return Ok(EntryOutcome {
                name: entry.name.clone(),
                fingerprint,
                display: installed.display,
                mode,
                action: SyncAction::DryRun,
            });
        }

        if let StateDiff::Drift { old_deploy_dir } = &diff {
            if old_deploy_dir != &entry.deploy_dir {
                let old_path = deploy_root.join(old_deploy_dir);
                deploy::undeploy(&old_path).ok();
            }
        }

        let action = if diff.needs_redeploy() {
            let t_deploy = std::time::Instant::now();
            deploy::deploy(&install_tree, &deploy_path, mode)?;
            tracing::info!(
                "[timing] {} deploy ({}) took {:.2}s",
                entry.name,
                mode,
                t_deploy.elapsed().as_secs_f64()
            );
            state.lock().unwrap().upsert(
                entry.name.clone(),
                DeployedEntry {
                    fingerprint: fingerprint.clone(),
                    deploy_dir: entry.deploy_dir.clone(),
                    mode,
                    target: fs_util::slash_str(&install_tree),
                    deployed_at: now_datetime(),
                },
            );
            if installed.freshly_extracted {
                SyncAction::InstalledAndDeployed
            } else {
                SyncAction::Redeployed
            }
        } else {
            SyncAction::UpToDate
        };

        Ok(EntryOutcome {
            name: entry.name.clone(),
            fingerprint,
            display: installed.display,
            mode,
            action,
        })
    }

    fn gc_stale_staging(&self) {
        let now = SystemTime::now();
        let staging = &self.cache.staging;
        let Ok(entries) = std::fs::read_dir(staging) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let stale = match fs_util::latest_inside_mtime(&path) {
                Ok(latest) => match now.duration_since(latest) {
                    Ok(age) => age.as_secs() >= STAGING_GC_THRESHOLD_SECS,
                    Err(_) => false, // future mtime; leave alone
                },
                Err(_) => true,
            };
            if stale {
                tracing::info!("gc-ing stale staging dir {}", path.display());
                fs_util::remove_dir_all_writable(&path).ok();
            }
        }
    }
}

fn now_datetime() -> toml::value::Datetime {
    use std::time::UNIX_EPOCH;
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let s = format_datetime(secs);
    s.parse().unwrap_or_else(|_| "1970-01-01T00:00:00Z".parse().unwrap())
}

fn format_datetime(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let sod = (secs % 86_400) as u32;
    let h = sod / 3600;
    let m = (sod / 60) % 60;
    let s = sod % 60;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mm = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = (y + if mm <= 2 { 1 } else { 0 }) as i32;
    format!(
        "{year:04}-{mm:02}-{d:02}T{h:02}:{m:02}:{s:02}Z"
    )
}
#[cfg(test)]
#[path = "tests/engine.rs"]
mod tests;
