//! Tests for `src/cli/windows_sdk.rs`.

use super::*;
use crate::providers::vs_manifest::tests::{
    file_url, pkg, pkg_with_payload, test_ctx, FxSnapshot, MirrorFixture,
};
use crate::providers::vs_manifest::VsVersion;
use tempfile::TempDir;

// ---- fixture builders ------------------------------------------------------

/// Build a JSON packages list for one Windows SDK component meta-package
/// plus its dependency MSI packages. Each dep id is a stub package with
/// a single .msi payload (which doesn't need to be a real MSI for the
/// `packages` / `versions` CLIs — they don't extract).
fn sdk_snapshot_packages(
    sdk_version: &str,
    msi_filenames: &[&str],
    payload_url: &str,
) -> String {
    let component_id = format!(
        "Microsoft.VisualStudio.Component.Windows11SDK.{sdk_version}"
    );
    let mut deps = Vec::new();
    let mut dep_pkgs = Vec::new();
    for (i, filename) in msi_filenames.iter().enumerate() {
        let dep_id = format!("Microsoft.Windows.10.SDK.Dep.{i}");
        deps.push(format!(r#""{dep_id}":"x""#));
        dep_pkgs.push(pkg_with_payload(&dep_id, "x", payload_url, filename));
    }
    let component_json = format!(
        r#"{{"id":"{component_id}","version":"x","payloads":[],"dependencies":{{{}}}}}"#,
        deps.join(",")
    );
    format!("{component_json},{}", dep_pkgs.join(","))
}

// ---- versions --------------------------------------------------------------

#[test]
fn versions_lists_distinct_across_series_sorted_desc() {
    let tmp = TempDir::new().unwrap();
    let mk = |v: &str| pkg(&format!("Microsoft.VisualStudio.Component.Windows11SDK.{v}"), "x");
    let fx = MirrorFixture::build(
        tmp.path(),
        &[
            (
                VsVersion::Vs2026,
                &[FxSnapshot {
                    sha: "s18",
                    date: "2026-05-01T00:00:00Z",
                    build_version: "18.6.0.0",
                    display_version: "18.6",
                    product_line_version: "18",
                    manifest_packages_json: format!("{},{}", mk("26100"), mk("22621")),
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
                    manifest_packages_json: format!("{},{}", mk("22621"), mk("19041")),
                }],
            ),
        ],
    );
    let ctx = test_ctx(&tmp);
    let mut out = Vec::new();
    run_versions(&ctx, &fx.urls, VersionsArgs {}, &mut out).unwrap();
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains("Windows SDK versions"), "{s}");
    assert!(s.contains("26100"), "{s}");
    assert!(s.contains("22621"), "{s}");
    assert!(s.contains("19041"), "{s}");
    // sorted desc
    let i_high = s.find("26100").unwrap();
    let i_mid = s.find("22621").unwrap();
    let i_low = s.find("19041").unwrap();
    assert!(i_high < i_mid && i_mid < i_low, "wrong order");
    // 22621 deduped (appears once in output)
    assert_eq!(s.matches("22621").count(), 1, "got duplicate 22621 in:\n{s}");
}

// ---- packages --------------------------------------------------------------

#[test]
fn packages_groups_default_and_extras() {
    let tmp = TempDir::new().unwrap();
    // One real (filler) URL so payload entries are well-formed; we never
    // download in `packages`.
    let filler = tmp.path().join("filler.msi");
    std::fs::write(&filler, b"x").unwrap();
    let filler_url = file_url(&filler);

    let msis = &[
        "Universal CRT Headers Libraries and Sources-x86_en-us.msi",
        "Windows SDK Desktop Headers x86-x86_en-us.msi",
        "Windows SDK Signing Tools-x86_en-us.msi",
        "Windows SDK Debugging Tools-x86_en-us.msi",
    ];
    let pkgs_json = sdk_snapshot_packages("26100", msis, &filler_url);

    let fx = MirrorFixture::build(
        tmp.path(),
        &[(
            VsVersion::Vs2026,
            &[FxSnapshot {
                sha: "s18",
                date: "2026-05-01T00:00:00Z",
                build_version: "18.6.0.0",
                display_version: "18.6",
                product_line_version: "18",
                manifest_packages_json: pkgs_json,
            }],
        )],
    );
    let ctx = test_ctx(&tmp);
    let mut out = Vec::new();
    run_packages(
        &ctx,
        &fx.urls,
        PackagesArgs {
            sdk_version: "26100".into(),
        },
        &mut out,
    )
    .unwrap();
    let s = String::from_utf8(out).unwrap();

    assert!(s.contains("windows_sdk 26100"), "{s}");
    assert!(s.contains("4 MSIs total"), "{s}");
    let i_default = s.find("== installed by base_install=default").unwrap();
    let i_extras = s.find("== available for extras").unwrap();
    assert!(i_default < i_extras, "default group must precede extras");

    // Universal CRT lands in default; Signing Tools lands in extras.
    let crt_pos = s.find("Universal CRT Headers Libraries and Sources").unwrap();
    let sig_pos = s.find("Windows SDK Signing Tools").unwrap();
    assert!(crt_pos < i_extras, "CRT should be in default group");
    assert!(sig_pos > i_extras, "Signing Tools should be in extras group");
}

#[test]
fn packages_errors_on_unknown_sdk_version() {
    let tmp = TempDir::new().unwrap();
    let mk = |v: &str| pkg(&format!("Microsoft.VisualStudio.Component.Windows11SDK.{v}"), "x");
    let fx = MirrorFixture::build(
        tmp.path(),
        &[(
            VsVersion::Vs2026,
            &[FxSnapshot {
                sha: "s18",
                date: "2026-05-01T00:00:00Z",
                build_version: "18.6.0.0",
                display_version: "18.6",
                product_line_version: "18",
                manifest_packages_json: mk("26100"),
            }],
        )],
    );
    let ctx = test_ctx(&tmp);
    let mut out = Vec::new();
    let err = run_packages(
        &ctx,
        &fx.urls,
        PackagesArgs {
            sdk_version: "99999".into(),
        },
        &mut out,
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("99999"), "got: {msg}");
}

// ---- extract ---------------------------------------------------------------
//
// Real MSI extraction needs Windows + msiexec, so we don't exercise the
// extract_msi step hermetically. The resolution + skip-if-populated paths
// are covered below; the full extract is verified manually via real
// upstream (`natron windows_sdk extract --sdk-version 26100 --out ...`).

#[test]
fn extract_errors_on_unknown_sdk_version() {
    let tmp = TempDir::new().unwrap();
    let mk = |v: &str| pkg(&format!("Microsoft.VisualStudio.Component.Windows11SDK.{v}"), "x");
    let fx = MirrorFixture::build(
        tmp.path(),
        &[(
            VsVersion::Vs2026,
            &[FxSnapshot {
                sha: "s18",
                date: "2026-05-01T00:00:00Z",
                build_version: "18.6.0.0",
                display_version: "18.6",
                product_line_version: "18",
                manifest_packages_json: mk("26100"),
            }],
        )],
    );
    let out_dir = tmp.path().join("out");
    let ctx = test_ctx(&tmp);
    let mut buf = Vec::new();
    let err = run_extract(
        &ctx,
        &fx.urls,
        ExtractArgs {
            sdk_version: "99999".into(),
            out: out_dir,
        },
        &mut buf,
    )
    .unwrap_err();
    assert!(err.to_string().contains("99999"), "got: {err}");
}
