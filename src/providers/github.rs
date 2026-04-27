//! `github` provider: download a release asset from a GitHub release.
//!
//! Required options:
//!   - `repo`: "owner/name" (e.g. "llvm/llvm-project")
//!   - `tag`: release tag (e.g. "llvmorg-21.1.6")
//!   - `asset`: asset filename (e.g. "clang+llvm-21.1.6-x86_64-pc-windows-msvc.tar.xz")
//!
//! Optional:
//!   - `version` — display-only.
//!   - `sha256` — pin the asset bytes.
//!   - `archive` — explicit archive type; inferred from `asset` extension if omitted.
//!   - `strip_prefix` — top-level directory to strip during extraction.
//!
//! Tests construct `GithubProvider::with_api_base("file:///path/to/fixtures")`
//! to redirect the release-info HTTP call at a local fixture file.

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

use super::{InstallCtx, Installed, Provider};
use crate::cache::sanitize_fingerprint;
use crate::config::ArchiveKind;
use crate::extract;

pub const ID: &str = "github";

/// Default GitHub API base URL.
pub const DEFAULT_API_BASE: &str = "https://api.github.com";

pub struct GithubProvider {
    api_base: String,
}

impl GithubProvider {
    pub fn new() -> Self {
        Self {
            api_base: DEFAULT_API_BASE.to_string(),
        }
    }

    /// Override the API base URL (used by tests pointing at fixture
    /// directories served via `file://`).
    pub fn with_api_base(api_base: impl Into<String>) -> Self {
        Self {
            api_base: api_base.into(),
        }
    }
}

impl Default for GithubProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for GithubProvider {
    fn id(&self) -> &'static str {
        ID
    }

    fn install(
        &self,
        options: &toml::Table,
        ctx: &mut InstallCtx,
    ) -> Result<Installed> {
        let repo = require_str(options, "repo")?;
        let tag = require_str(options, "tag")?;
        let asset = require_str(options, "asset")?;
        let sha256 = options.get("sha256").and_then(|v| v.as_str());
        let version_display = options.get("version").and_then(|v| v.as_str());
        let strip_prefix = options
            .get("strip_prefix")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let archive_kind = resolve_archive_kind(options, asset)?;

        let fingerprint = sanitize_fingerprint(&compute_fingerprint(repo, tag, asset));

        // Cache hit fast path.
        if ctx.cache().install_present(&fingerprint) {
            return Ok(Installed {
                fingerprint,
                display: display(repo, tag, version_display, asset),
                options: options.clone(),
                freshly_extracted: false,
            });
        }

        // Fetch the release JSON from {api_base}/repos/{repo}/releases/tags/{tag}.
        let release_url = format!(
            "{}/repos/{}/releases/tags/{}",
            self.api_base.trim_end_matches('/'),
            repo,
            tag
        );
        let release_path = ctx
            .download(&release_url, None)
            .with_context(|| format!("fetching GitHub release info from {release_url}"))?;
        let release_text = std::fs::read_to_string(&release_path)
            .with_context(|| format!("reading {}", release_path.display()))?;
        let release: ReleaseResponse = serde_json::from_str(&release_text)
            .with_context(|| format!("parsing release JSON from {}", release_path.display()))?;

        let asset_url = release
            .assets
            .into_iter()
            .find(|a| a.name == asset)
            .ok_or_else(|| {
                anyhow!(
                    "asset '{asset}' not found in release {}/{tag} (api_base={})",
                    repo,
                    self.api_base
                )
            })?
            .browser_download_url;

        // Download the asset itself, sha-verifying if pinned.
        let archive_path = ctx.download(&asset_url, sha256)?;

        // Extract into the staging dir.
        let staging_raw = ctx.staging_dir()?;
        extract::extract_archive(
            &archive_path,
            archive_kind,
            &staging_raw,
            strip_prefix.as_deref(),
        )?;

        Ok(Installed {
            fingerprint,
            display: display(repo, tag, version_display, asset),
            options: options.clone(),
            freshly_extracted: true,
        })
    }
}

fn require_str<'a>(options: &'a toml::Table, key: &str) -> Result<&'a str> {
    options
        .get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("`github` provider requires options.{key} (string)"))
}

fn resolve_archive_kind(options: &toml::Table, asset: &str) -> Result<ArchiveKind> {
    if let Some(s) = options.get("archive").and_then(|v| v.as_str()) {
        return ArchiveKind::parse(s);
    }
    ArchiveKind::infer_from_filename(asset).ok_or_else(|| {
        anyhow!(
            "could not infer archive kind from asset '{asset}'; specify `archive = \"zip\" | \"tar.xz\" | \"tar.gz\"`"
        )
    })
}

fn compute_fingerprint(repo: &str, tag: &str, asset: &str) -> String {
    // Strip extensions for readability; the engine sanitizes anyway.
    let stem = asset
        .trim_end_matches(".tar.xz")
        .trim_end_matches(".tar.gz")
        .trim_end_matches(".tgz")
        .trim_end_matches(".zip");
    format!("github-{repo}-{tag}-{stem}")
}

fn display(repo: &str, tag: &str, version: Option<&str>, asset: &str) -> String {
    let label = version.unwrap_or(tag);
    let stem = asset
        .trim_end_matches(".tar.xz")
        .trim_end_matches(".tar.gz")
        .trim_end_matches(".tgz")
        .trim_end_matches(".zip");
    format!("github {repo} {label} ({stem})")
}

#[derive(Debug, Deserialize)]
struct ReleaseResponse {
    #[serde(default)]
    #[allow(dead_code)] // we don't currently use tag_name; keep so JSON shape matches
    tag_name: Option<String>,
    #[serde(default)]
    assets: Vec<AssetEntry>,
}

#[derive(Debug, Deserialize)]
struct AssetEntry {
    name: String,
    browser_download_url: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::Cache;
    use std::fs;
    use std::fs::File;
    use std::io::Write as _;
    use std::path::Path;
    use tempfile::TempDir;

    fn build_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let f = File::create(path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opts = zip::write::FileOptions::default();
        for (name, bytes) in entries {
            zw.start_file(*name, opts).unwrap();
            zw.write_all(bytes).unwrap();
        }
        zw.finish().unwrap();
    }

    /// Write a fake GitHub release-info JSON. The asset's
    /// browser_download_url points at the local archive file.
    fn write_release_json(
        api_base_dir: &Path,
        repo: &str,
        tag: &str,
        asset_name: &str,
        archive_path: &Path,
    ) {
        let path = api_base_dir
            .join("repos")
            .join(repo)
            .join("releases")
            .join("tags")
            .join(tag);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let asset_url = url::Url::from_file_path(archive_path).unwrap().to_string();
        let json = format!(
            r#"{{"tag_name":"{tag}","assets":[{{"name":"{asset_name}","browser_download_url":"{asset_url}"}}]}}"#
        );
        fs::write(&path, json).unwrap();
    }

    #[test]
    fn github_provider_id() {
        assert_eq!(GithubProvider::new().id(), "github");
    }

    #[test]
    fn github_provider_resolves_release_and_extracts() {
        let tmp = TempDir::new().unwrap();
        let api = tmp.path().join("api");
        let archive = tmp.path().join("clang.zip");
        build_zip(&archive, &[("bin/clang.exe", b"BIN"), ("LICENSE", b"LIC")]);
        write_release_json(&api, "llvm/llvm-project", "llvmorg-21.1.6", "clang.zip", &archive);

        let api_base = url::Url::from_directory_path(&api).unwrap().to_string();
        // Trim trailing slash because we add one when constructing URLs.
        let api_base = api_base.trim_end_matches('/').to_string();

        let cache = Cache::at(tmp.path().join("c"));
        cache.ensure_layout().unwrap();
        let mut ctx = InstallCtx::new(cache);

        let mut opts = toml::Table::new();
        opts.insert("repo".into(), toml::Value::String("llvm/llvm-project".into()));
        opts.insert("tag".into(), toml::Value::String("llvmorg-21.1.6".into()));
        opts.insert("asset".into(), toml::Value::String("clang.zip".into()));

        let provider = GithubProvider::with_api_base(api_base);
        let installed = provider.install(&opts, &mut ctx).unwrap();
        assert!(installed.freshly_extracted);
        assert!(installed.fingerprint.contains("llvmorg-21.1.6"));

        let raw = ctx.staging_dir().unwrap();
        assert_eq!(fs::read(raw.join("bin").join("clang.exe")).unwrap(), b"BIN");
    }

    #[test]
    fn github_provider_missing_asset_errors() {
        let tmp = TempDir::new().unwrap();
        let api = tmp.path().join("api");
        let archive = tmp.path().join("real.zip");
        build_zip(&archive, &[("f", b"F")]);
        write_release_json(&api, "owner/repo", "v1", "real.zip", &archive);

        let api_base = url::Url::from_directory_path(&api).unwrap().to_string();
        let api_base = api_base.trim_end_matches('/').to_string();

        let cache = Cache::at(tmp.path().join("c"));
        cache.ensure_layout().unwrap();
        let mut ctx = InstallCtx::new(cache);

        let mut opts = toml::Table::new();
        opts.insert("repo".into(), toml::Value::String("owner/repo".into()));
        opts.insert("tag".into(), toml::Value::String("v1".into()));
        opts.insert("asset".into(), toml::Value::String("does-not-exist.zip".into()));

        let provider = GithubProvider::with_api_base(api_base);
        let err = provider.install(&opts, &mut ctx).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn github_provider_required_fields() {
        let tmp = TempDir::new().unwrap();
        let cache = Cache::at(tmp.path().join("c"));
        cache.ensure_layout().unwrap();
        let mut ctx = InstallCtx::new(cache);
        let opts = toml::Table::new();
        let err = GithubProvider::new().install(&opts, &mut ctx).unwrap_err();
        assert!(err.to_string().contains("repo"));
    }

    #[test]
    fn github_fingerprint_deterministic() {
        let a = compute_fingerprint("llvm/llvm-project", "llvmorg-21.1.6", "clang.tar.xz");
        let b = compute_fingerprint("llvm/llvm-project", "llvmorg-21.1.6", "clang.tar.xz");
        assert_eq!(a, b);
        let c = compute_fingerprint("llvm/llvm-project", "llvmorg-18.1.8", "clang.tar.xz");
        assert_ne!(a, c);
    }

    #[test]
    fn github_archive_kind_inference() {
        let mk = |asset: &str| {
            let opts = toml::Table::new();
            resolve_archive_kind(&opts, asset)
        };
        assert_eq!(mk("foo.tar.xz").unwrap(), ArchiveKind::TarXz);
        assert_eq!(mk("foo.zip").unwrap(), ArchiveKind::Zip);
        assert!(mk("foo.exe").is_err());
    }

    #[test]
    fn github_cache_hit_short_circuits() {
        let tmp = TempDir::new().unwrap();
        let api = tmp.path().join("api");
        let archive = tmp.path().join("a.zip");
        build_zip(&archive, &[("f", b"F")]);
        write_release_json(&api, "x/y", "v", "a.zip", &archive);
        let api_base = url::Url::from_directory_path(&api).unwrap().to_string();
        let api_base = api_base.trim_end_matches('/').to_string();

        let cache = Cache::at(tmp.path().join("c"));
        cache.ensure_layout().unwrap();
        let mut opts = toml::Table::new();
        opts.insert("repo".into(), toml::Value::String("x/y".into()));
        opts.insert("tag".into(), toml::Value::String("v".into()));
        opts.insert("asset".into(), toml::Value::String("a.zip".into()));

        // Pre-plant the install dir + metadata to simulate a prior install.
        let fp = compute_fingerprint("x/y", "v", "a.zip");
        let sanitized = crate::cache::sanitize_fingerprint(&fp);
        let install_dir = cache.install_dir(&sanitized);
        fs::create_dir_all(install_dir.join("tree")).unwrap();
        let md = crate::cache::InstallMetadata::new(
            "github",
            sanitized.clone(),
            "test",
            opts.clone(),
        );
        md.write(&cache.install_metadata_path(&sanitized)).unwrap();

        let mut ctx = InstallCtx::new(cache);
        let provider = GithubProvider::with_api_base(api_base);
        let installed = provider.install(&opts, &mut ctx).unwrap();
        assert!(!installed.freshly_extracted, "should have hit cache");
        // No staging dir should have been allocated.
        assert!(ctx.staging_root().is_none());
    }
}
