//! Tests for `src/providers\mod.rs` (split out so the production
//! file shows only the implementation).

use super::*;
use tempfile::TempDir;

struct StubProvider;
impl Provider for StubProvider {
    fn id(&self) -> &'static str {
        "stub"
    }
    fn install(&self, _options: &toml::Table, _ctx: &mut InstallCtx) -> Result<Installed> {
        Ok(Installed {
            fingerprint: "stub-fp".into(),
            display: "stub".into(),
            options: toml::Table::new(),
            freshly_extracted: false,
        })
    }
}

#[test]
fn registry_register_and_lookup() {
    let mut r = ProviderRegistry::empty();
    r.register(StubProvider);
    assert!(r.get("stub").is_some());
    assert!(r.get("missing").is_none());
    assert!(r.require("missing").is_err());
}

#[test]
fn install_ctx_lazy_staging() {
    let tmp = TempDir::new().unwrap();
    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let ctx = InstallCtx::new(cache);
    assert!(ctx.staging_root().is_none());
    let raw = ctx.staging_dir().unwrap();
    assert!(raw.is_dir());
    assert!(raw.ends_with("raw"));
    let raw2 = ctx.staging_dir().unwrap();
    assert_eq!(raw, raw2);
    let root = ctx.staging_root().unwrap();
    assert_eq!(raw.parent().unwrap(), root);
}

#[test]
fn install_ctx_no_staging_when_unused() {
    let tmp = TempDir::new().unwrap();
    let cache = Cache::at(tmp.path().join("c"));
    cache.ensure_layout().unwrap();
    let ctx = InstallCtx::new(cache);
    assert!(ctx.staging_root().is_none());
    // No directory should have been allocated.
    let entries: Vec<_> = std::fs::read_dir(tmp.path().join("c").join("staging"))
        .unwrap()
        .collect();
    assert!(entries.is_empty());
}

#[test]
fn default_registry_has_url_provider() {
    let r = ProviderRegistry::default();
    assert!(r.get("url").is_some());
}
