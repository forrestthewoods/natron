//! `url` provider: download a single archive from a fixed URL and extract.
//! Covers anything not on GitHub (NASM, etc.) and is the simplest provider.
//!
//! Accepts `http://`, `https://`, and `file://` URLs — the last makes
//! offline tests trivial.

use anyhow::{Context, Result, anyhow};

use super::{InstallCtx, Installed, Provider};
use crate::cache::sanitize_fingerprint;
use crate::config::ArchiveKind;
use crate::extract;

/// Provider id used in `[[toolchain]] provider = "url"`.
pub const ID: &str = "url";

pub struct UrlProvider;

impl UrlProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Default for UrlProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for UrlProvider {
    fn id(&self) -> &'static str {
        ID
    }

    fn install(
        &self,
        options: &toml::Table,
        ctx: &mut InstallCtx,
    ) -> Result<Installed> {
        let url = options
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("`url` provider requires options.url (string)"))?;
        let sha256 = options.get("sha256").and_then(|v| v.as_str());
        let archive_kind = resolve_archive_kind(options, url)?;
        let strip_prefix = options
            .get("strip_prefix")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Fingerprint is deterministic from URL (and strip_prefix +
        // archive_kind, since two configs differing only in those still
        // produce different install trees from the same bytes).
        let fingerprint =
            sanitize_fingerprint(&compute_fingerprint(url, &strip_prefix, archive_kind));

        // Cache hit fast path.
        if ctx.cache().install_present(&fingerprint) {
            return Ok(Installed {
                fingerprint,
                display: display(url, &archive_kind),
                options: resolved_options(options),
                freshly_extracted: false,
            });
        }

        // Fetch the archive.
        let archive = ctx.download(url, sha256)?;
        let staging_raw = ctx.staging_dir()?;
        extract::extract_archive(
            &archive,
            archive_kind,
            &staging_raw,
            strip_prefix.as_deref(),
        )?;

        Ok(Installed {
            fingerprint,
            display: display(url, &archive_kind),
            options: resolved_options(options),
            freshly_extracted: true,
        })
    }
}

fn resolve_archive_kind(options: &toml::Table, url: &str) -> Result<ArchiveKind> {
    if let Some(s) = options.get("archive").and_then(|v| v.as_str()) {
        return ArchiveKind::parse(s);
    }
    // Infer from URL filename.
    let parsed = url::Url::parse(url)
        .with_context(|| format!("parsing URL '{url}'"))?;
    let filename = parsed
        .path_segments()
        .and_then(|mut s| s.next_back())
        .unwrap_or("");
    ArchiveKind::infer_from_filename(filename).ok_or_else(|| {
        anyhow!(
            "could not infer archive kind from URL '{url}'; specify `archive = \"zip\" | \"tar.xz\" | \"tar.gz\"`"
        )
    })
}

fn compute_fingerprint(
    url: &str,
    strip_prefix: &Option<String>,
    kind: ArchiveKind,
) -> String {
    use xxhash_rust::xxh3::xxh3_64;
    let parsed = url::Url::parse(url).ok();
    let stem = parsed
        .as_ref()
        .and_then(|u| u.path_segments().and_then(|mut s| s.next_back()))
        .map(|s| s.trim_end_matches(".zip"))
        .map(|s| s.trim_end_matches(".tar.xz"))
        .map(|s| s.trim_end_matches(".tar.gz"))
        .map(|s| s.trim_end_matches(".tgz"))
        .unwrap_or("download")
        .to_string();
    let key = format!(
        "{url}|{kind:?}|{}",
        strip_prefix.as_deref().unwrap_or("")
    );
    let h = xxh3_64(key.as_bytes());
    format!("url-{stem}-{:08x}", h & 0xFFFF_FFFF)
}

fn display(url: &str, kind: &ArchiveKind) -> String {
    let parsed = url::Url::parse(url).ok();
    let stem = parsed
        .as_ref()
        .and_then(|u| u.path_segments().and_then(|mut s| s.next_back()))
        .unwrap_or("download")
        .to_string();
    format!("url {stem} ({kind:?})")
}

fn resolved_options(options: &toml::Table) -> toml::Table {
    // Pass through user options. Future: add a `resolved_archive` field if
    // we inferred it.
    options.clone()
}
#[cfg(test)]
#[path = "tests/url.rs"]
mod tests;
