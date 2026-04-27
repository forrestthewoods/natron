//! `zig` provider: looks up `version`+`platform` in ziglang.org's
//! `index.json`, downloads the tarball + sha-verifies via the index's
//! `shasum` field, and extracts.
//!
//! The official Zig archives nest everything under a top-level directory
//! like `zig-windows-x86_64-0.15.2/`. We auto-derive a `strip_prefix` from
//! the tarball filename so the deploy tree contains `zig.exe` directly
//! rather than `zig-windows-x86_64-0.15.2/zig.exe`. Override via
//! `options.strip_prefix = "..."` if you want a different layout.

use anyhow::{Context, Result, anyhow};
use serde_json::Value as Json;

use super::{InstallCtx, Installed, Provider};
use crate::cache::sanitize_fingerprint;
use crate::config::ArchiveKind;
use crate::extract;

pub const ID: &str = "zig";

pub const DEFAULT_INDEX_URL: &str = "https://ziglang.org/download/index.json";

pub struct ZigProvider {
    index_url: String,
}

impl ZigProvider {
    pub fn new() -> Self {
        Self {
            index_url: DEFAULT_INDEX_URL.to_string(),
        }
    }

    /// Override the index.json URL. Tests construct
    /// `ZigProvider::with_index_url(file_url)` to point at a fixture.
    pub fn with_index_url(index_url: impl Into<String>) -> Self {
        Self {
            index_url: index_url.into(),
        }
    }
}

impl Default for ZigProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for ZigProvider {
    fn id(&self) -> &'static str {
        ID
    }

    fn install(
        &self,
        options: &toml::Table,
        ctx: &mut InstallCtx,
    ) -> Result<Installed> {
        let version = require_str(options, "version")?;
        let platform = require_str(options, "platform")?;
        let strip_prefix_override = options
            .get("strip_prefix")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let fingerprint = sanitize_fingerprint(&format!("zig-{version}-{platform}"));

        if ctx.cache().install_present(&fingerprint) {
            return Ok(Installed {
                fingerprint,
                display: format!("zig {version} ({platform})"),
                options: options.clone(),
                freshly_extracted: false,
            });
        }

        // Fetch the index.
        let index_path = ctx
            .download(&self.index_url, None)
            .with_context(|| format!("fetching Zig index from {}", self.index_url))?;
        let index_text = std::fs::read_to_string(&index_path)
            .with_context(|| format!("reading {}", index_path.display()))?;
        let index: Json = serde_json::from_str(&index_text)
            .with_context(|| format!("parsing Zig index JSON from {}", index_path.display()))?;

        let entry = index
            .get(version)
            .and_then(|v| v.get(platform))
            .ok_or_else(|| {
                anyhow!(
                    "Zig index has no entry for version='{version}', platform='{platform}'"
                )
            })?;

        let tarball = entry
            .get("tarball")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("Zig index entry missing `tarball` URL"))?;
        let shasum = entry
            .get("shasum")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("Zig index entry missing `shasum`"))?;

        let archive_filename = filename_from_url(tarball)?;
        let archive_kind = ArchiveKind::infer_from_filename(&archive_filename)
            .ok_or_else(|| {
                anyhow!(
                    "could not infer archive kind from Zig tarball '{archive_filename}'"
                )
            })?;
        let strip_prefix = strip_prefix_override
            .or_else(|| derive_strip_prefix(&archive_filename, archive_kind));

        let archive_path = ctx.download(tarball, Some(shasum))?;
        let staging_raw = ctx.staging_dir()?;
        extract::extract_archive(
            &archive_path,
            archive_kind,
            &staging_raw,
            strip_prefix.as_deref(),
        )?;

        Ok(Installed {
            fingerprint,
            display: format!("zig {version} ({platform})"),
            options: options.clone(),
            freshly_extracted: true,
        })
    }
}

fn require_str<'a>(options: &'a toml::Table, key: &str) -> Result<&'a str> {
    options
        .get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("`zig` provider requires options.{key} (string)"))
}

fn filename_from_url(url: &str) -> Result<String> {
    let parsed = url::Url::parse(url)
        .with_context(|| format!("parsing tarball URL '{url}'"))?;
    let last = parsed
        .path_segments()
        .and_then(|mut s| s.next_back())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("could not extract filename from URL '{url}'"))?;
    Ok(last)
}

/// Derive a strip_prefix by stripping the archive extension from the filename.
fn derive_strip_prefix(filename: &str, kind: ArchiveKind) -> Option<String> {
    let stem = match kind {
        ArchiveKind::Zip => filename.strip_suffix(".zip"),
        ArchiveKind::TarXz => filename.strip_suffix(".tar.xz"),
        ArchiveKind::TarGz => filename
            .strip_suffix(".tar.gz")
            .or_else(|| filename.strip_suffix(".tgz")),
    };
    stem.map(|s| s.to_string())
}
#[cfg(test)]
#[path = "zig_tests.rs"]
mod tests;
