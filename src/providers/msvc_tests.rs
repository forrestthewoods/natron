//! Tests for `src/providers\msvc.rs` (split out so the production
//! file shows only the implementation).

use super::*;
use crate::cache::Cache;
use crate::providers::InstallCtx;
use std::io::Write;
use tempfile::TempDir;

#[test]
fn msvc_provider_id() {
    assert_eq!(MsvcProvider::new().id(), "msvc");
}

#[test]
fn msvc_provider_requires_vs() {
    let tmp = TempDir::new().unwrap();
    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let mut ctx = InstallCtx::new(cache);
    let opts = toml::Table::new();
    let err = MsvcProvider::new().install(&opts, &mut ctx).unwrap_err();
    assert!(err.to_string().contains("options.vs"));
}

#[test]
fn rejects_unknown_vs() {
    let mut opts = toml::Table::new();
    opts.insert("vs".into(), toml::Value::String("release18".into()));

    let err = MsvcSelection::from_options(&opts).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("release18"), "got: {msg}");
    assert!(msg.contains("vs2019"), "got: {msg}");
}

#[test]
fn vs_names_map_to_channels() {
    assert_eq!(VsVersion::parse("vs2019").unwrap().channel(), "16");
    assert_eq!(VsVersion::parse("vs2022").unwrap().channel(), "17");
    assert_eq!(VsVersion::parse("vs2026").unwrap().channel(), "18");
}

#[test]
fn profile_defaults_to_standard() {
    let selection = MsvcSelection::from_options(&opts_with_vs()).unwrap();
    assert_eq!(selection.profile, MsvcProfile::Standard);
}

#[test]
fn rejects_unknown_profile() {
    let mut opts = opts_with_vs();
    opts.insert("profile".into(), toml::Value::String("minimal".into()));

    let err = MsvcSelection::from_options(&opts).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("minimal"), "got: {msg}");
    assert!(msg.contains("standard"), "got: {msg}");
}

#[test]
fn custom_requires_include() {
    let mut opts = opts_with_vs();
    opts.insert("profile".into(), toml::Value::String("custom".into()));

    let err = MsvcSelection::from_options(&opts).unwrap_err();
    assert!(err.to_string().contains("requires a non-empty 'include'"));
}

#[test]
fn standard_rejects_include() {
    let mut opts = opts_with_vs();
    opts.insert("include".into(), toml_array(&["Tools.*"]));

    let err = MsvcSelection::from_options(&opts).unwrap_err();
    assert!(err.to_string().contains("standard"));
    assert!(err.to_string().contains("include"));
}

#[test]
fn custom_rejects_extras() {
    let mut opts = opts_with_vs();
    opts.insert("profile".into(), toml::Value::String("custom".into()));
    opts.insert("include".into(), toml_array(&["Tools.*"]));
    opts.insert("extras".into(), toml_array(&["CRT.*"]));

    let err = MsvcSelection::from_options(&opts).unwrap_err();
    assert!(err.to_string().contains("extras"));
}

#[test]
fn full_rejects_include_and_extras() {
    let mut opts = opts_with_vs();
    opts.insert("profile".into(), toml::Value::String("full".into()));
    opts.insert("include".into(), toml_array(&["Tools.*"]));

    let err = MsvcSelection::from_options(&opts).unwrap_err();
    assert!(err.to_string().contains("full"));
    assert!(err.to_string().contains("include"));
}

#[test]
fn rejects_empty_patterns() {
    let mut opts = opts_with_vs();
    opts.insert("extras".into(), toml_array(&[""]));

    let err = MsvcSelection::from_options(&opts).unwrap_err();
    assert!(err.to_string().contains("empty"));
}

#[test]
fn derives_family_prefix_from_modern_compiler_package() {
    assert_eq!(
        family_prefix_from_compiler_package("Microsoft.VC.14.52.Tools.HostX64.TargetX64.base")
            .unwrap(),
        "Microsoft.VC.14.52."
    );
}

#[test]
fn derives_family_prefix_for_dotted_families() {
    assert_eq!(
        family_prefix_from_compiler_package("Microsoft.VC.14.39.17.9.Tools.HostX64.TargetX64.base")
            .unwrap(),
        "Microsoft.VC.14.39.17.9."
    );
}

#[test]
fn rejects_non_modern_compiler_anchor() {
    let err = family_prefix_from_compiler_package(
        "Microsoft.VisualCpp.Tools.HostX64.TargetX64.14.16.base",
    )
    .unwrap_err();
    assert!(err
        .to_string()
        .contains("unsupported MSVC compiler package id"));
}

#[test]
fn full_selects_only_resolved_family_prefix() {
    let resolved = resolved_from_manifest(
        r#"{
            "packages": [
                {"id": "Microsoft.VC.14.52.Tools.HostX64.TargetX64.base", "version": "14.52.36328", "payloads": []},
                {"id": "Microsoft.VC.14.52.CRT.Headers.Resources.base", "version": "14.52.36328", "language": "en-US", "payloads": []},
                {"id": "Microsoft.VC.14.52.CRT.Headers.Resources.base", "version": "14.52.36328", "language": "ja-JP", "payloads": []},
                {"id": "Microsoft.VC.14.51.Tools.HostX64.TargetX64.base", "version": "14.51.36243", "payloads": []},
                {"id": "Microsoft.VC.Preview.DIA.SDK", "version": "14.52.36328", "payloads": []}
            ]
        }"#,
        "14.52",
        "14.52.36328",
    );
    let mut opts = opts_with_vs();
    opts.insert("profile".into(), toml::Value::String("full".into()));
    let selection = MsvcSelection::from_options(&opts).unwrap();

    let selected = select_msvc_packages(&resolved, &selection).unwrap();
    let ids = package_ids(&selected);

    assert_eq!(
        ids,
        vec![
            "Microsoft.VC.14.52.CRT.Headers.Resources.base:en-US",
            "Microsoft.VC.14.52.CRT.Headers.Resources.base:ja-JP",
            "Microsoft.VC.14.52.Tools.HostX64.TargetX64.base",
        ]
    );
}

#[test]
fn selection_respects_resolved_exact_package_version() {
    let resolved = resolved_from_manifest(
        r#"{
            "packages": [
                {"id": "Microsoft.VC.14.52.Tools.HostX64.TargetX64.base", "version": "14.52.36328", "payloads": []},
                {"id": "Microsoft.VC.14.52.CRT.Headers.base", "version": "14.52.36328", "payloads": []},
                {"id": "Microsoft.VC.14.52.CRT.Headers.base", "version": "14.52.99999", "payloads": []},
                {"id": "Microsoft.VC.Preview.DIA.SDK", "version": "14.52.36328", "payloads": []},
                {"id": "Microsoft.VC.Preview.DIA.SDK", "version": "14.52.99999", "payloads": []}
            ]
        }"#,
        "14.52",
        "14.52.36328",
    );
    let mut opts = opts_with_vs();
    opts.insert("profile".into(), toml::Value::String("custom".into()));
    opts.insert(
        "include".into(),
        toml_array(&["CRT.Headers.base", "Microsoft.VC.Preview.DIA.SDK"]),
    );
    let selection = MsvcSelection::from_options(&opts).unwrap();

    let selected = select_msvc_packages(&resolved, &selection).unwrap();

    assert_eq!(
        package_ids(&selected),
        vec![
            "Microsoft.VC.14.52.CRT.Headers.base",
            "Microsoft.VC.Preview.DIA.SDK",
        ]
    );
}

#[test]
fn standard_selects_builtin_patterns_and_all_resource_locales() {
    let resolved = standard_resolved_toolset();
    let selection = MsvcSelection::from_options(&opts_with_vs()).unwrap();

    let selected = select_msvc_packages(&resolved, &selection).unwrap();
    let ids = package_ids(&selected);

    assert_eq!(
        ids,
        vec![
            "Microsoft.VC.14.52.CRT.Headers.base",
            "Microsoft.VC.14.52.CRT.Redist.X64.base",
            "Microsoft.VC.14.52.CRT.x64.Desktop.base",
            "Microsoft.VC.14.52.CRT.x64.Store.base",
            "Microsoft.VC.14.52.Tools.HostX64.TargetX64.Res.base:en-US",
            "Microsoft.VC.14.52.Tools.HostX64.TargetX64.Res.base:ja-JP",
            "Microsoft.VC.14.52.Tools.HostX64.TargetX64.base",
        ]
    );
}

#[test]
fn extras_match_family_suffix_patterns() {
    let resolved = standard_resolved_toolset();
    let mut opts = opts_with_vs();
    opts.insert("extras".into(), toml_array(&["ATL.*.base"]));
    let selection = MsvcSelection::from_options(&opts).unwrap();

    let selected = select_msvc_packages(&resolved, &selection).unwrap();
    let ids = package_ids(&selected);

    assert!(ids.contains(&"Microsoft.VC.14.52.ATL.X64.base".to_string()));
}

#[test]
fn raw_patterns_match_raw_ids_in_exact_version() {
    let resolved = standard_resolved_toolset();
    let mut opts = opts_with_vs();
    opts.insert("extras".into(), toml_array(&["Microsoft.VC.Preview.DIA.*"]));
    let selection = MsvcSelection::from_options(&opts).unwrap();

    let selected = select_msvc_packages(&resolved, &selection).unwrap();
    let ids = package_ids(&selected);

    assert!(ids.contains(&"Microsoft.VC.Preview.DIA.SDK".to_string()));
    assert!(!ids.contains(&"Microsoft.VC.Preview.DIA.Old".to_string()));
}

#[test]
fn raw_pattern_prefix_is_case_insensitive() {
    let resolved = standard_resolved_toolset();
    let mut opts = opts_with_vs();
    opts.insert("extras".into(), toml_array(&["microsoft.vc.preview.dia.*"]));
    let selection = MsvcSelection::from_options(&opts).unwrap();

    let selected = select_msvc_packages(&resolved, &selection).unwrap();
    let ids = package_ids(&selected);

    assert!(ids.contains(&"Microsoft.VC.Preview.DIA.SDK".to_string()));
    assert!(!ids.contains(&"Microsoft.VC.Preview.DIA.Old".to_string()));
}

#[test]
fn pattern_zero_match_errors() {
    let resolved = standard_resolved_toolset();
    let mut opts = opts_with_vs();
    opts.insert("extras".into(), toml_array(&["Definitely.Not.Real.*"]));
    let selection = MsvcSelection::from_options(&opts).unwrap();

    let err = select_msvc_packages(&resolved, &selection).unwrap_err();
    assert!(err.to_string().contains("matched no packages"));
}

#[test]
fn pattern_matching_is_case_insensitive() {
    let resolved = standard_resolved_toolset();
    let mut opts = opts_with_vs();
    opts.insert("profile".into(), toml::Value::String("custom".into()));
    opts.insert(
        "include".into(),
        toml_array(&["tools.hostx64.targetx64.base"]),
    );
    let selection = MsvcSelection::from_options(&opts).unwrap();

    let selected = select_msvc_packages(&resolved, &selection).unwrap();

    assert_eq!(
        package_ids(&selected),
        vec!["Microsoft.VC.14.52.Tools.HostX64.TargetX64.base"]
    );
}

#[test]
fn dependency_closure_adds_resource_props_and_servicing() {
    let resolved = resolved_from_manifest(
        r#"{
            "packages": [
                {
                    "id": "Microsoft.VC.14.52.Tools.HostX64.TargetX64.base",
                    "version": "14.52.36328",
                    "payloads": [],
                    "dependencies": {
                        "Microsoft.VC.14.52.Tools.HostX64.TargetX64.Res.base": "14.52.36328",
                        "Microsoft.VC.14.52.Props.x64": "14.52.36328",
                        "Microsoft.VC.14.52.Servicing.Compilers": "14.52.36328",
                        "Microsoft.VC.14.52.ATL.X64.base": "14.52.36328"
                    }
                },
                {"id": "Microsoft.VC.14.52.Tools.HostX64.TargetX64.Res.base", "version": "14.52.36328", "language": "en-US", "payloads": []},
                {"id": "Microsoft.VC.14.52.Props.x64", "version": "14.52.36328", "payloads": []},
                {"id": "Microsoft.VC.14.52.Servicing.Compilers", "version": "14.52.36328", "payloads": []},
                {"id": "Microsoft.VC.14.52.ATL.X64.base", "version": "14.52.36328", "payloads": []}
            ]
        }"#,
        "14.52",
        "14.52.36328",
    );
    let mut opts = opts_with_vs();
    opts.insert("profile".into(), toml::Value::String("custom".into()));
    opts.insert(
        "include".into(),
        toml_array(&["Tools.HostX64.TargetX64.base"]),
    );
    let selection = MsvcSelection::from_options(&opts).unwrap();

    let selected = select_msvc_packages(&resolved, &selection).unwrap();
    let ids = package_ids(&selected);

    assert!(ids.contains(&"Microsoft.VC.14.52.Tools.HostX64.TargetX64.Res.base:en-US".to_string()));
    assert!(ids.contains(&"Microsoft.VC.14.52.Props.x64".to_string()));
    assert!(ids.contains(&"Microsoft.VC.14.52.Servicing.Compilers".to_string()));
    assert!(!ids.contains(&"Microsoft.VC.14.52.ATL.X64.base".to_string()));
}

#[test]
fn languageless_request_prefers_languageless_manifest_entry() {
    // Manifest lists a ja-JP variant BEFORE the languageless variant. A
    // request with language=None must still pick the languageless one,
    // not blindly take the first id+version match.
    let manifest = parse_msvc_manifest(
        r#"{
            "packages": [
                {"id": "Microsoft.VC.14.52.Foo.base", "version": "14.52.36328", "language": "ja-JP", "payloads": []},
                {"id": "Microsoft.VC.14.52.Foo.base", "version": "14.52.36328", "payloads": []}
            ]
        }"#,
    );
    let request = PackageRequest {
        id: "Microsoft.VC.14.52.Foo.base".into(),
        version: Some("14.52.36328".into()),
        language: None,
    };

    let found = find_requested_package(&manifest, &request).unwrap().unwrap();

    assert!(found.language.is_none(), "got: {:?}", found.language);
}

#[test]
fn fingerprint_changes_when_normalized_selection_changes() {
    let standard = MsvcSelection::from_options(&opts_with_vs()).unwrap();
    let mut extra_opts = opts_with_vs();
    extra_opts.insert("extras".into(), toml_array(&["ATL.X64.base"]));
    let with_extra = MsvcSelection::from_options(&extra_opts).unwrap();

    assert_ne!(
        msvc_fingerprint("14.52.36328", &standard),
        msvc_fingerprint("14.52.36328", &with_extra)
    );
}

#[test]
fn pinned_version_cache_hit_short_circuits_before_manifest_fetch() {
    let tmp = TempDir::new().unwrap();
    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();

    let mut opts = opts_with_vs();
    opts.insert(
        "msvc_version".into(),
        toml::Value::String("14.51.36243".into()),
    );
    let selection = MsvcSelection::from_options(&opts).unwrap();
    let fp = msvc_fingerprint("14.51.36243", &selection);
    let install_dir = cache.install_dir(&fp);
    std::fs::create_dir_all(install_dir.join("tree")).unwrap();
    let md = crate::cache::InstallMetadata::new(
        "msvc",
        fp.clone(),
        "msvc 14.51.36243 (vs2026)",
        resolved_options(&opts, "14.51.36243", &selection),
    );
    md.write(&cache.install_metadata_path(&fp)).unwrap();

    let mut ctx = InstallCtx::new(cache);
    let provider = MsvcProvider::with_channel_url_template("file:///never/exists/{channel}")
        .with_archive_manifest_url_template("file:///archive/also/missing.json");

    let installed = provider.install(&opts, &mut ctx).unwrap();

    assert!(!installed.freshly_extracted);
    assert_eq!(installed.fingerprint, fp);
    assert!(ctx.staging_root().is_none());
}

#[test]
fn pinned_version_resolves_from_live_manifest_first() {
    let tmp = TempDir::new().unwrap();
    let live_manifest = tmp.path().join("live.vsman");
    write_msvc_manifest(
        &live_manifest,
        &[("14.50.18.0", "14.50.35731"), ("14.39.17.9", "14.39.33523")],
    );
    let live_channel = tmp.path().join("channel.json");
    write_channel_manifest(&live_channel, &live_manifest);

    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let ctx = InstallCtx::new(cache);
    let provider = MsvcProvider::with_channel_url_template(file_url(&live_channel))
        .with_archive_manifest_url_template("file:///archive/should/not/be/read.json");

    let resolved = provider
        .resolve_toolset("17", Some("14.39.33523"), &ctx)
        .unwrap();

    assert_eq!(resolved.package_version, "14.39.33523");
    assert_eq!(resolved.family_prefix, "Microsoft.VC.14.39.17.9.");
}

#[test]
fn pinned_version_falls_back_to_archive_manifest() {
    let tmp = TempDir::new().unwrap();
    let live_manifest = tmp.path().join("live.vsman");
    write_msvc_manifest(&live_manifest, &[("14.50.18.0", "14.50.35731")]);
    let live_channel = tmp.path().join("channel.json");
    write_channel_manifest(&live_channel, &live_manifest);
    let archive_manifest = tmp.path().join("archive.vsman");
    write_msvc_manifest(&archive_manifest, &[("14.39.17.9", "14.39.33523")]);

    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let ctx = InstallCtx::new(cache);
    let provider = MsvcProvider::with_channel_url_template(file_url(&live_channel))
        .with_archive_manifest_url_template(file_url(&archive_manifest));

    let resolved = provider
        .resolve_toolset("17", Some("14.39.33523"), &ctx)
        .unwrap();

    assert_eq!(resolved.package_version, "14.39.33523");
    assert_eq!(resolved.family_prefix, "Microsoft.VC.14.39.17.9.");
}

#[test]
fn pinned_version_never_falls_back_to_latest() {
    let tmp = TempDir::new().unwrap();
    let live_manifest = tmp.path().join("live.vsman");
    write_msvc_manifest(&live_manifest, &[("14.50.18.0", "14.50.35731")]);
    let live_channel = tmp.path().join("channel.json");
    write_channel_manifest(&live_channel, &live_manifest);

    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let ctx = InstallCtx::new(cache);
    let provider = MsvcProvider::with_channel_url_template(file_url(&live_channel))
        .with_archive_manifest_url_template("file:///archive/missing.json");

    let err = provider
        .resolve_toolset("17", Some("14.39.33523"), &ctx)
        .unwrap_err();
    let msg = format!("{err:#}");

    assert!(msg.contains("14.39.33523"), "got: {msg}");
    assert!(msg.contains("14.50.35731"), "got: {msg}");
    assert!(msg.contains("archived manifest failed"), "got: {msg}");
}

#[test]
fn pinned_version_reports_both_searched_manifests() {
    let tmp = TempDir::new().unwrap();
    let live_manifest = tmp.path().join("live.vsman");
    write_msvc_manifest(&live_manifest, &[("14.50.18.0", "14.50.35731")]);
    let live_channel = tmp.path().join("channel.json");
    write_channel_manifest(&live_channel, &live_manifest);
    let archive_manifest = tmp.path().join("archive.vsman");
    write_msvc_manifest(&archive_manifest, &[("14.38.17.8", "14.38.33130")]);

    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let ctx = InstallCtx::new(cache);
    let provider = MsvcProvider::with_channel_url_template(file_url(&live_channel))
        .with_archive_manifest_url_template(file_url(&archive_manifest));

    let err = provider
        .resolve_toolset("17", Some("14.39.33523"), &ctx)
        .unwrap_err();
    let msg = format!("{err:#}");

    assert!(msg.contains("Microsoft live manifest"), "got: {msg}");
    assert!(msg.contains("archived manifest"), "got: {msg}");
    assert!(msg.contains("14.50.35731"), "got: {msg}");
    assert!(msg.contains("14.38.33130"), "got: {msg}");
}

#[test]
fn pinned_version_matches_package_version_not_package_id_version() {
    let tmp = TempDir::new().unwrap();
    let live_manifest = tmp.path().join("live.vsman");
    write_msvc_manifest(&live_manifest, &[("14.51", "14.51.36243")]);
    let live_channel = tmp.path().join("channel.json");
    write_channel_manifest(&live_channel, &live_manifest);

    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let ctx = InstallCtx::new(cache);
    let provider = MsvcProvider::with_channel_url_template(file_url(&live_channel))
        .with_archive_manifest_url_template("file:///archive/missing.json");

    let err = provider
        .resolve_toolset("18", Some("14.51"), &ctx)
        .unwrap_err();
    let msg = format!("{err:#}");

    assert!(msg.contains("14.51"), "got: {msg}");
    assert!(msg.contains("14.51.36243"), "got: {msg}");
}

#[test]
fn pinned_archive_fallback_standard_install_extracts_payloads() {
    let tmp = TempDir::new().unwrap();
    let live_manifest = tmp.path().join("live.vsman");
    write_msvc_manifest(&live_manifest, &[("14.52", "14.52.36328")]);
    let live_channel = tmp.path().join("channel.json");
    write_channel_manifest(&live_channel, &live_manifest);

    let fixtures = tmp.path().join("fixtures");
    std::fs::create_dir_all(&fixtures).unwrap();
    let archive_manifest = tmp.path().join("archive.vsman");
    write_installable_msvc_manifest(&archive_manifest, &fixtures, "14.51", "14.51.36243");

    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let mut ctx = InstallCtx::new(cache);
    let provider = MsvcProvider::with_channel_url_template(file_url(&live_channel))
        .with_archive_manifest_url_template(file_url(&archive_manifest));
    let mut opts = opts_with_vs();
    opts.insert(
        "msvc_version".into(),
        toml::Value::String("14.51.36243".into()),
    );

    let installed = provider.install(&opts, &mut ctx).unwrap();

    assert!(installed.freshly_extracted);
    assert!(installed
        .fingerprint
        .starts_with("msvc-14.51.36243-vs2026-"));
    assert_eq!(
        installed
            .options
            .get("msvc_version")
            .and_then(|value| value.as_str()),
        Some("14.51.36243")
    );
    let raw = ctx.staging_dir().unwrap();
    assert!(raw
        .join("VC/Tools/MSVC/14.51.36243/bin/Hostx64/x64/cl.exe")
        .is_file());
    assert!(raw
        .join("VC/Tools/MSVC/14.51.36243/bin/Hostx64/x64/clui.dll")
        .is_file());
    assert!(raw
        .join("VC/Tools/MSVC/14.51.36243/include/vcruntime.h")
        .is_file());
    assert!(raw
        .join("VC/Tools/MSVC/14.51.36243/lib/x64/vcruntime.lib")
        .is_file());
    assert!(raw
        .join("VC/Tools/MSVC/14.51.36243/lib/x64/store/store.lib")
        .is_file());
    assert!(raw
        .join("VC/Redist/MSVC/14.51.36243/x64/Microsoft.VC145.CRT/vcruntime140.dll")
        .is_file());
}

fn standard_resolved_toolset() -> ResolvedMsvcToolset {
    resolved_from_manifest(
        r#"{
            "packages": [
                {"id": "Microsoft.VC.14.52.Tools.HostX64.TargetX64.base", "version": "14.52.36328", "payloads": []},
                {"id": "Microsoft.VC.14.52.Tools.HostX64.TargetX64.Res.base", "version": "14.52.36328", "language": "en-US", "payloads": []},
                {"id": "Microsoft.VC.14.52.Tools.HostX64.TargetX64.Res.base", "version": "14.52.36328", "language": "ja-JP", "payloads": []},
                {"id": "Microsoft.VC.14.52.CRT.Headers.base", "version": "14.52.36328", "payloads": []},
                {"id": "Microsoft.VC.14.52.CRT.x64.Desktop.base", "version": "14.52.36328", "payloads": []},
                {"id": "Microsoft.VC.14.52.CRT.x64.Store.base", "version": "14.52.36328", "payloads": []},
                {"id": "Microsoft.VC.14.52.CRT.Redist.X64.base", "version": "14.52.36328", "payloads": []},
                {"id": "Microsoft.VC.14.52.ATL.X64.base", "version": "14.52.36328", "payloads": []},
                {"id": "Microsoft.VC.Preview.DIA.SDK", "version": "14.52.36328", "payloads": []},
                {"id": "Microsoft.VC.Preview.DIA.Old", "version": "14.51.36243", "payloads": []}
            ]
        }"#,
        "14.52",
        "14.52.36328",
    )
}

fn resolved_from_manifest(
    json: &str,
    id_version: &str,
    package_version: &str,
) -> ResolvedMsvcToolset {
    ResolvedMsvcToolset {
        manifest: parse_msvc_manifest(json),
        package_version: package_version.into(),
        family_prefix: format!("Microsoft.VC.{id_version}."),
    }
}

fn package_ids(packages: &[PackageRequest]) -> Vec<String> {
    packages
        .iter()
        .map(|request| match &request.language {
            Some(language) => format!("{}:{language}", request.id),
            None => request.id.clone(),
        })
        .collect()
}

fn opts_with_vs() -> toml::Table {
    let mut opts = toml::Table::new();
    opts.insert("vs".into(), toml::Value::String("vs2026".into()));
    opts
}

fn write_channel_manifest(path: &std::path::Path, vs_manifest_path: &std::path::Path) {
    let vs_url = file_url(vs_manifest_path);
    let json = format!(
        r#"{{
            "channelItems": [{{
                "type": "Manifest",
                "id": "Microsoft.VisualStudio.Manifests.VisualStudio",
                "payloads": [{{ "url": "{vs_url}" }}]
            }}]
        }}"#
    );
    std::fs::write(path, json).unwrap();
}

fn write_msvc_manifest(path: &std::path::Path, versions: &[(&str, &str)]) {
    let packages = versions
        .iter()
        .map(|(id_version, package_version)| {
            format!(
                r#"{{
                    "id": "Microsoft.VC.{id_version}.Tools.HostX64.TargetX64.base",
                    "version": "{package_version}",
                    "payloads": [],
                    "dependencies": {{
                        "Microsoft.VC.{id_version}.Tools.HostX64.TargetX64.Res.base": "{package_version}"
                    }}
                }}"#
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    std::fs::write(path, format!(r#"{{ "packages": [{packages}] }}"#)).unwrap();
}

fn write_installable_msvc_manifest(
    manifest_path: &std::path::Path,
    fixtures_dir: &std::path::Path,
    id_version: &str,
    package_version: &str,
) {
    let packages = [
        (
            format!("Microsoft.VC.{id_version}.Tools.HostX64.TargetX64.base"),
            "tools.vsix",
            format!("VC/Tools/MSVC/{package_version}/bin/Hostx64/x64/cl.exe"),
        ),
        (
            format!("Microsoft.VC.{id_version}.Tools.HostX64.TargetX64.Res.base"),
            "tools-res.vsix",
            format!("VC/Tools/MSVC/{package_version}/bin/Hostx64/x64/clui.dll"),
        ),
        (
            format!("Microsoft.VC.{id_version}.CRT.Headers.base"),
            "crt-headers.vsix",
            format!("VC/Tools/MSVC/{package_version}/include/vcruntime.h"),
        ),
        (
            format!("Microsoft.VC.{id_version}.CRT.x64.Desktop.base"),
            "crt-desktop.vsix",
            format!("VC/Tools/MSVC/{package_version}/lib/x64/vcruntime.lib"),
        ),
        (
            format!("Microsoft.VC.{id_version}.CRT.x64.Store.base"),
            "crt-store.vsix",
            format!("VC/Tools/MSVC/{package_version}/lib/x64/store/store.lib"),
        ),
        (
            format!("Microsoft.VC.{id_version}.CRT.Redist.X64.base"),
            "crt-redist.vsix",
            format!("VC/Redist/MSVC/{package_version}/x64/Microsoft.VC145.CRT/vcruntime140.dll"),
        ),
    ];

    let json_packages = packages
        .iter()
        .map(|(id, filename, entry)| {
            let archive = fixtures_dir.join(filename);
            build_vsix(&archive, &[(entry.as_str(), id.as_bytes())]);
            let url = file_url(&archive);
            format!(
                r#"{{
                    "id": "{id}",
                    "version": "{package_version}",
                    "payloads": [{{ "url": "{url}", "fileName": "{filename}" }}]
                }}"#
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    std::fs::write(
        manifest_path,
        format!(r#"{{ "packages": [{json_packages}] }}"#),
    )
    .unwrap();
}

fn build_vsix(path: &std::path::Path, entries: &[(&str, &[u8])]) {
    let file = std::fs::File::create(path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let opts = zip::write::FileOptions::default();
    for (name, bytes) in entries {
        zip.start_file(format!("Contents/{name}"), opts).unwrap();
        zip.write_all(bytes).unwrap();
    }
    zip.finish().unwrap();
}

fn parse_msvc_manifest(json: &str) -> VsManifest {
    serde_json::from_str(json).unwrap()
}

fn toml_array(values: &[&str]) -> toml::Value {
    toml::Value::Array(
        values
            .iter()
            .map(|value| toml::Value::String((*value).to_string()))
            .collect(),
    )
}

fn file_url(path: &std::path::Path) -> String {
    url::Url::from_file_path(path).unwrap().to_string()
}
