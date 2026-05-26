//! `Provider` trait, `InstallCtx`, and `ProviderRegistry`. Built-in provider
//! implementations live in sibling modules (added in steps 8–12).

use anyhow::{Context, Result, anyhow};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::cache::Cache;
use crate::download;

pub mod github;
pub mod msvc;
mod options;
pub mod url;
pub mod vs_manifest;
pub mod windows_sdk;
pub mod zig;

pub use github::GithubProvider;
pub use msvc::MsvcProvider;
pub use url::UrlProvider;
pub use windows_sdk::WindowsSdkProvider;
pub use zig::ZigProvider;

/// A single source of toolchain bytes (LLVM via GitHub release, NASM at a
/// fixed URL, Zig via index.json, MSVC via VS manifest, etc.).
pub trait Provider: Send + Sync {
    /// Stable string used in `[[toolchain]] provider = "..."`.
    fn id(&self) -> &'static str;

    /// Validate options + ensure the toolchain is present in the cache. Must
    /// be cheap when the cache is hit (i.e., short-circuit before any
    /// network call once the fingerprint is known to be deterministic from
    /// options).
    ///
    /// Provider responsibilities:
    ///  1. Validate `options`. Error on missing/invalid fields.
    ///  2. Compute fingerprint. If all options are pinned, this is
    ///     deterministic and requires no network. Otherwise (e.g.,
    ///     `msvc_version` omitted) the provider is allowed to make network
    ///     calls to resolve "latest".
    ///  3. If `<cache>/installs/<fingerprint>/metadata.toml` exists, return
    ///     `Installed { freshly_extracted: false, ... }`.
    ///  4. Otherwise: fetch payloads via `ctx.download(...)`, extract into
    ///     `ctx.staging_dir()`, return `Installed { freshly_extracted:
    ///     true, ... }`. The engine handles CAS pass + atomic commit.
    fn install(&self, options: &toml::Table, ctx: &mut InstallCtx) -> Result<Installed>;
}

/// What the provider tells the engine after `install()`.
#[derive(Debug, Clone)]
pub struct Installed {
    /// Sanitized at engine level. Provider can return its raw fingerprint
    /// (e.g., `github-foo-bar-llvmorg-21.1.6-asset+name`) and the engine
    /// will sanitize for filesystem use.
    pub fingerprint: String,
    /// Human-readable summary for `list` / logs (e.g., `"llvm 21.1.6
    /// (x86_64-pc-windows-msvc)"`).
    pub display: String,
    /// Resolved options to record in `metadata.toml`. Should be the
    /// user-supplied options plus any "latest" resolution (e.g., the
    /// concrete msvc_version when user omitted it).
    pub options: toml::Table,
    /// True if the provider populated `ctx.staging_dir()` and the engine
    /// must run the CAS pass + atomic commit. False for cache-hit fast path.
    pub freshly_extracted: bool,
}

/// Per-`install()` context. The engine constructs a fresh one each call.
pub struct InstallCtx {
    cache: Cache,
    /// Lazily allocated; `staging_dir()` creates this on first call.
    staging: RefCell<Option<PathBuf>>,
}

impl InstallCtx {
    pub fn new(cache: Cache) -> Self {
        Self {
            cache,
            staging: RefCell::new(None),
        }
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache.root
    }

    pub fn cache(&self) -> &Cache {
        &self.cache
    }

    /// Lazily create `<cache>/staging/<uuid>/raw/` on first call. Subsequent
    /// calls return the same path.
    pub fn staging_dir(&self) -> Result<PathBuf> {
        let mut guard = self.staging.borrow_mut();
        if let Some(p) = guard.as_ref() {
            return Ok(p.clone());
        }
        let staging_root = self.cache.allocate_staging()?;
        let raw = staging_root.join("raw");
        std::fs::create_dir_all(&raw)
            .with_context(|| format!("creating {}", raw.display()))?;
        *guard = Some(raw.clone());
        Ok(raw)
    }

    /// Returns the staging root (the `<cache>/staging/<uuid>/` directory),
    /// not the `raw/` subdirectory inside it. The engine uses this to
    /// build the install tree at `<staging>/tree/` next to `<staging>/raw/`.
    pub fn staging_root(&self) -> Option<PathBuf> {
        self.staging
            .borrow()
            .as_ref()
            .and_then(|raw| raw.parent().map(|p| p.to_path_buf()))
    }

    /// Fetch a URL into the shared `<cache>/downloads/` cache.
    pub fn download(&self, url: &str, expected_sha256: Option<&str>) -> Result<PathBuf> {
        download::fetch(url, expected_sha256, &self.cache.downloads)
    }

    /// Same as [`download`], but also reports whether the result came
    /// from cache or required a fresh fetch. Used by install loops that
    /// want to report a per-provider summary (e.g. "X downloaded, Y
    /// already cached").
    pub fn download_with_outcome(
        &self,
        url: &str,
        expected_sha256: Option<&str>,
    ) -> Result<(PathBuf, download::FetchSource)> {
        download::fetch_with_outcome(url, expected_sha256, &self.cache.downloads)
    }
}

/// Owns a set of provider implementations keyed by their `id()`.
pub struct ProviderRegistry {
    providers: HashMap<String, Box<dyn Provider>>,
}

impl ProviderRegistry {
    pub fn empty() -> Self {
        Self {
            providers: HashMap::new(),
        }
    }

    /// All built-in providers configured with production URLs.
    pub fn default() -> Self {
        let mut r = Self::empty();
        r.register(UrlProvider::new());
        r.register(GithubProvider::new());
        r.register(ZigProvider::new());
        r.register(MsvcProvider::new());
        r.register(WindowsSdkProvider::new());
        r
    }

    pub fn register<P: Provider + 'static>(&mut self, p: P) {
        self.providers.insert(p.id().to_string(), Box::new(p));
    }

    pub fn get(&self, id: &str) -> Option<&dyn Provider> {
        self.providers.get(id).map(|b| b.as_ref())
    }

    pub fn require(&self, id: &str) -> Result<&dyn Provider> {
        self.get(id).ok_or_else(|| {
            let known: Vec<_> = self.providers.keys().cloned().collect();
            anyhow!("no such provider: '{id}' (registered: {})", known.join(", "))
        })
    }

    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.providers.keys().map(|s| s.as_str())
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::default()
    }
}
#[cfg(test)]
#[path = "tests/providers.rs"]
mod tests;
