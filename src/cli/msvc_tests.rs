//! Tests for `src/cli/msvc.rs`.

use super::*;
use crate::providers::vs_manifest::tests::{
    build_vsix, file_url, pkg, pkg_with_payload, test_ctx, FxSnapshot, MirrorFixture,
};
use tempfile::TempDir;

// ---- versions --------------------------------------------------------------

#[test]
fn versions_lists_builds_descending_per_series() {
    let tmp = TempDir::new().unwrap();
    let fx = MirrorFixture::build(
        tmp.path(),
        &[(
            VsVersion::Vs2026,
            &[
                FxSnapshot {
                    sha: "sha_old",
                    date: "2026-01-01T00:00:00Z",
                    build_version: "18.0.0.0",
                    display_version: "18.0",
                    product_line_version: "18",
                    manifest_packages_json: String::new(),
                },
                FxSnapshot {
                    sha: "sha_new",
                    date: "2026-05-01T00:00:00Z",
                    build_version: "18.6.11819.183",
                    display_version: "18.6.1",
                    product_line_version: "18",
                    manifest_packages_json: String::new(),
                },
            ],
        )],
    );
    let ctx = test_ctx(&tmp);
    let mut out = Vec::new();
    run_versions(
        &ctx,
        &fx.urls,
        VersionsArgs {
            vs: Some("vs2026".into()),
        },
        &mut out,
    )
    .unwrap();
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains("vs2026 (channel 18)"), "{s}");
    assert!(s.contains("18.6.11819.183"), "{s}");
    assert!(s.contains("18.0.0.0"), "{s}");
    let i_new = s.find("18.6.11819.183").unwrap();
    let i_old = s.find("18.0.0.0").unwrap();
    assert!(i_new < i_old, "expected newest-first");
}

#[test]
fn versions_iterates_all_series_when_no_filter() {
    let tmp = TempDir::new().unwrap();
    let fx = MirrorFixture::build(
        tmp.path(),
        &[
            (
                VsVersion::Vs2019,
                &[FxSnapshot {
                    sha: "s16",
                    date: "2026-05-01T00:00:00Z",
                    build_version: "16.11.0.0",
                    display_version: "16.11",
                    product_line_version: "16",
                    manifest_packages_json: String::new(),
                }],
            ),
            (
                VsVersion::Vs2022,
                &[FxSnapshot {
                    sha: "s17",
                    date: "2026-05-01T00:00:00Z",
                    build_version: "17.14.0.0",
                    display_version: "17.14",
                    product_line_version: "17",
                    manifest_packages_json: String::new(),
                }],
            ),
            (
                VsVersion::Vs2026,
                &[FxSnapshot {
                    sha: "s18",
                    date: "2026-05-01T00:00:00Z",
                    build_version: "18.6.0.0",
                    display_version: "18.6",
                    product_line_version: "18",
                    manifest_packages_json: String::new(),
                }],
            ),
        ],
    );
    let ctx = test_ctx(&tmp);
    let mut out = Vec::new();
    run_versions(&ctx, &fx.urls, VersionsArgs { vs: None }, &mut out).unwrap();
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains("vs2019 (channel 16)"), "{s}");
    assert!(s.contains("vs2022 (channel 17)"), "{s}");
    assert!(s.contains("vs2026 (channel 18)"), "{s}");
}

// ---- packages --------------------------------------------------------------

#[test]
fn packages_groups_family_first_then_others() {
    let tmp = TempDir::new().unwrap();
    let family_pkg = pkg("Microsoft.VC.14.52.18.5.Tools.HostX64.TargetX64.base", "14.52.36328");
    let other_pkg = pkg("Microsoft.VC.Preview.DIA.SDK", "14.52.36328");
    let manifest_packages = format!("{family_pkg},{other_pkg}");

    let fx = MirrorFixture::build(
        tmp.path(),
        &[(
            VsVersion::Vs2026,
            &[FxSnapshot {
                sha: "abc",
                date: "2026-05-01T00:00:00Z",
                build_version: "18.6.11819.183",
                display_version: "18.6.1",
                product_line_version: "18",
                manifest_packages_json: manifest_packages,
            }],
        )],
    );
    let ctx = test_ctx(&tmp);
    let mut out = Vec::new();
    run_packages(
        &ctx,
        &fx.urls,
        PackagesArgs {
            build_version: "18.6.11819.183".into(),
        },
        &mut out,
    )
    .unwrap();
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains("18.6.11819.183"), "{s}");
    assert!(s.contains("== family =="), "{s}");
    assert!(s.contains("== other in snapshot =="), "{s}");
    let i_fam = s.find("== family ==").unwrap();
    let i_oth = s.find("== other in snapshot ==").unwrap();
    assert!(i_fam < i_oth, "family must precede other");
    assert!(s.contains("Microsoft.VC.14.52.18.5.Tools.HostX64.TargetX64.base"), "{s}");
    assert!(s.contains("Microsoft.VC.Preview.DIA.SDK"), "{s}");
}

#[test]
fn packages_errors_on_unknown_build_version() {
    let tmp = TempDir::new().unwrap();
    let fx = MirrorFixture::build(
        tmp.path(),
        &[(
            VsVersion::Vs2026,
            &[FxSnapshot {
                sha: "abc",
                date: "2026-05-01T00:00:00Z",
                build_version: "18.6.11819.183",
                display_version: "18.6.1",
                product_line_version: "18",
                manifest_packages_json: String::new(),
            }],
        )],
    );
    let ctx = test_ctx(&tmp);
    let mut out = Vec::new();
    let err = run_packages(
        &ctx,
        &fx.urls,
        PackagesArgs {
            build_version: "18.99.99.99".into(),
        },
        &mut out,
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("18.99.99.99"), "got: {msg}");
}

// ---- extract ---------------------------------------------------------------

#[test]
fn extract_writes_per_package_dirs_idempotently() {
    let tmp = TempDir::new().unwrap();
    let fixtures = tmp.path().join("vsix");
    std::fs::create_dir_all(&fixtures).unwrap();

    let id1 = "Microsoft.VC.14.52.18.5.Tools.HostX64.TargetX64.base";
    let arch1 = fixtures.join("tools.vsix");
    build_vsix(
        &arch1,
        &[("VC/Tools/MSVC/14.52.36328/bin/Hostx64/x64/cl.exe", id1.as_bytes())],
    );
    let id2 = "Microsoft.VC.14.52.18.5.CRT.Headers.base";
    let arch2 = fixtures.join("crt.vsix");
    build_vsix(
        &arch2,
        &[("VC/Tools/MSVC/14.52.36328/include/vcruntime.h", id2.as_bytes())],
    );
    let pkgs = format!(
        "{a},{b}",
        a = pkg_with_payload(id1, "14.52.36328", &file_url(&arch1), "tools.vsix"),
        b = pkg_with_payload(id2, "14.52.36328", &file_url(&arch2), "crt.vsix"),
    );

    let fx = MirrorFixture::build(
        tmp.path(),
        &[(
            VsVersion::Vs2026,
            &[FxSnapshot {
                sha: "abc",
                date: "2026-05-01T00:00:00Z",
                build_version: "18.6.11819.183",
                display_version: "18.6.1",
                product_line_version: "18",
                manifest_packages_json: pkgs,
            }],
        )],
    );

    let out_dir = tmp.path().join("out");
    let ctx = test_ctx(&tmp);
    let args = || ExtractArgs {
        build_version: "18.6.11819.183".into(),
        out: out_dir.clone(),
    };

    let mut buf = Vec::new();
    run_extract(&ctx, &fx.urls, args(), &mut buf).unwrap();
    let entries: Vec<_> = std::fs::read_dir(&out_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries.len(), 2);
    assert!(out_dir
        .join("Microsoft.VC.14.52.18.5.Tools.HostX64.TargetX64.base")
        .join("VC/Tools/MSVC/14.52.36328/bin/Hostx64/x64/cl.exe")
        .is_file());

    buf.clear();
    run_extract(&ctx, &fx.urls, args(), &mut buf).unwrap();
    let s = String::from_utf8(buf).unwrap();
    assert!(s.contains("0 extracted, 2 already present"), "{s}");
}

#[test]
fn per_package_dir_name_suffixes_language() {
    assert_eq!(
        per_package_dir_name("Microsoft.VC.14.52.Tools.base", None),
        "Microsoft.VC.14.52.Tools.base"
    );
    assert_eq!(
        per_package_dir_name("Microsoft.VC.14.52.Tools.Res.base", Some("ja-JP")),
        "Microsoft.VC.14.52.Tools.Res.base+ja-JP"
    );
}
