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
fn msvc_provider_required_fields() {
    let tmp = TempDir::new().unwrap();
    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let mut ctx = InstallCtx::new(cache);
    let opts = toml::Table::new();
    let err = MsvcProvider::new().install(&opts, &mut ctx).unwrap_err();
    assert!(err.to_string().contains("vs_channel"));
}

#[test]
fn msvc_provider_pinned_version_fast_path_no_network() {
    // Pre-plant an install dir matching the deterministic fingerprint.
    // The provider should short-circuit without ever calling fetch.
    let tmp = TempDir::new().unwrap();
    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let fp = sanitize_fingerprint("msvc-14.50.35731-18");
    let install_dir = cache.install_dir(&fp);
    std::fs::create_dir_all(install_dir.join("tree")).unwrap();
    let md = crate::cache::InstallMetadata::new(
        "msvc",
        fp.clone(),
        "msvc 14.50.35731 (vs18)",
        toml::Table::new(),
    );
    md.write(&cache.install_metadata_path(&fp)).unwrap();

    let mut ctx = InstallCtx::new(cache);
    let mut opts = toml::Table::new();
    opts.insert("vs_channel".into(), toml::Value::String("18".into()));
    opts.insert(
        "msvc_version".into(),
        toml::Value::String("14.50.35731".into()),
    );

    // Use a deliberately invalid template — if the provider tries to
    // hit it, we'll see the failure.
    let provider =
        MsvcProvider::with_channel_url_template("file:///never/exists/{channel}");
    let installed = provider.install(&opts, &mut ctx).unwrap();
    assert!(!installed.freshly_extracted);
    assert_eq!(installed.fingerprint, fp);
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
    assert_eq!(resolved.package_id_version, "14.39.17.9");
    assert!(resolved.base_package_id.contains("14.39.17.9"));
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
    assert_eq!(resolved.package_id_version, "14.39.17.9");
    assert!(resolved.base_package_id.contains("14.39.17.9"));
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

    let err = provider.resolve_toolset("18", Some("14.51"), &ctx).unwrap_err();
    let msg = format!("{err:#}");

    assert!(msg.contains("14.51"), "got: {msg}");
    assert!(msg.contains("14.51.36243"), "got: {msg}");
}

#[test]
fn required_packages_use_resource_dependency_from_manifest() {
    let manifest = parse_msvc_manifest(
        r#"{
            "packages": [
                {
                    "id": "Microsoft.VC.14.51.Tools.HostX64.TargetX64.base",
                    "version": "14.51.36243",
                    "payloads": [],
                    "dependencies": {
                        "Microsoft.VC.14.51.Tools.HostX64.TargetX64.Res.base": "14.51.36243"
                    }
                }
            ]
        }"#,
    );
    let resolved = ResolvedMsvcToolset {
        manifest,
        package_version: "14.51.36243".into(),
        package_id_version: "14.51".into(),
        base_package_id: "Microsoft.VC.14.51.Tools.HostX64.TargetX64.base".into(),
    };

    let ids = required_msvc_package_ids(&resolved).unwrap();

    assert_eq!(
        ids,
        vec![
            "Microsoft.VC.14.51.Tools.HostX64.TargetX64.base",
            "Microsoft.VC.14.51.Tools.HostX64.TargetX64.Res.base",
            "Microsoft.VC.14.51.CRT.Headers.base",
            "Microsoft.VC.14.51.CRT.x64.Desktop.base",
            "Microsoft.VC.14.51.CRT.x64.Store.base",
        ]
    );
}

#[test]
fn missing_resource_dependency_is_clear_error() {
    let manifest = parse_msvc_manifest(
        r#"{
            "packages": [{
                "id": "Microsoft.VC.14.51.Tools.HostX64.TargetX64.base",
                "version": "14.51.36243",
                "payloads": []
            }]
        }"#,
    );
    let resolved = ResolvedMsvcToolset {
        manifest,
        package_version: "14.51.36243".into(),
        package_id_version: "14.51".into(),
        base_package_id: "Microsoft.VC.14.51.Tools.HostX64.TargetX64.base".into(),
    };

    let err = required_msvc_package_ids(&resolved).unwrap_err();
    assert!(
        err.to_string().contains("resource package dependency"),
        "got: {err:#}"
    );
}

#[test]
fn pinned_archive_fallback_full_install_extracts_payloads() {
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
    let mut opts = toml::Table::new();
    opts.insert("vs_channel".into(), toml::Value::String("18".into()));
    opts.insert(
        "msvc_version".into(),
        toml::Value::String("14.51.36243".into()),
    );

    let installed = provider.install(&opts, &mut ctx).unwrap();

    assert!(installed.freshly_extracted);
    assert_eq!(installed.fingerprint, sanitize_fingerprint("msvc-14.51.36243-18"));
    assert_eq!(
        installed
            .options
            .get("msvc_version")
            .and_then(|value| value.as_str()),
        Some("14.51.36243")
    );
    let raw = ctx.staging_dir().unwrap();
    assert!(raw.join("VC/Tools/MSVC/14.51.36243/bin/Hostx64/x64/cl.exe").is_file());
    assert!(raw.join("VC/Tools/MSVC/14.51.36243/bin/Hostx64/x64/clui.dll").is_file());
    assert!(raw.join("VC/Tools/MSVC/14.51.36243/include/vcruntime.h").is_file());
    assert!(raw.join("VC/Tools/MSVC/14.51.36243/lib/x64/vcruntime.lib").is_file());
    assert!(raw.join("VC/Tools/MSVC/14.51.36243/lib/x64/store/store.lib").is_file());
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
    ];

    let json_packages = packages
        .iter()
        .map(|(id, filename, entry)| {
            let archive = fixtures_dir.join(filename);
            build_vsix(&archive, &[(entry.as_str(), id.as_bytes())]);
            let url = file_url(&archive);
            let dependencies = if id.ends_with(".TargetX64.base") {
                format!(
                    r#""dependencies": {{
                        "Microsoft.VC.{id_version}.Tools.HostX64.TargetX64.Res.base": "{package_version}"
                    }},"#
                )
            } else {
                String::new()
            };
            format!(
                r#"{{
                    "id": "{id}",
                    "version": "{package_version}",
                    {dependencies}
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

fn file_url(path: &std::path::Path) -> String {
    url::Url::from_file_path(path).unwrap().to_string()
}
