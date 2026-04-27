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
mod tests {
    use super::*;
    use crate::cache::Cache;
    use crate::download;
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

    fn write_index_json(
        index_path: &Path,
        version: &str,
        platform: &str,
        tarball_url: &str,
        sha: &str,
    ) {
        let json = format!(
            r#"{{"{version}":{{"{platform}":{{"tarball":"{tarball_url}","shasum":"{sha}"}}}}}}"#
        );
        std::fs::write(index_path, json).unwrap();
    }

    #[test]
    fn zig_provider_id() {
        assert_eq!(ZigProvider::new().id(), "zig");
    }

    #[test]
    fn zig_provider_full_install() {
        let tmp = TempDir::new().unwrap();
        // Build a synthetic zig zip with the conventional top-level dir.
        let archive = tmp.path().join("zig-windows-x86_64-0.15.2.zip");
        build_zip(
            &archive,
            &[
                ("zig-windows-x86_64-0.15.2/zig.exe", b"ZIG"),
                ("zig-windows-x86_64-0.15.2/lib/std.zig", b"STD"),
            ],
        );
        // Compute its sha so the index can claim a correct hash.
        let sha = download::sha256_of_file(&archive).unwrap();

        // Build a fake index.json pointing at the local archive.
        let index_path = tmp.path().join("index.json");
        let archive_url = url::Url::from_file_path(&archive).unwrap().to_string();
        write_index_json(
            &index_path,
            "0.15.2",
            "x86_64-windows",
            &archive_url,
            &sha,
        );
        let index_url = url::Url::from_file_path(&index_path).unwrap().to_string();

        let cache = Cache::at(tmp.path().join("c"));
        cache.ensure_layout().unwrap();
        let mut ctx = InstallCtx::new(cache);

        let mut opts = toml::Table::new();
        opts.insert("version".into(), toml::Value::String("0.15.2".into()));
        opts.insert("platform".into(), toml::Value::String("x86_64-windows".into()));

        let provider = ZigProvider::with_index_url(index_url);
        let installed = provider.install(&opts, &mut ctx).unwrap();
        assert!(installed.freshly_extracted);
        assert_eq!(installed.fingerprint, "zig-0.15.2-x86_64-windows");

        // Strip prefix should have been auto-derived; zig.exe sits at the
        // staging root rather than in a nested dir.
        let raw = ctx.staging_dir().unwrap();
        assert_eq!(fs::read(raw.join("zig.exe")).unwrap(), b"ZIG");
        assert_eq!(fs::read(raw.join("lib").join("std.zig")).unwrap(), b"STD");
        assert!(!raw.join("zig-windows-x86_64-0.15.2").exists());
    }

    #[test]
    fn zig_provider_missing_version_in_index() {
        let tmp = TempDir::new().unwrap();
        let index_path = tmp.path().join("index.json");
        std::fs::write(&index_path, r#"{"0.15.2":{"x86_64-linux":{"tarball":"x","shasum":"y"}}}"#).unwrap();
        let index_url = url::Url::from_file_path(&index_path).unwrap().to_string();

        let cache = Cache::at(tmp.path().join("c"));
        cache.ensure_layout().unwrap();
        let mut ctx = InstallCtx::new(cache);
        let mut opts = toml::Table::new();
        opts.insert("version".into(), toml::Value::String("9.9.9".into()));
        opts.insert("platform".into(), toml::Value::String("x86_64-linux".into()));
        let err = ZigProvider::with_index_url(index_url)
            .install(&opts, &mut ctx)
            .unwrap_err();
        assert!(err.to_string().contains("no entry"));
    }

    #[test]
    fn zig_provider_required_fields() {
        let tmp = TempDir::new().unwrap();
        let cache = Cache::at(tmp.path().join("c"));
        cache.ensure_layout().unwrap();
        let mut ctx = InstallCtx::new(cache);
        let opts = toml::Table::new();
        let err = ZigProvider::new().install(&opts, &mut ctx).unwrap_err();
        assert!(err.to_string().contains("version"));
    }

    #[test]
    fn zig_derive_strip_prefix_works() {
        assert_eq!(
            derive_strip_prefix("zig-windows-x86_64-0.15.2.zip", ArchiveKind::Zip),
            Some("zig-windows-x86_64-0.15.2".into())
        );
        assert_eq!(
            derive_strip_prefix("zig-linux-0.15.2.tar.xz", ArchiveKind::TarXz),
            Some("zig-linux-0.15.2".into())
        );
    }

    #[test]
    fn zig_filename_from_url() {
        assert_eq!(
            filename_from_url("https://ziglang.org/download/0.15.2/zig-windows-x86_64-0.15.2.zip")
                .unwrap(),
            "zig-windows-x86_64-0.15.2.zip"
        );
    }

    #[test]
    fn zig_provider_sha_mismatch_in_index() {
        let tmp = TempDir::new().unwrap();
        let archive = tmp.path().join("z.zip");
        build_zip(&archive, &[("zig.exe", b"X")]);
        let index_path = tmp.path().join("index.json");
        let archive_url = url::Url::from_file_path(&archive).unwrap().to_string();
        write_index_json(
            &index_path,
            "0.15.2",
            "p",
            &archive_url,
            "0000000000000000000000000000000000000000000000000000000000000000",
        );
        let index_url = url::Url::from_file_path(&index_path).unwrap().to_string();

        let cache = Cache::at(tmp.path().join("c"));
        cache.ensure_layout().unwrap();
        let mut ctx = InstallCtx::new(cache);
        let mut opts = toml::Table::new();
        opts.insert("version".into(), toml::Value::String("0.15.2".into()));
        opts.insert("platform".into(), toml::Value::String("p".into()));
        let err = ZigProvider::with_index_url(index_url)
            .install(&opts, &mut ctx)
            .unwrap_err();
        assert!(err.to_string().contains("sha256 mismatch"));
    }
}
