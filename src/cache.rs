//! Global cache layout: `installs/`, `cas/`, `downloads/`, `staging/`.
//!
//! Also owns fingerprint sanitization and the per-install `metadata.toml`
//! schema (read + write).

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::fs_util;

/// Current `metadata.toml` schema version. Incrementing requires a migration
/// story; v0.1 will hard-error on any other value.
pub const METADATA_SCHEMA_VERSION: u32 = 1;

/// All cache subdirectories under one root.
#[derive(Debug, Clone)]
pub struct Cache {
    pub root: PathBuf,
    pub installs: PathBuf,
    pub cas: PathBuf,
    pub downloads: PathBuf,
    pub staging: PathBuf,
}

impl Cache {
    /// Resolve cache layout. If `override_dir` is `Some`, use it. Else use
    /// `[settings] cache_dir`. Else fall back to the platform default
    /// (`~/.cache/natron` on Linux/macOS, `%LOCALAPPDATA%\natron\cache`
    /// on Windows).
    pub fn resolve(
        override_dir: Option<&Path>,
        config_setting: Option<&Path>,
    ) -> Result<Self> {
        let root = if let Some(p) = override_dir {
            p.to_path_buf()
        } else if let Some(p) = config_setting {
            p.to_path_buf()
        } else {
            default_cache_dir()?
        };
        Ok(Self::layout(root))
    }

    /// Build a Cache pointed at an explicit root (tests use this).
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self::layout(root.into())
    }

    fn layout(root: PathBuf) -> Self {
        let installs = root.join("installs");
        let cas = root.join("cas");
        let downloads = root.join("downloads");
        let staging = root.join("staging");
        Self {
            root,
            installs,
            cas,
            downloads,
            staging,
        }
    }

    /// Create all four subdirectories if missing. Verifies they share a
    /// filesystem volume (engine guarantees this since they're siblings of
    /// `root`, but we double-check on first init).
    pub fn ensure_layout(&self) -> Result<()> {
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("creating cache root {}", self.root.display()))?;
        for d in [&self.installs, &self.cas, &self.downloads, &self.staging] {
            std::fs::create_dir_all(d)
                .with_context(|| format!("creating {}", d.display()))?;
        }
        Ok(())
    }

    /// Path of a specific install dir given its sanitized fingerprint.
    pub fn install_dir(&self, fingerprint: &str) -> PathBuf {
        self.installs.join(fingerprint)
    }

    /// `tree/` inside an install dir — the actual extracted toolchain.
    pub fn install_tree(&self, fingerprint: &str) -> PathBuf {
        self.install_dir(fingerprint).join("tree")
    }

    /// `metadata.toml` inside an install dir.
    pub fn install_metadata_path(&self, fingerprint: &str) -> PathBuf {
        self.install_dir(fingerprint).join("metadata.toml")
    }

    /// CAS file path for a given content hash hex string.
    pub fn cas_path(&self, hex: &str) -> PathBuf {
        let (prefix, rest) = if hex.len() >= 2 {
            hex.split_at(2)
        } else {
            (hex, "")
        };
        self.cas.join(prefix).join(rest)
    }

    /// Returns true if an install with this fingerprint is present and has
    /// valid metadata. Caller reads metadata if it needs more than this.
    pub fn install_present(&self, fingerprint: &str) -> bool {
        self.install_metadata_path(fingerprint).is_file()
    }

    /// Allocate a fresh staging dir under `staging/<uuid>/`. Returns the
    /// absolute path. Creates the directory.
    pub fn allocate_staging(&self) -> Result<PathBuf> {
        let id = uuid::Uuid::new_v4();
        let path = self.staging.join(id.to_string());
        std::fs::create_dir_all(&path)
            .with_context(|| format!("creating staging dir {}", path.display()))?;
        Ok(path)
    }
}

/// Find the default cache directory: `~/.natron` on every platform.
///
/// One dot-directory in the user's home, matching the convention of
/// cross-project tools like `~/.cargo`, `~/.rustup`, `~/.gradle`. The cache
/// is shared across every project on the machine, so a home-dir location
/// makes more sense than a platform-specific app-data dir.
fn default_cache_dir() -> Result<PathBuf> {
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow!("could not determine user home directory"))?;
    Ok(home.join(".natron"))
}

/// Sanitize a fingerprint for use as a directory name.
///
/// Replace any character outside `[A-Za-z0-9._-]` with `_`. If sanitization
/// changes any byte, append a short hash of the *original* fingerprint so
/// that two distinct fingerprints can never collide on the sanitized form.
pub fn sanitize_fingerprint(raw: &str) -> String {
    let mut changed = false;
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
            out.push(c);
        } else {
            changed = true;
            out.push('_');
        }
    }
    if changed {
        let hash = xxhash_rust::xxh3::xxh3_64(raw.as_bytes());
        out.push('-');
        out.push_str(&format!("{:08x}", hash & 0xFFFF_FFFF));
    }
    out
}

/// `metadata.toml` body inside each `installs/<fingerprint>/`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallMetadata {
    pub schema_version: u32,
    pub provider: String,
    pub fingerprint: String,
    pub display: String,
    pub installed_at: toml::value::Datetime,
    pub tool_version: String,
    /// User-supplied options + resolved version. Stored as a nested table
    /// (not inline) for readability.
    #[serde(default)]
    pub options: toml::Table,
}

impl InstallMetadata {
    pub fn new(
        provider: impl Into<String>,
        fingerprint: impl Into<String>,
        display: impl Into<String>,
        options: toml::Table,
    ) -> Self {
        Self {
            schema_version: METADATA_SCHEMA_VERSION,
            provider: provider.into(),
            fingerprint: fingerprint.into(),
            display: display.into(),
            installed_at: now_datetime(),
            tool_version: env!("CARGO_PKG_VERSION").to_string(),
            options,
        }
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        let toml_str = toml::to_string_pretty(self)
            .with_context(|| format!("serializing metadata for {}", self.fingerprint))?;
        fs_util::atomic_write(path, toml_str.as_bytes())
    }

    pub fn read(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let md: InstallMetadata = toml::from_str(&text)
            .with_context(|| format!("parsing {}", path.display()))?;
        if md.schema_version != METADATA_SCHEMA_VERSION {
            bail!(
                "metadata at {} has schema_version={}, this natron expects {}; binary version mismatch",
                path.display(),
                md.schema_version,
                METADATA_SCHEMA_VERSION
            );
        }
        Ok(md)
    }
}

fn now_datetime() -> toml::value::Datetime {
    // Format: YYYY-MM-DDTHH:MM:SSZ
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let s = format_unix_seconds(secs);
    s.parse().unwrap_or_else(|_| {
        // Should never fail given our format, but degrade gracefully.
        "1970-01-01T00:00:00Z".parse().unwrap()
    })
}

/// Lightweight UTC formatter (avoids pulling chrono/time crate in for one fn).
fn format_unix_seconds(secs: u64) -> String {
    // Days since epoch + seconds within day.
    let days = (secs / 86_400) as i64;
    let sod = (secs % 86_400) as u32;
    let h = sod / 3600;
    let m = (sod / 60) % 60;
    let s = sod % 60;
    let (year, month, day) = ymd_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}Z"
    )
}

/// Days-since-1970-01-01 → (year, month, day). Standard civil-from-days
/// algorithm (Howard Hinnant).
fn ymd_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = (y + if m <= 2 { 1 } else { 0 }) as i32;
    (year, m as u32, d as u32)
}
#[cfg(test)]
#[path = "cache_tests.rs"]
mod tests;
