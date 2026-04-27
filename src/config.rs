//! TOML config (`natron.toml`) parsing, validation, path expansion, and discovery.
//!
//! Schema is intentionally small: a `[settings]` table and an array of
//! `[[toolchain]]` entries. Each entry has a `[toolchain.options]` sub-table
//! that the chosen provider validates.

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const CONFIG_FILENAME: &str = "natron.toml";

/// Root of a parsed `natron.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub settings: Settings,
    #[serde(default, rename = "toolchain")]
    pub toolchains: Vec<ToolchainEntry>,

    /// Absolute path to the directory that contained `natron.toml`. Used to
    /// resolve `settings.deploy_dir` if it is relative. Populated by `load`.
    #[serde(skip)]
    pub config_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// Where deployed toolchain dirs land. Relative paths are resolved from
    /// the config file's directory.
    #[serde(default = "default_deploy_dir")]
    pub deploy_dir: PathBuf,

    /// Default deploy mode for toolchains that don't override it.
    #[serde(default)]
    pub deploy_mode: DeployMode,

    /// Override the global cache directory. `~`, `${HOME}`, and `%USERPROFILE%`
    /// are expanded post-parse.
    #[serde(default)]
    pub cache_dir: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            deploy_dir: default_deploy_dir(),
            deploy_mode: DeployMode::default(),
            cache_dir: None,
        }
    }
}

fn default_deploy_dir() -> PathBuf {
    PathBuf::from("toolchains")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeployMode {
    Hardlink,
    Symlink,
    Copy,
}

impl Default for DeployMode {
    fn default() -> Self {
        // Symlink is the right default for deployment: instant, atomic
        // version swaps, cross-volume safe, and on Windows we transparently
        // fall back to a directory junction when symlink privilege is
        // missing. Hardlinks within the cache (CAS dedup) are unaffected
        // — that machinery is internal to <cache>/cas/ and <cache>/installs/.
        DeployMode::Symlink
    }
}

impl std::fmt::Display for DeployMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeployMode::Hardlink => f.write_str("hardlink"),
            DeployMode::Symlink => f.write_str("symlink"),
            DeployMode::Copy => f.write_str("copy"),
        }
    }
}

/// One `[[toolchain]]` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolchainEntry {
    /// Unique identifier. Used in state tracking and CLI args.
    pub name: String,
    /// Path under `settings.deploy_dir` where the toolchain is deployed.
    /// Two toolchains may not share the same `deploy_dir`.
    pub deploy_dir: String,
    /// Registered provider id (e.g. "github", "url", "zig", "msvc",
    /// "windows_sdk").
    pub provider: String,
    /// Optional per-toolchain override of the deploy mode.
    #[serde(default)]
    pub deploy_mode: Option<DeployMode>,
    /// Provider-specific options. Validated by the provider.
    #[serde(default)]
    pub options: toml::Table,
}

/// Recognized archive formats. MSVC's `.vsix` and `.msi` payloads are
/// provider-internal and never appear in user config — the `archive` field
/// is only legal for the `github` and `url` providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveKind {
    Zip,
    TarXz,
    TarGz,
}

impl ArchiveKind {
    /// Parse a config-supplied `archive` string.
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "zip" => Ok(Self::Zip),
            "tar.xz" => Ok(Self::TarXz),
            "tar.gz" => Ok(Self::TarGz),
            other => bail!(
                "unknown archive kind '{other}' (expected one of: zip, tar.xz, tar.gz)"
            ),
        }
    }

    /// Best-effort inference from a filename or URL path.
    pub fn infer_from_filename(name: &str) -> Option<Self> {
        let lower = name.to_lowercase();
        if lower.ends_with(".tar.xz") {
            Some(Self::TarXz)
        } else if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
            Some(Self::TarGz)
        } else if lower.ends_with(".zip") {
            Some(Self::Zip)
        } else {
            None
        }
    }
}

impl Config {
    /// Load + validate. If `path` is a file, load it. If `path` is a directory,
    /// search upward from `path` toward filesystem root for the first
    /// `natron.toml`.
    pub fn load(path: &Path) -> Result<Self> {
        let resolved = if path.is_file() {
            path.to_path_buf()
        } else if path.is_dir() {
            find_upward(path).ok_or_else(|| {
                anyhow!(
                    "no `{CONFIG_FILENAME}` found searching upward from {}",
                    path.display()
                )
            })?
        } else {
            bail!("config path does not exist: {}", path.display());
        };

        let text = std::fs::read_to_string(&resolved)
            .with_context(|| format!("reading {}", resolved.display()))?;
        let mut cfg: Config = toml::from_str(&text)
            .with_context(|| format!("parsing {}", resolved.display()))?;

        cfg.config_dir = resolved
            .parent()
            .ok_or_else(|| anyhow!("config has no parent dir: {}", resolved.display()))?
            .to_path_buf();

        cfg.expand_paths()?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Walk upward from `start_dir` looking for a `natron.toml`.
    pub fn discover(start_dir: &Path) -> Option<PathBuf> {
        find_upward(start_dir)
    }

    /// Path to the deploy dir, resolved against `config_dir`.
    pub fn resolved_deploy_dir(&self) -> PathBuf {
        if self.settings.deploy_dir.is_absolute() {
            self.settings.deploy_dir.clone()
        } else {
            self.config_dir.join(&self.settings.deploy_dir)
        }
    }

    /// Resolve the effective deploy mode for a given entry.
    pub fn effective_mode(&self, entry: &ToolchainEntry) -> DeployMode {
        entry.deploy_mode.unwrap_or(self.settings.deploy_mode)
    }

    fn expand_paths(&mut self) -> Result<()> {
        if let Some(p) = self.settings.cache_dir.take() {
            self.settings.cache_dir = Some(expand_user_path(&p)?);
        }
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        let mut names = HashSet::new();
        let mut deploy_dirs = HashSet::new();
        for entry in &self.toolchains {
            if entry.name.is_empty() {
                bail!("toolchain has empty `name`");
            }
            if entry.deploy_dir.is_empty() {
                bail!("toolchain '{}' has empty `deploy_dir`", entry.name);
            }
            if !names.insert(entry.name.clone()) {
                bail!("duplicate toolchain name: '{}'", entry.name);
            }
            if !deploy_dirs.insert(entry.deploy_dir.clone()) {
                bail!(
                    "duplicate deploy_dir: '{}' (used by '{}')",
                    entry.deploy_dir, entry.name
                );
            }
            // Provider id is checked when we look it up in the registry; we
            // can't check here because the registry is constructed by the
            // engine, not the config layer.
            if !is_valid_provider_id(&entry.provider) {
                bail!(
                    "toolchain '{}': invalid provider id '{}' (must be lowercase letters/digits/underscores)",
                    entry.name, entry.provider
                );
            }
        }
        Ok(())
    }
}

fn is_valid_provider_id(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn find_upward(start: &Path) -> Option<PathBuf> {
    let mut cur: Option<&Path> = Some(start);
    while let Some(dir) = cur {
        let candidate = dir.join(CONFIG_FILENAME);
        if candidate.is_file() {
            return Some(candidate);
        }
        cur = dir.parent();
    }
    None
}

/// Expand `~`, `${HOME}`, and `%USERPROFILE%` in a path. We only handle the
/// common leading-tilde shape for `~`; full glob-style tilde expansion is
/// out of scope.
pub fn expand_user_path(p: &Path) -> Result<PathBuf> {
    let s = p
        .to_str()
        .ok_or_else(|| anyhow!("non-UTF-8 path: {}", p.display()))?;
    let expanded = expand_user_str(s)?;
    Ok(PathBuf::from(expanded))
}

fn expand_user_str(s: &str) -> Result<String> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;

    // Leading ~ → home dir
    if let Some(stripped) = rest.strip_prefix('~') {
        let home = home_dir()?;
        out.push_str(home.to_str().unwrap_or(""));
        rest = stripped;
    }

    // Environment-style expansions: ${VAR} and %VAR% (for Windows familiarity)
    let bytes = rest.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // ${VAR}
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            if let Some(close) = rest[i + 2..].find('}') {
                let name = &rest[i + 2..i + 2 + close];
                match std::env::var(name) {
                    Ok(v) => out.push_str(&v),
                    Err(_) => bail!("env var '${{{name}}}' is not set"),
                }
                i += 2 + close + 1;
                continue;
            }
        }
        // %VAR%
        if bytes[i] == b'%' {
            if let Some(close) = rest[i + 1..].find('%') {
                let name = &rest[i + 1..i + 1 + close];
                if !name.is_empty() {
                    match std::env::var(name) {
                        Ok(v) => {
                            out.push_str(&v);
                            i += 1 + close + 1;
                            continue;
                        }
                        Err(_) => bail!("env var '%{name}%' is not set"),
                    }
                }
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    Ok(out)
}

fn home_dir() -> Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| anyhow!("could not determine user home directory"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_config(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join(CONFIG_FILENAME);
        std::fs::write(&path, body).unwrap();
        path
    }

    fn minimal_entry(name: &str, deploy_dir: &str) -> String {
        format!(
            r#"
[[toolchain]]
name = "{name}"
deploy_dir = "{deploy_dir}"
provider = "url"
[toolchain.options]
url = "https://example.com/{name}.zip"
"#
        )
    }

    #[test]
    fn parses_minimal_config() {
        let tmp = TempDir::new().unwrap();
        let body = format!("[settings]\n{}", minimal_entry("foo", "foo"));
        let path = write_config(tmp.path(), &body);
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.toolchains.len(), 1);
        assert_eq!(cfg.toolchains[0].name, "foo");
        assert_eq!(cfg.settings.deploy_mode, DeployMode::Symlink);
        assert_eq!(cfg.config_dir, tmp.path().canonicalize().unwrap_or(tmp.path().to_path_buf()).parent().map(|_| tmp.path().to_path_buf()).unwrap_or_else(|| tmp.path().to_path_buf()));
    }

    #[test]
    fn config_dir_is_set() {
        let tmp = TempDir::new().unwrap();
        let body = minimal_entry("foo", "foo");
        let path = write_config(tmp.path(), &body);
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.config_dir, tmp.path());
    }

    #[test]
    fn rejects_duplicate_name() {
        let tmp = TempDir::new().unwrap();
        let body = format!(
            "{}{}",
            minimal_entry("foo", "foo"),
            minimal_entry("foo", "bar")
        );
        let path = write_config(tmp.path(), &body);
        let err = Config::load(&path).unwrap_err();
        assert!(err.to_string().contains("duplicate toolchain name"));
    }

    #[test]
    fn rejects_duplicate_deploy_dir() {
        let tmp = TempDir::new().unwrap();
        let body = format!(
            "{}{}",
            minimal_entry("foo", "shared"),
            minimal_entry("bar", "shared")
        );
        let path = write_config(tmp.path(), &body);
        let err = Config::load(&path).unwrap_err();
        assert!(err.to_string().contains("duplicate deploy_dir"));
    }

    #[test]
    fn rejects_malformed_toml() {
        let tmp = TempDir::new().unwrap();
        let path = write_config(tmp.path(), "[[toolchain\nname = \"x\"\n");
        let err = Config::load(&path).unwrap_err();
        assert!(err.to_string().contains("parsing"));
    }

    #[test]
    fn rejects_invalid_provider_id() {
        let tmp = TempDir::new().unwrap();
        let body = r#"
[[toolchain]]
name = "x"
deploy_dir = "x"
provider = "GitHub-Capital"
[toolchain.options]
"#;
        let path = write_config(tmp.path(), body);
        let err = Config::load(&path).unwrap_err();
        assert!(err.to_string().contains("invalid provider id"));
    }

    #[test]
    fn rejects_empty_name() {
        let tmp = TempDir::new().unwrap();
        let body = r#"
[[toolchain]]
name = ""
deploy_dir = "x"
provider = "url"
[toolchain.options]
"#;
        let path = write_config(tmp.path(), body);
        let err = Config::load(&path).unwrap_err();
        assert!(err.to_string().contains("empty `name`"));
    }

    #[test]
    fn discovers_config_upward() {
        let tmp = TempDir::new().unwrap();
        let body = minimal_entry("foo", "foo");
        write_config(tmp.path(), &body);
        let nested = tmp.path().join("a").join("b").join("c");
        std::fs::create_dir_all(&nested).unwrap();
        let cfg = Config::load(&nested).unwrap();
        assert_eq!(cfg.toolchains[0].name, "foo");
    }

    #[test]
    fn discover_returns_none_when_missing() {
        let tmp = TempDir::new().unwrap();
        assert!(Config::discover(tmp.path()).is_none());
    }

    #[test]
    fn errors_when_config_path_does_not_exist() {
        let err = Config::load(Path::new("/this/definitely/does/not/exist")).unwrap_err();
        assert!(err.to_string().contains("does not exist"));
    }

    #[test]
    fn expands_tilde_in_cache_dir() {
        let tmp = TempDir::new().unwrap();
        let body = format!(
            r#"
[settings]
cache_dir = "~/some/where"
{}
"#,
            minimal_entry("foo", "foo")
        );
        let path = write_config(tmp.path(), &body);
        let cfg = Config::load(&path).unwrap();
        let cache = cfg.settings.cache_dir.unwrap();
        let cache_str = cache.to_string_lossy();
        assert!(!cache_str.starts_with('~'));
        assert!(cache_str.ends_with("some/where") || cache_str.ends_with("some\\where"));
    }

    #[test]
    fn expands_dollar_brace_var() {
        let env_key = "NATRON_TEST_EXPAND_VAR_42";
        // SAFETY: tests run in parallel but each uses a unique env var name.
        unsafe {
            std::env::set_var(env_key, "/tmp/expanded");
        }
        let expanded = expand_user_str("${NATRON_TEST_EXPAND_VAR_42}/sub").unwrap();
        assert_eq!(expanded, "/tmp/expanded/sub");
        unsafe {
            std::env::remove_var(env_key);
        }
    }

    #[test]
    fn errors_on_unset_env_var() {
        let err = expand_user_str("${THIS_VAR_IS_NOT_SET_84938}/x").unwrap_err();
        assert!(err.to_string().contains("not set"));
    }

    #[test]
    fn deploy_mode_default_is_symlink() {
        assert_eq!(DeployMode::default(), DeployMode::Symlink);
    }

    #[test]
    fn archive_kind_parse_and_infer() {
        assert_eq!(ArchiveKind::parse("zip").unwrap(), ArchiveKind::Zip);
        assert_eq!(ArchiveKind::parse("tar.xz").unwrap(), ArchiveKind::TarXz);
        assert_eq!(ArchiveKind::parse("tar.gz").unwrap(), ArchiveKind::TarGz);
        assert!(ArchiveKind::parse("rar").is_err());

        assert_eq!(
            ArchiveKind::infer_from_filename("foo.tar.xz"),
            Some(ArchiveKind::TarXz)
        );
        assert_eq!(
            ArchiveKind::infer_from_filename("foo.zip"),
            Some(ArchiveKind::Zip)
        );
        assert_eq!(
            ArchiveKind::infer_from_filename("foo.tgz"),
            Some(ArchiveKind::TarGz)
        );
        assert_eq!(ArchiveKind::infer_from_filename("foo.bin"), None);
    }

    #[test]
    fn effective_mode_uses_entry_override() {
        let cfg = Config {
            settings: Settings::default(),
            toolchains: vec![],
            config_dir: PathBuf::new(),
        };
        let entry = ToolchainEntry {
            name: "x".into(),
            deploy_dir: "x".into(),
            provider: "url".into(),
            deploy_mode: Some(DeployMode::Copy),
            options: toml::Table::new(),
        };
        assert_eq!(cfg.effective_mode(&entry), DeployMode::Copy);

        let entry2 = ToolchainEntry { deploy_mode: None, ..entry };
        assert_eq!(cfg.effective_mode(&entry2), DeployMode::Symlink);
    }
}
