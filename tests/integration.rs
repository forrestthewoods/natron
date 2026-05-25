//! Integration tests. Most run on the default `cargo test` — each one
//! sets up a fresh `TestEnv`, builds synthetic archives under `fixtures/`,
//! and points entries at them via `file://` URLs.
//!
//! Tests that hit real upstream services (ziglang.org, GitHub, the
//! roblabla mirror) are individually marked `#[ignore]` with a reason
//! string. Standard runner flags opt them in:
//!
//!     cargo test                          # hermetic only (default)
//!     cargo test -- --ignored             # only network tests
//!     cargo test -- --include-ignored     # everything

mod common;

use common::{TestEnv, github_entry, url_entry, zig_entry};
use natron::{
    Cache, DeployMode, GithubProvider, Natron, ProviderRegistry, SyncAction, SyncOptions,
    ToolchainEntry, UrlProvider, ZigProvider,
};
use std::fs;
use std::path::Path;

// ============================================================================
// Hermetic tests — synthetic fixtures via file:// URLs.
// ============================================================================

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
    // Pre-place the fake GitHub release asset.
    env.place_github_asset(
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
fn test_install_zig_provider() {
    let env = TestEnv::new();
    // Synthetic zig archive shaped like the real one.
    let archive = env.make_zip(
        "zig-windows-x86_64-0.15.2.zip",
        &[
            ("zig-windows-x86_64-0.15.2/zig.exe", b"ZIG-BIN"),
            ("zig-windows-x86_64-0.15.2/lib/std.zig", b"STD-LIB"),
        ],
    );
    env.write_zig_index_json("0.15.2", "x86_64-windows", &archive);

    let cfg = env.build_config(vec![zig_entry("zig", "zig", "0.15.2", "x86_64-windows")]);
    let n = env.make_natron(cfg);
    let report = n.sync().unwrap();
    assert!(report.errors.is_empty(), "errors: {:?}", report.errors);
    let deploy = env.deploy_root().join("zig");
    // strip_prefix should have flattened the top-level dir.
    assert_eq!(fs::read(deploy.join("zig.exe")).unwrap(), b"ZIG-BIN");
    assert_eq!(fs::read(deploy.join("lib").join("std.zig")).unwrap(), b"STD-LIB");
}

#[test]
fn test_install_three_providers_at_once() {
    let env = TestEnv::new();
    let url_archive = env.make_zip("nasm.zip", &[("nasm.exe", b"NASM")]);
    let gh_archive = env.make_tar_xz("llvm.tar.xz", &[("clang", b"CLANG")]);
    let zig_archive = env.make_zip(
        "zig-windows-x86_64-0.15.2.zip",
        &[("zig-windows-x86_64-0.15.2/zig.exe", b"ZIG")],
    );
    env.place_github_asset(
        "llvm/llvm-project",
        "llvmorg-21",
        "llvm.tar.xz",
        &gh_archive,
    );
    env.write_zig_index_json("0.15.2", "x86_64-windows", &zig_archive);

    let cfg = env.build_config(vec![
        url_entry("nasm", "nasm", &url_archive),
        github_entry("llvm21", "llvm21", "llvm/llvm-project", "llvmorg-21", "llvm.tar.xz"),
        zig_entry("zig", "zig", "0.15.2", "x86_64-windows"),
    ]);
    let n = env.make_natron(cfg);
    let report = n.sync().unwrap();
    assert!(report.errors.is_empty(), "errors: {:?}", report.errors);
    assert_eq!(report.entries.len(), 3);
    assert!(env.deploy_root().join("nasm").join("nasm.exe").exists());
    assert!(env.deploy_root().join("llvm21").join("clang").exists());
    assert!(env.deploy_root().join("zig").join("zig.exe").exists());
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

// ============================================================================
// Network tests — real upstream services. All `#[ignore]`'d.
// ============================================================================

#[test]
#[ignore = "network: requires upstream access (cargo test -- --ignored)"]
fn test_real_zig_install() {
    let env = TestEnv::new();
    let cache = Cache::at(env.cache_dir.clone());
    let mut reg = ProviderRegistry::empty();
    reg.register(ZigProvider::new());

    let mut opts = toml::Table::new();
    opts.insert("version".into(), toml::Value::String("0.15.2".into()));
    opts.insert(
        "platform".into(),
        toml::Value::String(detect_zig_platform().into()),
    );
    let entry = ToolchainEntry {
        name: "zig".into(),
        deploy_dir: "zig".into(),
        provider: "zig".into(),
        deploy_mode: None,
        options: opts,
    };
    let cfg = env.build_config(vec![entry]);
    let n = Natron::new(cfg, cache, reg);
    let report = n.sync().expect("real zig install");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let zig_bin = env.deploy_root().join("zig");
    let any = std::fs::read_dir(&zig_bin).unwrap().next().is_some();
    assert!(any, "deployed zig dir should not be empty");
}

#[test]
#[ignore = "network: requires upstream access (cargo test -- --ignored)"]
fn test_real_url_nasm_install() {
    let env = TestEnv::new();
    let cache = Cache::at(env.cache_dir.clone());
    let mut reg = ProviderRegistry::empty();
    reg.register(UrlProvider::new());

    let mut opts = toml::Table::new();
    opts.insert(
        "url".into(),
        toml::Value::String(
            "https://www.nasm.us/pub/nasm/releasebuilds/3.01/win64/nasm-3.01-win64.zip"
                .into(),
        ),
    );
    opts.insert(
        "strip_prefix".into(),
        toml::Value::String("nasm-3.01".into()),
    );
    let entry = ToolchainEntry {
        name: "nasm".into(),
        deploy_dir: "nasm".into(),
        provider: "url".into(),
        deploy_mode: None,
        options: opts,
    };
    let cfg = env.build_config(vec![entry]);
    let n = Natron::new(cfg, cache, reg);
    let report = n.sync().expect("real nasm install");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
}

#[test]
#[ignore = "network: requires upstream access (cargo test -- --ignored)"]
fn test_real_github_llvm_install() {
    // Choose a smaller release if you want this test to run faster locally;
    // LLVM Windows asset is ~1.5 GB.
    let env = TestEnv::new();
    let cache = Cache::at(env.cache_dir.clone());
    let mut reg = ProviderRegistry::empty();
    reg.register(GithubProvider::new());

    let mut opts = toml::Table::new();
    opts.insert("repo".into(), toml::Value::String("llvm/llvm-project".into()));
    opts.insert("tag".into(), toml::Value::String("llvmorg-21.1.6".into()));
    opts.insert(
        "asset".into(),
        toml::Value::String(
            "clang+llvm-21.1.6-x86_64-pc-windows-msvc.tar.xz".into(),
        ),
    );
    opts.insert(
        "strip_prefix".into(),
        toml::Value::String("clang+llvm-21.1.6-x86_64-pc-windows-msvc".into()),
    );
    let entry = ToolchainEntry {
        name: "llvm21".into(),
        deploy_dir: "llvm21".into(),
        provider: "github".into(),
        deploy_mode: None,
        options: opts,
    };
    let cfg = env.build_config(vec![entry]);
    let n = Natron::new(cfg, cache, reg);
    let report = n.sync().expect("real github LLVM install");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
}

// Focused per-concern probes against the live roblabla mirror. Each one
// pinpoints a specific failure mode if it breaks: "commits API shape
// changed" vs "primary compiler heuristic missed vs2026" vs "SDK
// enumeration broke" vs "SDK MSI grouping broke".

#[test]
#[ignore = "network: requires upstream access (cargo test -- --ignored)"]
fn test_real_mirror_enumeration() {
    // Partial-clone the live mirror and confirm each release branch yields
    // at least one build. Exercises clone + `git log` + channel.json reads.
    use natron::providers::vs_manifest::{self, ManifestHistory, VsVersion};
    let env = TestEnv::new();
    let cache = Cache::at(env.cache_dir.clone());
    cache.ensure_layout().expect("cache layout");
    let history = ManifestHistory::open(&vs_manifest::default_remote(), &cache).expect("open");
    for vs in VsVersion::all() {
        let entries = history
            .index(&[vs])
            .unwrap_or_else(|e| panic!("index for {}: {e:#}", vs.as_str()));
        assert!(
            !entries.is_empty(),
            "{} has zero builds on the mirror",
            vs.as_str()
        );
    }
}

#[test]
#[ignore = "network: requires upstream access (cargo test -- --ignored)"]
fn test_real_msvc_primary_compiler() {
    // Newest vs2026 snapshot → find_primary_compiler returns a
    // Microsoft.VC.*.18.<minor>.Tools.HostX64.TargetX64.base id. If
    // Microsoft renames the family or changes the .18.<minor>. structure,
    // this catches it.
    use natron::providers::msvc;
    use natron::providers::vs_manifest::{self, ManifestHistory, VsVersion};
    let env = TestEnv::new();
    let cache = Cache::at(env.cache_dir.clone());
    cache.ensure_layout().expect("cache layout");
    let history = ManifestHistory::open(&vs_manifest::default_remote(), &cache).expect("open");

    let entries = history.index(&[VsVersion::Vs2026]).expect("index");
    let head_sha = &entries[0].commit.sha; // newest-first
    let manifest = history.manifest(head_sha).expect("manifest");
    let primary = msvc::find_primary_compiler(&manifest, VsVersion::Vs2026)
        .expect("primary compiler");
    assert!(
        primary.id.starts_with("Microsoft.VC.") && primary.id.contains(".18.")
            && primary.id.ends_with(".Tools.HostX64.TargetX64.base"),
        "unexpected primary id: {}",
        primary.id,
    );
}

#[test]
#[ignore = "network: requires upstream access (cargo test -- --ignored)"]
fn test_real_windows_sdk_versions() {
    use natron::providers::vs_manifest::{self, ManifestHistory};
    use natron::providers::windows_sdk;
    let env = TestEnv::new();
    let cache = Cache::at(env.cache_dir.clone());
    cache.ensure_layout().expect("cache layout");
    let history = ManifestHistory::open(&vs_manifest::default_remote(), &cache).expect("open");

    let versions = windows_sdk::discover_sdk_versions(&history).expect("discover");
    assert!(
        !versions.is_empty(),
        "no Windows SDK versions discovered on the mirror"
    );
}

#[test]
#[ignore = "network: requires upstream access (cargo test -- --ignored)"]
fn test_real_windows_sdk_packages() {
    // Resolve the newest SDK and confirm enumerate_msis returns at least
    // one default-installed MSI AND at least one extras-available MSI.
    use natron::providers::vs_manifest::{self, ManifestHistory};
    use natron::providers::windows_sdk;
    let env = TestEnv::new();
    let cache = Cache::at(env.cache_dir.clone());
    cache.ensure_layout().expect("cache layout");
    let history = ManifestHistory::open(&vs_manifest::default_remote(), &cache).expect("open");

    let versions = windows_sdk::discover_sdk_versions(&history).expect("discover");
    let newest = versions.first().expect("at least one SDK").clone();
    let resolved = windows_sdk::resolve_sdk_version(&history, &newest).expect("resolve");
    let msis = windows_sdk::enumerate_msis(&resolved.manifest, &resolved.sdk_pkg_id)
        .expect("enumerate");
    let default_count = msis.iter().filter(|(_, g)| g == "default").count();
    let extras_count = msis.iter().filter(|(_, g)| g == "extras").count();
    assert!(
        default_count > 0,
        "no default MSIs in SDK {newest} (have: {msis:?})"
    );
    assert!(
        extras_count > 0,
        "no extras MSIs in SDK {newest} (only default — unusual?)"
    );
}

// ============================================================================
// Helpers
// ============================================================================

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

fn detect_zig_platform() -> &'static str {
    if cfg!(target_os = "windows") && cfg!(target_arch = "x86_64") {
        "x86_64-windows"
    } else if cfg!(target_os = "linux") && cfg!(target_arch = "x86_64") {
        "x86_64-linux"
    } else if cfg!(target_os = "macos") && cfg!(target_arch = "aarch64") {
        "aarch64-macos"
    } else if cfg!(target_os = "macos") && cfg!(target_arch = "x86_64") {
        "x86_64-macos"
    } else {
        // Fall back; the test will fail at install time with a clear
        // "no entry" error from the zig provider.
        "x86_64-linux"
    }
}
