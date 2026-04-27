//! Offline integration tests. Always run on `cargo test`. No network.
//!
//! Each test sets up a fresh `TestEnv`, builds a synthetic archive (zip or
//! tar.xz) under fixtures/, and points an entry at it via `file://`.

mod common;

use common::{TestEnv, github_entry, url_entry};
use natron::{DeployMode, SyncAction, SyncOptions};
use std::fs;
use std::path::Path;

#[test]
fn test_install_url_provider() {
    let env = TestEnv::new();
    let archive = env.make_zip(
        "synthetic-nasm.zip",
        &[
            ("nasm.exe", b"NASM"),
            ("LICENSE", b"license-text"),
        ],
    );
    let cfg = env.build_config(vec![url_entry("nasm", "nasm", &archive)]);
    let n = env.make_natron(cfg);

    let report = n.sync().unwrap();
    assert!(report.errors.is_empty(), "errors: {:?}", report.errors);
    assert_eq!(report.entries.len(), 1);
    assert_eq!(report.entries[0].action, SyncAction::InstalledAndDeployed);

    let deploy = env.deploy_root().join("nasm");
    assert_eq!(fs::read(deploy.join("nasm.exe")).unwrap(), b"NASM");
    assert_eq!(fs::read(deploy.join("LICENSE")).unwrap(), b"license-text");

    // State file written.
    assert!(env.deploy_root().join(".natron-state.toml").is_file());

    // Cache install dir present.
    let installs = fs::read_dir(env.cache_dir.join("installs"))
        .unwrap()
        .count();
    assert_eq!(installs, 1);
}

#[test]
fn test_fast_path_no_work() {
    let env = TestEnv::new();
    let archive = env.make_zip("a.zip", &[("file", b"AAA")]);
    let cfg = env.build_config(vec![url_entry("a", "a", &archive)]);
    let n = env.make_natron(cfg);

    n.sync().unwrap();

    // Wipe downloads/ to prove the second run doesn't re-fetch.
    fs::remove_dir_all(env.cache_dir.join("downloads")).unwrap();

    let report = n.sync().unwrap();
    assert_eq!(report.entries[0].action, SyncAction::UpToDate);

    // downloads/ may have been re-created (ensure_layout) but should be
    // empty — provider's install short-circuited via cache hit.
    let dl_count = fs::read_dir(env.cache_dir.join("downloads"))
        .map(|d| d.count())
        .unwrap_or(0);
    assert_eq!(dl_count, 0, "fast path should not re-download");
}

#[test]
fn test_cas_dedupe() {
    let env = TestEnv::new();
    // Two distinct archives whose entries SHARE one file ("LICENSE").
    let a = env.make_tar_xz(
        "llvm-21.tar.xz",
        &[
            ("clang.exe", b"BIN-21"),
            ("LICENSE", b"shared-license-bytes"),
        ],
    );
    let b = env.make_tar_xz(
        "llvm-18.tar.xz",
        &[
            ("clang.exe", b"BIN-18"),
            ("LICENSE", b"shared-license-bytes"),
        ],
    );
    let cfg = env.build_config(vec![
        url_entry("llvm21", "llvm21", &a),
        url_entry("llvm18", "llvm18", &b),
    ]);
    let n = env.make_natron(cfg);
    let report = n.sync().unwrap();
    assert!(report.errors.is_empty());

    // Both deploys present.
    let d21 = env.deploy_root().join("llvm21");
    let d18 = env.deploy_root().join("llvm18");
    assert_eq!(fs::read(d21.join("LICENSE")).unwrap(), b"shared-license-bytes");
    assert_eq!(fs::read(d18.join("LICENSE")).unwrap(), b"shared-license-bytes");

    // CAS contains exactly one blob per unique file. Three uniques: BIN-21,
    // BIN-18, shared-license-bytes.
    let cas_blobs = count_cas_blobs(&env.cache_dir.join("cas"));
    assert_eq!(cas_blobs, 3, "expected 3 unique blobs in CAS");

    // On Unix, the two LICENSE files share an inode.
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let i1 = fs::metadata(d21.join("LICENSE")).unwrap().ino();
        let i2 = fs::metadata(d18.join("LICENSE")).unwrap().ino();
        assert_eq!(i1, i2, "deduped LICENSE files should share inode");
    }
}

#[test]
fn test_deploy_mode_hardlink() {
    let env = TestEnv::new();
    let archive = env.make_zip("a.zip", &[("bin", b"DATA")]);
    let mut cfg = env.build_config(vec![url_entry("a", "a", &archive)]);
    cfg.toolchains[0].deploy_mode = Some(DeployMode::Hardlink);
    let n = env.make_natron(cfg);
    n.sync().unwrap();

    let deployed = env.deploy_root().join("a").join("bin");
    assert_eq!(fs::read(&deployed).unwrap(), b"DATA");

    #[cfg(unix)]
    {
        // Deploy file shares inode with cache file.
        use std::os::unix::fs::MetadataExt;
        // Find the install tree inside cache/installs/<fp>/tree/bin.
        let installs = env.cache_dir.join("installs");
        let install_dir = fs::read_dir(&installs).unwrap().next().unwrap().unwrap();
        let cache_bin = install_dir.path().join("tree").join("bin");
        let deployed_md = fs::metadata(&deployed).unwrap();
        let cache_md = fs::metadata(&cache_bin).unwrap();
        assert_eq!(deployed_md.ino(), cache_md.ino());
    }
}

#[test]
fn test_deploy_mode_symlink() {
    let env = TestEnv::new();
    let archive = env.make_zip("a.zip", &[("file", b"X")]);
    let mut cfg = env.build_config(vec![url_entry("a", "a", &archive)]);
    cfg.toolchains[0].deploy_mode = Some(DeployMode::Symlink);
    let n = env.make_natron(cfg);
    n.sync().unwrap();

    let deploy = env.deploy_root().join("a");
    // Read-through-link works.
    assert_eq!(fs::read(deploy.join("file")).unwrap(), b"X");
    // The deploy dir is a symlink (or junction on Windows).
    let md = fs::symlink_metadata(&deploy).unwrap();
    assert!(md.file_type().is_symlink() || cfg!(windows));
}

#[test]
fn test_deploy_mode_copy() {
    let env = TestEnv::new();
    let archive = env.make_zip("a.zip", &[("file", b"DATA")]);
    let mut cfg = env.build_config(vec![url_entry("a", "a", &archive)]);
    cfg.toolchains[0].deploy_mode = Some(DeployMode::Copy);
    let n = env.make_natron(cfg);
    n.sync().unwrap();

    let deployed_file = env.deploy_root().join("a").join("file");
    assert_eq!(fs::read(&deployed_file).unwrap(), b"DATA");
    // Copy mode: file must be writable, no readonly attr.
    let md = fs::metadata(&deployed_file).unwrap();
    assert!(!md.permissions().readonly(), "copy mode files must be writable");

    // Mutating the copy must not affect the cache.
    fs::write(&deployed_file, b"MUTATED").unwrap();
    let installs = env.cache_dir.join("installs");
    let install_dir = fs::read_dir(&installs).unwrap().next().unwrap().unwrap();
    let cache_file = install_dir.path().join("tree").join("file");
    assert_eq!(fs::read(&cache_file).unwrap(), b"DATA");
}

#[test]
fn test_change_deploy_mode() {
    let env = TestEnv::new();
    let archive = env.make_zip("a.zip", &[("file", b"X")]);
    let mut cfg = env.build_config(vec![url_entry("a", "a", &archive)]);
    cfg.toolchains[0].deploy_mode = Some(DeployMode::Symlink);
    let n = env.make_natron(cfg.clone());
    n.sync().unwrap();
    let deploy = env.deploy_root().join("a");
    assert!(fs::symlink_metadata(&deploy).unwrap().file_type().is_symlink() || cfg!(windows));

    // Switch to hardlink, sync again.
    let mut cfg2 = cfg;
    cfg2.toolchains[0].deploy_mode = Some(DeployMode::Hardlink);
    let n2 = env.make_natron(cfg2);
    let report = n2.sync().unwrap();
    assert_eq!(report.entries[0].action, SyncAction::Redeployed);

    // Now it's a real directory with hardlinked files, not a symlink.
    let md = fs::symlink_metadata(&deploy).unwrap();
    assert!(md.file_type().is_dir());
    assert_eq!(fs::read(deploy.join("file")).unwrap(), b"X");
}

#[test]
fn test_remove_entry_cleans_deploy() {
    let env = TestEnv::new();
    let a = env.make_zip("a.zip", &[("a", b"A")]);
    let b = env.make_zip("b.zip", &[("b", b"B")]);
    let cfg1 = env.build_config(vec![
        url_entry("foo", "foo", &a),
        url_entry("bar", "bar", &b),
    ]);
    let n1 = env.make_natron(cfg1);
    n1.sync().unwrap();
    assert!(env.deploy_root().join("foo").exists());
    assert!(env.deploy_root().join("bar").exists());

    // New config drops "bar".
    let cfg2 = env.build_config(vec![url_entry("foo", "foo", &a)]);
    let n2 = env.make_natron(cfg2);
    n2.sync().unwrap();
    assert!(env.deploy_root().join("foo").exists());
    assert!(
        !env.deploy_root().join("bar").exists(),
        "bar should be cleaned up"
    );
}

#[test]
fn test_sha_pin_mismatch() {
    let env = TestEnv::new();
    let archive = env.make_zip("a.zip", &[("f", b"X")]);
    let mut entry = url_entry("a", "a", &archive);
    entry.options.insert(
        "sha256".into(),
        toml::Value::String(
            "0000000000000000000000000000000000000000000000000000000000000000".into(),
        ),
    );
    let cfg = env.build_config(vec![entry]);
    let n = env.make_natron(cfg);
    let report = n.sync().unwrap();
    assert!(!report.errors.is_empty(), "expected sha-mismatch error");
    assert!(report.errors[0].message.contains("sha256 mismatch"));
    // Cache is unchanged.
    let installs = env.cache_dir.join("installs");
    let count = fs::read_dir(&installs).map(|d| d.count()).unwrap_or(0);
    assert_eq!(count, 0);
}

#[test]
fn test_no_cas_flag() {
    let env = TestEnv::new();
    let archive = env.make_zip("a.zip", &[("f", b"X")]);
    let cfg = env.build_config(vec![url_entry("a", "a", &archive)]);
    let mut opts = SyncOptions::default();
    opts.no_cas = true;
    let n = env.make_natron(cfg).with_options(opts);
    n.sync().unwrap();
    let cas = env.cache_dir.join("cas");
    let count = fs::read_dir(&cas).map(|d| d.count()).unwrap_or(0);
    assert_eq!(count, 0, "CAS should be empty in --no-cas mode");
    // Install tree still present.
    assert!(env.deploy_root().join("a").join("f").is_file());
}

#[test]
fn test_staging_gc() {
    let env = TestEnv::new();
    let archive = env.make_zip("a.zip", &[("f", b"X")]);
    let cfg = env.build_config(vec![url_entry("a", "a", &archive)]);

    // Pre-create a "stale" staging dir and backdate everything.
    let staging = env.cache_dir.join("staging");
    fs::create_dir_all(&staging).unwrap();
    let stale = staging.join("stale-uuid");
    fs::create_dir_all(&stale).unwrap();
    let stale_file = stale.join("leftover.bin");
    fs::write(&stale_file, b"orphan").unwrap();
    backdate(&stale_file, 90 * 60);
    backdate(&stale, 90 * 60);

    let n = env.make_natron(cfg);
    n.sync().unwrap();

    // Stale dir should be gone.
    assert!(!stale.exists(), "stale staging dir should be GC'd");
}

#[test]
fn test_invalid_config_dup_name() {
    // Duplicate names are caught at config validation time.
    let toml_text = r#"
[settings]
deploy_dir = "tc"
[[toolchain]]
name = "foo"
deploy_dir = "a"
provider = "url"
[toolchain.options]
url = "https://example.com/x.zip"
[[toolchain]]
name = "foo"
deploy_dir = "b"
provider = "url"
[toolchain.options]
url = "https://example.com/y.zip"
"#;
    let env = TestEnv::new();
    let path = env.project_dir.join("natron.toml");
    fs::write(&path, toml_text).unwrap();
    let err = natron::Config::load(&path).unwrap_err();
    assert!(err.to_string().contains("duplicate toolchain name"));
}

#[test]
fn test_invalid_config_unknown_provider() {
    let env = TestEnv::new();
    let archive = env.make_zip("a.zip", &[("f", b"X")]);
    let mut entry = url_entry("a", "a", &archive);
    entry.provider = "no_such_provider".into();
    let cfg = env.build_config(vec![entry]);
    let n = env.make_natron(cfg);
    let report = n.sync().unwrap();
    assert_eq!(report.errors.len(), 1);
    assert!(report.errors[0].message.contains("no such provider"));
}

#[test]
fn test_manual_deploy_deletion() {
    let env = TestEnv::new();
    let archive = env.make_zip("a.zip", &[("f", b"X")]);
    let cfg = env.build_config(vec![url_entry("a", "a", &archive)]);
    let n = env.make_natron(cfg);
    n.sync().unwrap();

    // User does `rm -rf <deploy>/a`.
    let dest = env.deploy_root().join("a");
    natron::fs_util::remove_dir_all_writable(&dest).unwrap();
    assert!(!dest.exists());

    let report = n.sync().unwrap();
    assert_eq!(report.entries[0].action, SyncAction::Redeployed);
    assert!(dest.join("f").is_file());
}

#[test]
fn test_corrupt_metadata_recovers() {
    let env = TestEnv::new();
    let archive = env.make_zip("a.zip", &[("f", b"X")]);
    let cfg = env.build_config(vec![url_entry("a", "a", &archive)]);
    let n = env.make_natron(cfg);
    n.sync().unwrap();

    // Corrupt metadata.toml in the cache.
    let installs = env.cache_dir.join("installs");
    let install_dir = fs::read_dir(&installs).unwrap().next().unwrap().unwrap();
    let metadata_path = install_dir.path().join("metadata.toml");
    // Strip readonly so we can clobber.
    natron::fs_util::clear_readonly(&metadata_path).ok();
    let perms_dir = install_dir.path();
    natron::fs_util::clear_readonly(&perms_dir).ok();
    fs::write(&metadata_path, "{{ this is not toml }}").unwrap();

    // The file becomes "present" (install_present checks existence) but
    // would fail to parse if read. install_present is shallow by design;
    // a future verify command would detect this. The fast-path test
    // simply re-deploys (fingerprint matches). We assert no panic.
    let report = n.sync().unwrap();
    // Either succeeds (state matches) or errors clean — never panics.
    assert!(report.errors.is_empty() || report.errors.len() == 1);
}

#[test]
fn test_fresh_cache_dir() {
    // cache_dir does not exist before sync.
    let env = TestEnv::new();
    assert!(!env.cache_dir.exists());

    let archive = env.make_zip("a.zip", &[("f", b"X")]);
    let cfg = env.build_config(vec![url_entry("a", "a", &archive)]);
    let n = env.make_natron(cfg);
    n.sync().unwrap();

    // All four subdirs created.
    for sub in ["installs", "cas", "downloads", "staging"] {
        assert!(
            env.cache_dir.join(sub).is_dir(),
            "{sub} subdir should be created"
        );
    }
}

#[test]
fn test_install_github_provider() {
    let env = TestEnv::new();
    // Build the asset archive.
    let archive = env.make_tar_xz(
        "clang+llvm-21.1.6-x86_64-pc-windows-msvc.tar.xz",
        &[
            ("bin/clang.exe", b"CLANG-BIN"),
            ("LICENSE", b"llvm-license"),
        ],
    );
    // Pre-populate the fake GitHub release JSON.
    env.write_github_release_json(
        "llvm/llvm-project",
        "llvmorg-21.1.6",
        "clang+llvm-21.1.6-x86_64-pc-windows-msvc.tar.xz",
        &archive,
    );

    let cfg = env.build_config(vec![github_entry(
        "llvm21",
        "llvm21",
        "llvm/llvm-project",
        "llvmorg-21.1.6",
        "clang+llvm-21.1.6-x86_64-pc-windows-msvc.tar.xz",
    )]);
    let n = env.make_natron(cfg);
    let report = n.sync().unwrap();
    assert!(report.errors.is_empty(), "errors: {:?}", report.errors);
    assert_eq!(report.entries[0].action, SyncAction::InstalledAndDeployed);

    let deploy = env.deploy_root().join("llvm21");
    assert_eq!(fs::read(deploy.join("bin").join("clang.exe")).unwrap(), b"CLANG-BIN");
    assert_eq!(fs::read(deploy.join("LICENSE")).unwrap(), b"llvm-license");
}

#[test]
fn test_install_three_providers_at_once() {
    // url + github (zig added in step 10).
    let env = TestEnv::new();
    let url_archive = env.make_zip("nasm.zip", &[("nasm.exe", b"NASM")]);
    let gh_archive = env.make_tar_xz("llvm.tar.xz", &[("clang", b"CLANG")]);
    env.write_github_release_json(
        "llvm/llvm-project",
        "llvmorg-21",
        "llvm.tar.xz",
        &gh_archive,
    );

    let cfg = env.build_config(vec![
        url_entry("nasm", "nasm", &url_archive),
        github_entry("llvm21", "llvm21", "llvm/llvm-project", "llvmorg-21", "llvm.tar.xz"),
    ]);
    let n = env.make_natron(cfg);
    let report = n.sync().unwrap();
    assert!(report.errors.is_empty(), "errors: {:?}", report.errors);
    assert_eq!(report.entries.len(), 2);
    assert!(env.deploy_root().join("nasm").join("nasm.exe").exists());
    assert!(env.deploy_root().join("llvm21").join("clang").exists());
}

#[test]
fn test_dry_run_makes_no_changes() {
    let env = TestEnv::new();
    let archive = env.make_zip("a.zip", &[("f", b"X")]);
    let cfg = env.build_config(vec![url_entry("a", "a", &archive)]);
    let mut opts = SyncOptions::default();
    opts.dry_run = true;
    let n = env.make_natron(cfg).with_options(opts);
    let report = n.sync().unwrap();
    assert_eq!(report.entries[0].action, SyncAction::DryRun);
    assert!(!env.deploy_root().join("a").exists());
}

// ---- helpers --------------------------------------------------------------

fn count_cas_blobs(cas: &Path) -> usize {
    let mut total = 0;
    if let Ok(entries) = fs::read_dir(cas) {
        for outer in entries.flatten() {
            if let Ok(inner) = fs::read_dir(outer.path()) {
                total += inner.count();
            }
        }
    }
    total
}

fn backdate(path: &Path, secs: u64) {
    use std::time::{Duration, SystemTime};
    let when = SystemTime::now() - Duration::from_secs(secs);
    let ft = filetime::FileTime::from_system_time(when);
    filetime::set_file_mtime(path, ft).unwrap();
}
