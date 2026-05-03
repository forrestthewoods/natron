//! Tests for `src/providers\msvc.rs` (split out so the production
//! file shows only the implementation).

use super::*;
use crate::cache::Cache;
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
    let fp = sanitize_fingerprint("msvc-14.50.18.0-18");
    let install_dir = cache.install_dir(&fp);
    std::fs::create_dir_all(install_dir.join("tree")).unwrap();
    let md = crate::cache::InstallMetadata::new(
        "msvc",
        fp.clone(),
        "msvc 14.50.18.0 (vs18)",
        toml::Table::new(),
    );
    md.write(&cache.install_metadata_path(&fp)).unwrap();

    let mut ctx = InstallCtx::new(cache);
    let mut opts = toml::Table::new();
    opts.insert("vs_channel".into(), toml::Value::String("18".into()));
    opts.insert(
        "msvc_version".into(),
        toml::Value::String("14.50.18.0".into()),
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
fn msvc_manifest_history_requires_pinned_version() {
    let tmp = TempDir::new().unwrap();
    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let mut ctx = InstallCtx::new(cache);

    let mut opts = toml::Table::new();
    opts.insert("vs_channel".into(), toml::Value::String("17".into()));
    opts.insert("manifest_history".into(), toml::Value::Boolean(true));

    let err = MsvcProvider::new().install(&opts, &mut ctx).unwrap_err();
    assert!(err.to_string().contains("msvc_version"), "got: {err}");
}

#[test]
fn msvc_manifest_history_routes_to_walker_not_channel() {
    // Hermetic regression test for the if/else in install(). Both URL families
    // are pointed at distinguishable file:// paths that don't exist; whichever
    // path the provider actually takes surfaces its template fragment in the
    // resulting error. Asserts we hit the history walker, not aka.ms.
    let tmp = TempDir::new().unwrap();
    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let mut ctx = InstallCtx::new(cache);

    let mut opts = toml::Table::new();
    opts.insert("vs_channel".into(), toml::Value::String("17".into()));
    opts.insert(
        "msvc_version".into(),
        toml::Value::String("14.39.33519.0".into()),
    );
    opts.insert("manifest_history".into(), toml::Value::Boolean(true));

    let provider = MsvcProvider::with_channel_url_template(
        "file:///nope/CHANNEL-{channel}",
    )
    .with_history_urls(
        "file:///nope/COMMITS-{channel}-{page}",
        "file:///nope/RAW-{sha}",
    );
    let err = provider.install(&opts, &mut ctx).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("COMMITS-"), "expected walker path; got: {msg}");
    assert!(!msg.contains("CHANNEL-"), "fell through to channel; got: {msg}");
    // The new with_context should surface the searched version.
    assert!(msg.contains("14.39.33519.0"), "missing version; got: {msg}");
}
