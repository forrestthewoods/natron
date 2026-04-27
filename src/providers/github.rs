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
#[path = "github_tests.rs"]
mod tests;
