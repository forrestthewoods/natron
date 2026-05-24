//! Network-dependent integration tests. Each test is `#[ignore]`'d so the
//! default `cargo test` run stays hermetic. To exercise:
//!
//!     cargo test -- --ignored            # only the network tests
//!     cargo test -- --include-ignored    # full suite, hermetic + network
//!
//! Tests hit real upstream services (ziglang.org, GitHub, the roblabla
//! mirror) — they're for CI on a dedicated runner or for human spot checks.

mod common;

use common::TestEnv;
use natron::{Cache, GithubProvider, Natron, ProviderRegistry, ToolchainEntry, UrlProvider, ZigProvider};

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

// ---- MSVC / windows_sdk mirror tests ---------------------------------------
//
// Focused per-concern probes against the live roblabla mirror. Each one
// pinpoints a specific failure mode if it breaks: "commits API shape
// changed" vs "primary compiler heuristic missed vs2026" vs "SDK
// enumeration broke" vs "SDK MSI grouping broke".

#[test]
#[ignore = "network: requires upstream access (cargo test -- --ignored)"]
fn test_real_mirror_commits_api() {
    use natron::providers::vs_manifest::{self, MirrorUrls, VsVersion};
    let urls = MirrorUrls::default();
    for vs in VsVersion::all() {
        let commits = vs_manifest::fetch_commits(&urls.commits_base, vs)
            .unwrap_or_else(|e| panic!("commits for {}: {e:#}", vs.as_str()));
        assert!(
            !commits.is_empty(),
            "{} has zero commits on the mirror",
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
    use natron::providers::vs_manifest::{self, MirrorUrls, VsVersion};
    use natron::providers::InstallCtx;
    let env = TestEnv::new();
    let cache = Cache::at(env.cache_dir.clone());
    cache.ensure_layout().expect("cache layout");
    let ctx = InstallCtx::new(cache);
    let urls = MirrorUrls::default();

    let commits = vs_manifest::fetch_commits(&urls.commits_base, VsVersion::Vs2026)
        .expect("commits");
    let head_sha = &commits[0].sha;
    let manifest = vs_manifest::fetch_manifest_at(&urls.raw_base, head_sha, &ctx)
        .expect("manifest");
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
    use natron::providers::vs_manifest::MirrorUrls;
    use natron::providers::windows_sdk;
    use natron::providers::InstallCtx;
    let env = TestEnv::new();
    let cache = Cache::at(env.cache_dir.clone());
    cache.ensure_layout().expect("cache layout");
    let ctx = InstallCtx::new(cache);
    let urls = MirrorUrls::default();

    let versions = windows_sdk::discover_sdk_versions(&urls, &ctx).expect("discover");
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
    use natron::providers::vs_manifest::MirrorUrls;
    use natron::providers::windows_sdk;
    use natron::providers::InstallCtx;
    let env = TestEnv::new();
    let cache = Cache::at(env.cache_dir.clone());
    cache.ensure_layout().expect("cache layout");
    let ctx = InstallCtx::new(cache);
    let urls = MirrorUrls::default();

    let versions = windows_sdk::discover_sdk_versions(&urls, &ctx).expect("discover");
    let newest = versions.first().expect("at least one SDK").clone();
    let resolved =
        windows_sdk::resolve_sdk_version(&urls, &newest, &ctx).expect("resolve");
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
