//! Test harness shared across integration tests. Builds a tempdir-backed
//! Natron whose providers point at fixture URLs (via api_base override
//! and `file://` archive URLs) so the test suite is fully hermetic.

#![allow(dead_code)] // each integration test binary uses different parts

use natron::{
    Cache, Config, DeployMode, Natron, ProviderRegistry, Settings, ToolchainEntry,
    UrlProvider,
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
        let enc = xz2::write::XzEncoder::new(f, 0);
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

    /// Build a Natron with a default registry (currently includes the url
    /// provider). For tests that need github / zig overrides, use
    /// `make_natron_with_registry`.
    pub fn make_natron(&self, cfg: Config) -> Natron {
        let cache = Cache::at(self.cache_dir.clone());
        let mut reg = ProviderRegistry::empty();
        reg.register(UrlProvider::new());
        Natron::new(cfg, cache, reg)
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
