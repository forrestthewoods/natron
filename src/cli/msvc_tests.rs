//! Tests for `src/cli/msvc.rs`.

use super::*;
use crate::cache::Cache;
use crate::providers::InstallCtx;
use std::io::Write;
use tempfile::TempDir;

// ---- versions ---------------------------------------------------------------

#[test]
fn versions_lists_live_and_archive_dedup() {
    let tmp = TempDir::new().unwrap();
    let live = tmp.path().join("live.vsman");
    write_msvc_manifest(&live, &[("14.52.18.0", "14.52.36328"), ("14.51.17.0", "14.51.36243")]);
    let live_ch = tmp.path().join("ch.json");
    write_channel_manifest(&live_ch, &live);
    let archive = tmp.path().join("archive.vsman");
    write_msvc_manifest(
        &archive,
        &[("14.51.17.0", "14.51.36243"), ("14.43.10.0", "14.43.34808")],
    );

    let ctx = test_ctx(&tmp);
    let urls = ChannelUrls {
        live: file_url(&live_ch),
        archive: file_url(&archive),
    };
    let mut out = Vec::new();
    run_versions(
        &ctx,
        &urls,
        VersionsArgs {
            vs: Some("vs2026".into()),
        },
        &mut out,
    )
    .unwrap();
    let s = String::from_utf8(out).unwrap();

    assert!(s.contains("vs2026 (channel 18)"), "{s}");
    // 14.52 only in live.
    assert!(s.contains("14.52.36328"), "{s}");
    let line_52 = line_for(&s, "14.52.36328");
    assert!(line_52.contains("live"), "{line_52}");
    assert!(!line_52.contains("archive"), "{line_52}");
    // 14.51 in both.
    let line_51 = line_for(&s, "14.51.36243");
    assert!(line_51.contains("live"), "{line_51}");
    assert!(line_51.contains("archive"), "{line_51}");
    // 14.43 archive-only.
    let line_43 = line_for(&s, "14.43.34808");
    assert!(line_43.contains("archive-only"), "{line_43}");
}

#[test]
fn versions_tolerates_archive_failure() {
    let tmp = TempDir::new().unwrap();
    let live = tmp.path().join("live.vsman");
    write_msvc_manifest(&live, &[("14.52.18.0", "14.52.36328")]);
    let live_ch = tmp.path().join("ch.json");
    write_channel_manifest(&live_ch, &live);

    let ctx = test_ctx(&tmp);
    let urls = ChannelUrls {
        live: file_url(&live_ch),
        archive: "file:///definitely/missing.json".into(),
    };
    let mut out = Vec::new();
    run_versions(
        &ctx,
        &urls,
        VersionsArgs {
            vs: Some("vs2026".into()),
        },
        &mut out,
    )
    .unwrap();
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains("14.52.36328"), "{s}");
    assert!(s.contains("archive: <error"), "{s}");
}

// ---- packages ---------------------------------------------------------------

#[test]
fn packages_groups_in_family_first_then_outside() {
    let tmp = TempDir::new().unwrap();
    let live = tmp.path().join("live.vsman");
    write_installable_msvc_manifest(&live, &tmp.path().join("vsix"), "14.51", "14.51.36243");
    let live_ch = tmp.path().join("ch.json");
    write_channel_manifest(&live_ch, &live);

    let ctx = test_ctx(&tmp);
    let urls = ChannelUrls {
        live: file_url(&live_ch),
        archive: "file:///nope.json".into(),
    };
    let mut out = Vec::new();
    run_packages(
        &ctx,
        &urls,
        PackagesArgs {
            vs: "vs2026".into(),
            version: Some("14.51.36243".into()),
        },
        &mut out,
    )
    .unwrap();
    let s = String::from_utf8(out).unwrap();

    assert!(s.contains("14.51.36243"), "{s}");
    assert!(s.contains("Microsoft.VC.14.51"), "{s}");
    assert!(s.contains("== family =="), "{s}");
    assert!(s.contains("== other"), "{s}");
    // Family section appears before "other" section.
    let fam = s.find("== family ==").unwrap();
    let other = s.find("== other").unwrap();
    assert!(fam < other, "family must precede other");
}

#[test]
fn packages_resolves_latest_when_version_omitted() {
    let tmp = TempDir::new().unwrap();
    let live = tmp.path().join("live.vsman");
    write_installable_msvc_manifest(&live, &tmp.path().join("vsix"), "14.52", "14.52.36328");
    let live_ch = tmp.path().join("ch.json");
    write_channel_manifest(&live_ch, &live);

    let ctx = test_ctx(&tmp);
    let urls = ChannelUrls {
        live: file_url(&live_ch),
        archive: "file:///nope.json".into(),
    };
    let mut out = Vec::new();
    run_packages(
        &ctx,
        &urls,
        PackagesArgs {
            vs: "vs2026".into(),
            version: None,
        },
        &mut out,
    )
    .unwrap();
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains("14.52.36328"), "{s}");
}

// ---- extract ----------------------------------------------------------------

#[test]
fn extract_writes_one_dir_per_package() {
    let tmp = TempDir::new().unwrap();
    let live = tmp.path().join("live.vsman");
    write_installable_msvc_manifest(&live, &tmp.path().join("vsix"), "14.51", "14.51.36243");
    let live_ch = tmp.path().join("ch.json");
    write_channel_manifest(&live_ch, &live);

    let out_dir = tmp.path().join("out");
    let ctx = test_ctx(&tmp);
    let urls = ChannelUrls {
        live: file_url(&live_ch),
        archive: "file:///nope.json".into(),
    };
    let mut out_buf = Vec::new();
    run_extract(
        &ctx,
        &urls,
        ExtractArgs {
            vs: "vs2026".into(),
            version: "14.51.36243".into(),
            out: out_dir.clone(),
        },
        &mut out_buf,
    )
    .unwrap();

    // The fixture writes 6 packages. Each should have its own dir with a
    // file matching the package's expected entry.
    let entries: Vec<_> = std::fs::read_dir(&out_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries.len(), 6, "expected 6 package dirs, got {entries:?}");
    assert!(
        entries.iter().any(|n| n.starts_with("Microsoft.VC.14.51.Tools.HostX64.TargetX64.base")),
        "got: {entries:?}"
    );

    // Spot-check one extracted file landed in its package's own dir.
    let tools_dir =
        out_dir.join("Microsoft.VC.14.51.Tools.HostX64.TargetX64.base");
    assert!(
        tools_dir
            .join("VC/Tools/MSVC/14.51.36243/bin/Hostx64/x64/cl.exe")
            .is_file(),
        "cl.exe missing from Tools.HostX64.TargetX64.base"
    );

    let s = String::from_utf8(out_buf).unwrap();
    assert!(s.contains("done: 6 extracted, 0 already present"), "{s}");
}

#[test]
fn extract_is_idempotent_for_populated_dirs() {
    let tmp = TempDir::new().unwrap();
    let live = tmp.path().join("live.vsman");
    write_installable_msvc_manifest(&live, &tmp.path().join("vsix"), "14.51", "14.51.36243");
    let live_ch = tmp.path().join("ch.json");
    write_channel_manifest(&live_ch, &live);

    let out_dir = tmp.path().join("out");
    let ctx = test_ctx(&tmp);
    let urls = ChannelUrls {
        live: file_url(&live_ch),
        archive: "file:///nope.json".into(),
    };
    let args = || ExtractArgs {
        vs: "vs2026".into(),
        version: "14.51.36243".into(),
        out: out_dir.clone(),
    };

    let mut out_buf = Vec::new();
    run_extract(&ctx, &urls, args(), &mut out_buf).unwrap();
    out_buf.clear();
    run_extract(&ctx, &urls, args(), &mut out_buf).unwrap();
    let s = String::from_utf8(out_buf).unwrap();
    assert!(s.contains("done: 0 extracted, 6 already present"), "{s}");
}

#[test]
fn per_package_dir_name_suffixes_language() {
    assert_eq!(
        per_package_dir_name("Microsoft.VC.14.51.Tools.HostX64.TargetX64.base", None),
        "Microsoft.VC.14.51.Tools.HostX64.TargetX64.base"
    );
    assert_eq!(
        per_package_dir_name("Microsoft.VC.14.51.Tools.Res.base", Some("en-US")),
        "Microsoft.VC.14.51.Tools.Res.base+en-US"
    );
}

// ---- fixtures ---------------------------------------------------------------
//
// Mirror the helpers in providers/msvc_tests.rs. Kept local because making
// those `pub` solely for test reuse isn't worth the surface area.

fn test_ctx(tmp: &TempDir) -> InstallCtx {
    let cache = Cache::at(tmp.path().join("cache"));
    cache.ensure_layout().unwrap();
    InstallCtx::new(cache)
}

fn line_for(s: &str, needle: &str) -> String {
    s.lines()
        .find(|l| l.contains(needle))
        .map(|l| l.to_string())
        .unwrap_or_else(|| panic!("no line containing '{needle}' in:\n{s}"))
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
                    "payloads": []
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
    std::fs::create_dir_all(fixtures_dir).unwrap();
    // 5 family packages + 1 unrelated package to exercise "other" grouping.
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
            format!("Microsoft.VC.{id_version}.ATL.X64.base"),
            "atl.vsix",
            format!("VC/Tools/MSVC/{package_version}/atlmfc/lib/x64/atls.lib"),
        ),
        (
            "Microsoft.VC.Preview.DIA.SDK".to_string(),
            "dia.vsix",
            "DIA SDK/bin/msdia140.dll".to_string(),
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

fn file_url(path: &std::path::Path) -> String {
    url::Url::from_file_path(path).unwrap().to_string()
}
