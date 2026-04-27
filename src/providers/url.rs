//! `url` provider: download a single archive from a fixed URL and extract.
//! Covers anything not on GitHub (NASM, etc.) and is the simplest provider.
//!
//! Accepts `http://`, `https://`, and `file://` URLs — the last makes
//! offline tests trivial.

use anyhow::{Context, Result, anyhow};

use super::{InstallCtx, Installed, Provider};
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
        let fingerprint = compute_fingerprint(url, &strip_prefix, archive_kind);

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
mod tests {
    use super::*;
    use crate::cache::Cache;
    use std::fs::File;
    use std::io::Write as _;
    use std::path::Path;
    use tempfile::TempDir;

    fn build_zip(path: &Path) {
        let f = File::create(path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opts = zip::write::FileOptions::default();
        zw.start_file("nasm.exe", opts).unwrap();
        zw.write_all(b"fake-nasm").unwrap();
        zw.start_file("LICENSE", opts).unwrap();
        zw.write_all(b"license-text").unwrap();
        zw.finish().unwrap();
    }

    #[test]
    fn url_provider_id() {
        assert_eq!(UrlProvider::new().id(), "url");
    }

    #[test]
    fn url_provider_installs_from_file_url() {
        let tmp = TempDir::new().unwrap();
        let archive = tmp.path().join("nasm-3.01.zip");
        build_zip(&archive);
        let cache = Cache::at(tmp.path().join("c"));
        cache.ensure_layout().unwrap();
        let mut ctx = InstallCtx::new(cache.clone());

        let url = url::Url::from_file_path(&archive).unwrap().to_string();
        let mut opts = toml::Table::new();
        opts.insert("url".into(), toml::Value::String(url));

        let provider = UrlProvider::new();
        let installed = provider.install(&opts, &mut ctx).unwrap();
        assert!(installed.freshly_extracted);
        assert!(installed.fingerprint.starts_with("url-nasm-3.01-"));

        // Staging contains the extracted files.
        let raw = ctx.staging_dir().unwrap();
        assert_eq!(std::fs::read(raw.join("nasm.exe")).unwrap(), b"fake-nasm");
    }

    #[test]
    fn url_provider_strip_prefix_works() {
        let tmp = TempDir::new().unwrap();
        let archive = tmp.path().join("foo.zip");
        let f = File::create(&archive).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opts = zip::write::FileOptions::default();
        zw.start_file("foo-1.0/bin/foo", opts).unwrap();
        zw.write_all(b"!").unwrap();
        zw.finish().unwrap();

        let cache = Cache::at(tmp.path().join("c"));
        cache.ensure_layout().unwrap();
        let mut ctx = InstallCtx::new(cache);

        let url = url::Url::from_file_path(&archive).unwrap().to_string();
        let mut opts = toml::Table::new();
        opts.insert("url".into(), toml::Value::String(url));
        opts.insert(
            "strip_prefix".into(),
            toml::Value::String("foo-1.0".into()),
        );

        let provider = UrlProvider::new();
        provider.install(&opts, &mut ctx).unwrap();
        let raw = ctx.staging_dir().unwrap();
        assert_eq!(std::fs::read(raw.join("bin").join("foo")).unwrap(), b"!");
    }

    #[test]
    fn url_provider_archive_kind_inference() {
        // Test the helper directly with various filenames.
        let mk = |archive: Option<&str>, url: &str| {
            let mut t = toml::Table::new();
            if let Some(a) = archive {
                t.insert("archive".into(), toml::Value::String(a.into()));
            }
            resolve_archive_kind(&t, url)
        };
        assert_eq!(mk(None, "https://x/foo.zip").unwrap(), ArchiveKind::Zip);
        assert_eq!(mk(None, "https://x/foo.tar.xz").unwrap(), ArchiveKind::TarXz);
        assert_eq!(mk(None, "https://x/foo.tgz").unwrap(), ArchiveKind::TarGz);
        assert!(mk(None, "https://x/foo.bin").is_err());
        // explicit archive overrides
        assert_eq!(
            mk(Some("zip"), "https://x/foo.bin").unwrap(),
            ArchiveKind::Zip
        );
    }

    #[test]
    fn url_provider_missing_url_errors() {
        let tmp = TempDir::new().unwrap();
        let cache = Cache::at(tmp.path().join("c"));
        cache.ensure_layout().unwrap();
        let mut ctx = InstallCtx::new(cache);
        let opts = toml::Table::new();
        let err = UrlProvider::new().install(&opts, &mut ctx).unwrap_err();
        assert!(err.to_string().contains("requires options.url"));
    }

    #[test]
    fn url_provider_sha_pin_mismatch_errors() {
        let tmp = TempDir::new().unwrap();
        let archive = tmp.path().join("foo.zip");
        build_zip(&archive);
        let cache = Cache::at(tmp.path().join("c"));
        cache.ensure_layout().unwrap();
        let mut ctx = InstallCtx::new(cache);

        let url = url::Url::from_file_path(&archive).unwrap().to_string();
        let mut opts = toml::Table::new();
        opts.insert("url".into(), toml::Value::String(url));
        opts.insert(
            "sha256".into(),
            toml::Value::String(
                "0000000000000000000000000000000000000000000000000000000000000000".into(),
            ),
        );

        let err = UrlProvider::new().install(&opts, &mut ctx).unwrap_err();
        assert!(err.to_string().contains("sha256 mismatch"));
    }

    #[test]
    fn url_provider_fingerprint_changes_with_strip_prefix() {
        let url = "https://example.com/foo.zip";
        let a = compute_fingerprint(url, &None, ArchiveKind::Zip);
        let b = compute_fingerprint(url, &Some("foo-1".into()), ArchiveKind::Zip);
        assert_ne!(a, b);
    }
}

