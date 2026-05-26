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
//! Release assets have a stable public URL
//! (`{download_base}/{repo}/releases/download/{tag}/{asset}`), so we build it
//! directly instead of querying the rate-limited releases API. Tests construct
//! `GithubProvider::with_download_base("file:///path/to/fixtures")` to redirect
//! the asset download at a local fixture tree.

use anyhow::{Context, Result, anyhow};

use super::{InstallCtx, Installed, Provider};
use crate::cache::sanitize_fingerprint;
use crate::config::ArchiveKind;
use crate::extract;

pub const ID: &str = "github";

/// Default base for release-asset downloads. Real assets live under
/// `https://github.com/{repo}/releases/download/...`.
pub const DEFAULT_DOWNLOAD_BASE: &str = "https://github.com";

pub struct GithubProvider {
    download_base: String,
}

impl GithubProvider {
    pub fn new() -> Self {
        Self {
            download_base: DEFAULT_DOWNLOAD_BASE.to_string(),
        }
    }

    /// Override the download base (used by tests pointing at fixture
    /// directories served via `file://`).
    pub fn with_download_base(download_base: impl Into<String>) -> Self {
        Self {
            download_base: download_base.into(),
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

        // GitHub release assets have a stable, predictable download URL; build
        // it directly rather than querying the (rate-limited) releases API.
        let asset_url = format!(
            "{}/{}/releases/download/{}/{}",
            self.download_base.trim_end_matches('/'),
            repo,
            tag,
            asset,
        );
        let t_dl = std::time::Instant::now();
        let archive_path = ctx
            .download(&asset_url, sha256)
            .with_context(|| format!("downloading {asset} from {asset_url}"))?;
        tracing::info!(
            "[timing] github {asset}: download took {:.2}s",
            t_dl.elapsed().as_secs_f64()
        );

        // Extract into the staging dir.
        let staging_raw = ctx.staging_dir()?;
        let t_ex = std::time::Instant::now();
        extract::extract_archive(
            &archive_path,
            archive_kind,
            &staging_raw,
            strip_prefix.as_deref(),
        )?;
        tracing::info!(
            "[timing] github {asset}: extract took {:.2}s",
            t_ex.elapsed().as_secs_f64()
        );

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

#[cfg(test)]
#[path = "tests/github.rs"]
mod tests;
