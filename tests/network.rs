//! Network-dependent integration tests. Gated behind `NATRON_NETWORK_TESTS=1`.
//! These hit real upstream services (ziglang.org, GitHub, aka.ms) and are
//! not run on every `cargo test` — they're for CI on a dedicated runner.
//!
//! Each test starts with a short-circuit if the env var isn't set, so they
//! show up as `ok` (skipped) in the default suite.

mod common;

use common::TestEnv;
use natron::{Cache, GithubProvider, Natron, ProviderRegistry, ToolchainEntry, UrlProvider, ZigProvider};

fn enabled() -> bool {
    std::env::var("NATRON_NETWORK_TESTS").is_ok()
}

#[test]
fn test_real_zig_install() {
    if !enabled() {
        return;
    }
    let env = TestEnv::new();
    // Use the real index URL, not a fixture.
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
fn test_real_url_nasm_install() {
    if !enabled() {
        return;
    }
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
fn test_real_github_llvm_install() {
    if !enabled() {
        return;
    }
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

#[test]
fn test_real_msvc_manifest_shape() {
    // Smoke test for the roblabla mirror — the sole upstream the msvc and
    // windows_sdk providers depend on. For each VS series: enumerate commits
    // via the GitHub API, fetch the HEAD `channel.json` (confirming the
    // info.buildVersion shape), and parse the HEAD `manifest.json` (confirming
    // the packages shape). Doesn't install anything.
    if !enabled() {
        return;
    }
    use natron::providers::vs_manifest::{
        self, MirrorUrls, VsVersion,
    };
    use natron::providers::InstallCtx;
    let env = TestEnv::new();
    let cache = Cache::at(env.cache_dir.clone());
    cache.ensure_layout().expect("cache layout");
    let ctx = InstallCtx::new(cache);
    let urls = MirrorUrls::default();

    for vs in VsVersion::all() {
        let commits = vs_manifest::fetch_commits(&urls.commits_base, vs)
            .unwrap_or_else(|e| panic!("commits for {}: {e:#}", vs.as_str()));
        assert!(!commits.is_empty(), "no commits for {}", vs.as_str());

        let head_sha = &commits[0].sha;
        let info = vs_manifest::fetch_channel_info(&urls.raw_base, head_sha, &ctx)
            .unwrap_or_else(|e| panic!("channel.json for {}@{head_sha}: {e:#}", vs.as_str()));
        assert!(
            !info.build_version.is_empty(),
            "{} HEAD has no build_version",
            vs.as_str()
        );

        let manifest = vs_manifest::fetch_manifest_at(&urls.raw_base, head_sha, &ctx)
            .unwrap_or_else(|e| panic!("manifest.json for {}@{head_sha}: {e:#}", vs.as_str()));
        assert!(
            !manifest.packages.is_empty(),
            "{} HEAD manifest has zero packages",
            vs.as_str()
        );
    }
}

fn detect_zig_platform() -> &'static str {
    // Zig's index.json keys are arch-os pairs.
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
