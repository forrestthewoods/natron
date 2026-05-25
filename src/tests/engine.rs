//! Tests for `src/engine.rs` (split out so the production
//! file shows only the implementation).

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
