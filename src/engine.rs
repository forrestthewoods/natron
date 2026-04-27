//! `Natron` orchestrator. Owns the config + cache + provider registry and
//! drives `sync()` (install + deploy + state update for every entry).

use anyhow::{Result, anyhow};
use std::collections::HashSet;
use std::path::Path;
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

        let mut report = SyncReport::default();
        for entry in &self.config.toolchains {
            if !opts.only.is_empty() && !opts.only.contains(&entry.name) {
                report.entries.push(EntryOutcome {
                    name: entry.name.clone(),
                    fingerprint: String::new(),
                    display: String::new(),
                    mode: self.effective_mode(entry, opts),
                    action: SyncAction::Skipped,
                });
                continue;
            }
            match self.sync_entry(entry, &mut state, opts) {
                Ok(outcome) => report.entries.push(outcome),
                Err(err) => {
                    let msg = format!("{err:#}");
                    tracing::error!("entry '{}' failed: {msg}", entry.name);
                    report.errors.push(EntryError {
                        name: entry.name.clone(),
                        message: msg,
                    });
                }
            }
        }

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
        state: &mut DeployState,
        opts: &SyncOptions,
    ) -> Result<EntryOutcome> {
        let provider = self.registry.require(&entry.provider)?;
        let mode = self.effective_mode(entry, opts);
        let mut ctx = InstallCtx::new(self.cache.clone());
        let installed = provider.install(&entry.options, &mut ctx)?;
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

            let cas_report = if opts.no_cas {
                cas::run_no_cas(&staging_raw, &staging_tree)?
            } else {
                cas::run(&self.cache, &staging_raw, &staging_tree)?
            };
            tracing::debug!(
                "cas pass: files={} dedupe_hits={} bytes_freed={}",
                cas_report.files_processed,
                cas_report.dedupe_hits,
                cas_report.bytes_freed
            );

            fs_util::set_readonly_recursive(&staging_tree)?;
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

        let diff = state.diff(
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
            deploy::deploy(&install_tree, &deploy_path, mode)?;
            state.upsert(
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
mod tests {
    use super::*;
    use crate::config::Settings;
    use crate::providers::{Installed, Provider};
    use tempfile::TempDir;

    /// Test-only provider that writes a fixed file tree into staging.
    struct StubProvider {
        files: Vec<(&'static str, &'static [u8])>,
        fingerprint_seed: &'static str,
    }

    impl Provider for StubProvider {
        fn id(&self) -> &'static str {
            "stub"
        }
        fn install(&self, _options: &toml::Table, ctx: &mut InstallCtx) -> Result<Installed> {
            let fp = format!("stub-{}", self.fingerprint_seed);
            // Cache hit fast path
            if ctx.cache().install_present(&fp) {
                return Ok(Installed {
                    fingerprint: fp,
                    display: format!("stub({})", self.fingerprint_seed),
                    options: toml::Table::new(),
                    freshly_extracted: false,
                });
            }
            let raw = ctx.staging_dir()?;
            for (path, bytes) in &self.files {
                let p = raw.join(path);
                if let Some(parent) = p.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&p, bytes)?;
            }
            Ok(Installed {
                fingerprint: fp,
                display: format!("stub({})", self.fingerprint_seed),
                options: toml::Table::new(),
                freshly_extracted: true,
            })
        }
    }

    fn build_config(deploy_dir: &Path, entries: Vec<ToolchainEntry>) -> Config {
        Config {
            settings: Settings {
                deploy_dir: deploy_dir.to_path_buf(),
                ..Default::default()
            },
            toolchains: entries,
            config_dir: deploy_dir.parent().unwrap_or(deploy_dir).to_path_buf(),
        }
    }

    fn entry(name: &str, deploy_dir: &str) -> ToolchainEntry {
        ToolchainEntry {
            name: name.into(),
            deploy_dir: deploy_dir.into(),
            provider: "stub".into(),
            deploy_mode: None,
            options: toml::Table::new(),
        }
    }

    #[test]
    fn sync_installs_and_deploys() {
        let tmp = TempDir::new().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let deploy_dir = project.join("toolchains");
        let cfg = build_config(&deploy_dir, vec![entry("foo", "foo")]);
        let cache = Cache::at(tmp.path().join("cache"));
        let mut reg = ProviderRegistry::empty();
        reg.register(StubProvider {
            files: vec![("bin/clang", b"BIN"), ("LICENSE", b"LIC")],
            fingerprint_seed: "v1",
        });
        let n = Natron::new(cfg, cache, reg);
        let report = n.sync().unwrap();
        assert!(report.errors.is_empty(), "errors: {:?}", report.errors);
        assert_eq!(report.entries.len(), 1);
        assert_eq!(report.entries[0].action, SyncAction::InstalledAndDeployed);

        let deploy_path = deploy_dir.join("foo");
        assert_eq!(std::fs::read(deploy_path.join("LICENSE")).unwrap(), b"LIC");
        assert!(n.cache.install_present("stub-v1"));
    }

    #[test]
    fn sync_fast_path_when_cache_hit_and_deploy_intact() {
        let tmp = TempDir::new().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let deploy_dir = project.join("toolchains");
        let cfg = build_config(&deploy_dir, vec![entry("foo", "foo")]);
        let cache = Cache::at(tmp.path().join("cache"));
        let mut reg = ProviderRegistry::empty();
        reg.register(StubProvider {
            files: vec![("LICENSE", b"LIC")],
            fingerprint_seed: "v1",
        });
        let n = Natron::new(cfg, cache, reg);
        n.sync().unwrap();
        let report = n.sync().unwrap();
        assert_eq!(report.entries.len(), 1);
        assert_eq!(report.entries[0].action, SyncAction::UpToDate);
    }

    #[test]
    fn sync_redeploys_after_manual_deletion() {
        let tmp = TempDir::new().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let deploy_dir = project.join("toolchains");
        let cfg = build_config(&deploy_dir, vec![entry("foo", "foo")]);
        let cache = Cache::at(tmp.path().join("cache"));
        let mut reg = ProviderRegistry::empty();
        reg.register(StubProvider {
            files: vec![("LICENSE", b"LIC")],
            fingerprint_seed: "v1",
        });
        let n = Natron::new(cfg, cache, reg);
        n.sync().unwrap();

        // User manually deletes the deploy dir.
        fs_util::remove_dir_all_writable(&deploy_dir.join("foo")).unwrap();

        let report = n.sync().unwrap();
        assert_eq!(report.entries[0].action, SyncAction::Redeployed);
        assert!(deploy_dir.join("foo").join("LICENSE").is_file());
    }

    #[test]
    fn sync_cleans_removed_entries() {
        let tmp = TempDir::new().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let deploy_dir = project.join("toolchains");
        let cache = Cache::at(tmp.path().join("cache"));
        let mut reg = ProviderRegistry::empty();
        reg.register(StubProvider {
            files: vec![("LICENSE", b"LIC")],
            fingerprint_seed: "v1",
        });
        let cfg1 = build_config(
            &deploy_dir,
            vec![entry("foo", "foo"), entry("bar", "bar")],
        );
        let n1 = Natron::new(cfg1, cache.clone(), reg);
        n1.sync().unwrap();
        assert!(deploy_dir.join("foo").exists());
        assert!(deploy_dir.join("bar").exists());

        // New config drops "bar".
        let mut reg2 = ProviderRegistry::empty();
        reg2.register(StubProvider {
            files: vec![("LICENSE", b"LIC")],
            fingerprint_seed: "v1",
        });
        let cfg2 = build_config(&deploy_dir, vec![entry("foo", "foo")]);
        let n2 = Natron::new(cfg2, cache, reg2);
        n2.sync().unwrap();
        assert!(deploy_dir.join("foo").exists());
        assert!(!deploy_dir.join("bar").exists(), "bar should be cleaned up");
    }

    #[test]
    fn sync_dry_run_makes_no_changes() {
        let tmp = TempDir::new().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let deploy_dir = project.join("toolchains");
        let cfg = build_config(&deploy_dir, vec![entry("foo", "foo")]);
        let cache = Cache::at(tmp.path().join("cache"));
        let mut reg = ProviderRegistry::empty();
        reg.register(StubProvider {
            files: vec![("LICENSE", b"LIC")],
            fingerprint_seed: "v1",
        });
        let mut opts = SyncOptions::default();
        opts.dry_run = true;
        let n = Natron::new(cfg, cache, reg).with_options(opts);
        let report = n.sync().unwrap();
        assert_eq!(report.entries[0].action, SyncAction::DryRun);
        // No deploy dir should have been created.
        assert!(!deploy_dir.join("foo").exists());
    }

    #[test]
    fn sync_only_filters_entries() {
        let tmp = TempDir::new().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let deploy_dir = project.join("toolchains");
        let cache = Cache::at(tmp.path().join("cache"));
        let mut reg = ProviderRegistry::empty();
        reg.register(StubProvider {
            files: vec![("L", b"L")],
            fingerprint_seed: "v1",
        });
        let cfg = build_config(
            &deploy_dir,
            vec![entry("foo", "foo"), entry("bar", "bar")],
        );
        let mut opts = SyncOptions::default();
        opts.only.insert("foo".into());
        let n = Natron::new(cfg, cache, reg).with_options(opts);
        let report = n.sync().unwrap();
        assert_eq!(report.entries.len(), 2);
        assert_eq!(report.entries[0].action, SyncAction::InstalledAndDeployed);
        assert_eq!(report.entries[1].action, SyncAction::Skipped);
    }

    #[test]
    fn sync_handles_unknown_provider_gracefully() {
        let tmp = TempDir::new().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let deploy_dir = project.join("toolchains");
        let mut e = entry("foo", "foo");
        e.provider = "no_such_provider".into();
        let cfg = build_config(&deploy_dir, vec![e]);
        let cache = Cache::at(tmp.path().join("cache"));
        let n = Natron::new(cfg, cache, ProviderRegistry::empty());
        let report = n.sync().unwrap();
        assert_eq!(report.errors.len(), 1);
        assert!(report.errors[0].message.contains("no such provider"));
    }

    #[test]
    fn sync_no_cas_skips_dedupe() {
        let tmp = TempDir::new().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let deploy_dir = project.join("toolchains");
        let cfg = build_config(&deploy_dir, vec![entry("foo", "foo")]);
        let cache = Cache::at(tmp.path().join("cache"));
        let mut reg = ProviderRegistry::empty();
        reg.register(StubProvider {
            files: vec![("LICENSE", b"LIC")],
            fingerprint_seed: "v1",
        });
        let mut opts = SyncOptions::default();
        opts.no_cas = true;
        let n = Natron::new(cfg, cache.clone(), reg).with_options(opts);
        n.sync().unwrap();
        // CAS should be empty (no entries).
        let cas_entries: Vec<_> = std::fs::read_dir(&cache.cas).unwrap().collect();
        assert!(cas_entries.is_empty(), "CAS should be empty in --no-cas mode");
    }

    #[test]
    fn sync_redeploys_on_mode_change() {
        let tmp = TempDir::new().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let deploy_dir = project.join("toolchains");
        let cache = Cache::at(tmp.path().join("cache"));
        let mut reg1 = ProviderRegistry::empty();
        reg1.register(StubProvider {
            files: vec![("LICENSE", b"LIC")],
            fingerprint_seed: "v1",
        });

        let cfg = build_config(&deploy_dir, vec![entry("foo", "foo")]);
        let n = Natron::new(cfg.clone(), cache.clone(), reg1);
        n.sync().unwrap();

        // Re-sync with mode_override = Copy.
        let mut reg2 = ProviderRegistry::empty();
        reg2.register(StubProvider {
            files: vec![("LICENSE", b"LIC")],
            fingerprint_seed: "v1",
        });
        let mut opts = SyncOptions::default();
        opts.mode_override = Some(DeployMode::Copy);
        let n2 = Natron::new(cfg, cache, reg2).with_options(opts);
        let report = n2.sync().unwrap();
        assert_eq!(report.entries[0].action, SyncAction::Redeployed);
        assert_eq!(report.entries[0].mode, DeployMode::Copy);
    }

    #[test]
    fn sync_concurrent_threads_both_succeed() {
        // Two threads trying to install the same fingerprint. Both should
        // succeed, the cache should have exactly one install dir, and the
        // loser silently dropped its staging.
        let tmp = TempDir::new().unwrap();
        let cache_root = tmp.path().join("cache");
        let project_a = tmp.path().join("a");
        let project_b = tmp.path().join("b");
        std::fs::create_dir_all(&project_a).unwrap();
        std::fs::create_dir_all(&project_b).unwrap();
        let deploy_a = project_a.join("toolchains");
        let deploy_b = project_b.join("toolchains");

        let h1 = std::thread::spawn({
            let cache_root = cache_root.clone();
            let deploy_a = deploy_a.clone();
            move || {
                let cfg = build_config(&deploy_a, vec![entry("foo", "foo")]);
                let cache = Cache::at(cache_root);
                let mut reg = ProviderRegistry::empty();
                reg.register(StubProvider {
                    files: vec![("LICENSE", b"LIC")],
                    fingerprint_seed: "v1",
                });
                Natron::new(cfg, cache, reg).sync()
            }
        });
        let h2 = std::thread::spawn({
            let cache_root = cache_root.clone();
            let deploy_b = deploy_b.clone();
            move || {
                let cfg = build_config(&deploy_b, vec![entry("foo", "foo")]);
                let cache = Cache::at(cache_root);
                let mut reg = ProviderRegistry::empty();
                reg.register(StubProvider {
                    files: vec![("LICENSE", b"LIC")],
                    fingerprint_seed: "v1",
                });
                Natron::new(cfg, cache, reg).sync()
            }
        });
        let r1 = h1.join().unwrap().unwrap();
        let r2 = h2.join().unwrap().unwrap();
        assert!(r1.errors.is_empty(), "{:?}", r1.errors);
        assert!(r2.errors.is_empty(), "{:?}", r2.errors);

        // Cache must have exactly one install dir.
        let installs_dir = cache_root.join("installs");
        let installs: Vec<_> = std::fs::read_dir(&installs_dir).unwrap().collect();
        assert_eq!(installs.len(), 1, "expected one install dir");

        // Both deploys must be present.
        assert!(deploy_a.join("foo").join("LICENSE").is_file());
        assert!(deploy_b.join("foo").join("LICENSE").is_file());
    }
}
