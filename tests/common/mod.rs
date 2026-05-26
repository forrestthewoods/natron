//! Test harness shared across integration tests. Builds a tempdir-backed
//! Natron whose providers point at fixture URLs (via api_base override
//! and `file://` archive URLs) so the test suite is fully hermetic.

#![allow(dead_code)] // each integration test binary uses different parts

use natron::{
    Cache, Config, DeployMode, GithubProvider, Natron, ProviderRegistry, Settings,
    ToolchainEntry, UrlProvider, ZigProvider,
};
use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// One per integration test: a tempdir holding a project dir, a cache root,
/// and a fixture root. The harness keeps the TempDir alive so paths stay
/// valid for the test's lifetime.
pub struct TestEnv {
    pub temp: TempDir,
    pub project_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub fixture_root: PathBuf,
}

impl TestEnv {
    pub fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let project_dir = temp.path().join("project");
        let cache_dir = temp.path().join("cache");
        let fixture_root = temp.path().join("fixtures");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::create_dir_all(&fixture_root).unwrap();
        Self {
            temp,
            project_dir,
            cache_dir,
            fixture_root,
        }
    }

    /// Generate a synthetic zip archive under fixtures/ and return its path.
    pub fn make_zip(&self, name: &str, entries: &[(&str, &[u8])]) -> PathBuf {
        let path = self.fixture_root.join(name);
        let f = File::create(&path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opts = zip::write::FileOptions::default();
        for (entry_name, bytes) in entries {
            zw.start_file(*entry_name, opts).unwrap();
            zw.write_all(bytes).unwrap();
        }
        zw.finish().unwrap();
        path
    }

    /// Generate a synthetic tar.xz archive.
    pub fn make_tar_xz(&self, name: &str, entries: &[(&str, &[u8])]) -> PathBuf {
        let path = self.fixture_root.join(name);
        let f = File::create(&path).unwrap();
        let enc = liblzma::write::XzEncoder::new(f, 0);
        let mut tar = tar::Builder::new(enc);
        for (entry_name, bytes) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, *entry_name, *bytes).unwrap();
        }
        tar.finish().unwrap();
        path
    }

    /// Convert an absolute filesystem path into a `file://` URL string.
    pub fn file_url(path: &Path) -> String {
        url::Url::from_file_path(path).unwrap().to_string()
    }

    /// Build a Config with the given entries and the harness's deploy_dir.
    pub fn build_config(&self, entries: Vec<ToolchainEntry>) -> Config {
        let deploy_dir = self.project_dir.join("toolchains");
        Config {
            settings: Settings {
                deploy_dir: deploy_dir.clone(),
                deploy_mode: DeployMode::Hardlink,
                cache_dir: Some(self.cache_dir.clone()),
            },
            toolchains: entries,
            config_dir: self.project_dir.clone(),
        }
    }

    /// Build a Natron with a default-style registry. The github provider's
    /// download base is set to a `file://` URL pointing at `<fixture_root>/dl/`
    /// so tests can pre-place release assets there. The zig provider's
    /// index_url points at `<fixture_root>/zig-index.json`; tests that use
    /// the zig provider must call `write_zig_index_json` first.
    pub fn make_natron(&self, cfg: Config) -> Natron {
        let cache = Cache::at(self.cache_dir.clone());
        let mut reg = ProviderRegistry::empty();
        reg.register(UrlProvider::new());
        reg.register(GithubProvider::with_download_base(self.download_base()));
        reg.register(ZigProvider::with_index_url(self.zig_index_url()));
        Natron::new(cfg, cache, reg)
    }

    /// `file://` URL for the fake Zig index.json the test harness uses.
    pub fn zig_index_url(&self) -> String {
        let path = self.fixture_root.join("zig-index.json");
        // We may construct this URL before the file exists; that's fine
        // because url::Url::from_file_path doesn't require the path to
        // resolve. We DO ensure the parent dir exists.
        std::fs::create_dir_all(&self.fixture_root).ok();
        url::Url::from_file_path(&path).unwrap().to_string()
    }

    /// Write a fake Zig index.json. The single entry maps
    /// `(version, platform)` to a `tarball` file:// URL pointing at
    /// `archive_path` and a `shasum` of those bytes.
    pub fn write_zig_index_json(
        &self,
        version: &str,
        platform: &str,
        archive_path: &Path,
    ) {
        let path = self.fixture_root.join("zig-index.json");
        let archive_url = url::Url::from_file_path(archive_path).unwrap().to_string();
        // Compute sha256 of the archive bytes.
        let bytes = std::fs::read(archive_path).unwrap();
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let result = hasher.finalize();
        let mut sha = String::with_capacity(64);
        for b in result.iter() {
            use std::fmt::Write as _;
            write!(sha, "{b:02x}").unwrap();
        }
        let json = format!(
            r#"{{"{version}":{{"{platform}":{{"tarball":"{archive_url}","shasum":"{sha}"}}}}}}"#
        );
        std::fs::write(&path, json).unwrap();
    }

    /// Download base URL used by the github provider in tests. Format:
    /// `file:///path/to/fixture_root/dl` (no trailing slash).
    pub fn download_base(&self) -> String {
        let dir = self.fixture_root.join("dl");
        std::fs::create_dir_all(&dir).ok();
        let url = url::Url::from_directory_path(&dir).unwrap().to_string();
        url.trim_end_matches('/').to_string()
    }

    /// Pre-place a GitHub release asset where the provider builds its URL:
    /// `<download_base>/{repo}/releases/download/{tag}/{asset}`.
    pub fn place_github_asset(
        &self,
        repo: &str,
        tag: &str,
        asset_name: &str,
        archive_path: &Path,
    ) {
        let dst = self
            .fixture_root
            .join("dl")
            .join(repo)
            .join("releases")
            .join("download")
            .join(tag)
            .join(asset_name);
        std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
        std::fs::copy(archive_path, &dst).unwrap();
    }

    /// Build a Natron with a caller-provided registry (lets tests inject
    /// providers configured for fixture URLs).
    pub fn make_natron_with_registry(
        &self,
        cfg: Config,
        registry: ProviderRegistry,
    ) -> Natron {
        let cache = Cache::at(self.cache_dir.clone());
        Natron::new(cfg, cache, registry)
    }

    /// Path to the project's deploy_dir (where deployed entries land).
    pub fn deploy_root(&self) -> PathBuf {
        self.project_dir.join("toolchains")
    }
}

/// Build a `[[toolchain]]` entry that uses `provider = "url"` against the
/// given file path.
pub fn url_entry(name: &str, deploy_dir: &str, archive_path: &Path) -> ToolchainEntry {
    let mut opts = toml::Table::new();
    opts.insert(
        "url".into(),
        toml::Value::String(TestEnv::file_url(archive_path)),
    );
    ToolchainEntry {
        name: name.into(),
        deploy_dir: deploy_dir.into(),
        provider: "url".into(),
        deploy_mode: None,
        options: opts,
    }
}

/// Build a `[[toolchain]]` entry using `provider = "github"`. Caller is
/// responsible for having pre-placed the matching asset via
/// `TestEnv::place_github_asset`.
pub fn github_entry(
    name: &str,
    deploy_dir: &str,
    repo: &str,
    tag: &str,
    asset: &str,
) -> ToolchainEntry {
    let mut opts = toml::Table::new();
    opts.insert("repo".into(), toml::Value::String(repo.into()));
    opts.insert("tag".into(), toml::Value::String(tag.into()));
    opts.insert("asset".into(), toml::Value::String(asset.into()));
    ToolchainEntry {
        name: name.into(),
        deploy_dir: deploy_dir.into(),
        provider: "github".into(),
        deploy_mode: None,
        options: opts,
    }
}

/// Build a `[[toolchain]]` entry using `provider = "zig"`. Caller is
/// responsible for `TestEnv::write_zig_index_json` first.
pub fn zig_entry(
    name: &str,
    deploy_dir: &str,
    version: &str,
    platform: &str,
) -> ToolchainEntry {
    let mut opts = toml::Table::new();
    opts.insert("version".into(), toml::Value::String(version.into()));
    opts.insert("platform".into(), toml::Value::String(platform.into()));
    ToolchainEntry {
        name: name.into(),
        deploy_dir: deploy_dir.into(),
        provider: "zig".into(),
        deploy_mode: None,
        options: opts,
    }
}
