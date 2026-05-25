//! Test harness shared across integration tests. Builds a tempdir-backed
//! Natron whose providers point at fixture URLs (via api_base override
//! and `file://` archive URLs) so the test suite is fully hermetic.

#![allow(dead_code)] // each integration test binary uses different parts

use natron::{
    Cache, Config, DeployMode, GithubProvider, Natron, ProviderRegistry, Settings,
    ToolchainEntry, UrlProvider, ZigProvider,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
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

    /// Build a Natron with a default-style registry. The github provider's
    /// api_base is set to a `file://` URL pointing at `<fixture_root>/api/`
    /// so tests can pre-populate fake release JSON there. The zig provider's
    /// index_url points at `<fixture_root>/zig-index.json`; tests that use
    /// the zig provider must call `write_zig_index_json` first.
    pub fn make_natron(&self, cfg: Config) -> Natron {
        let cache = Cache::at(self.cache_dir.clone());
        let mut reg = ProviderRegistry::empty();
        reg.register(UrlProvider::new());
        reg.register(GithubProvider::with_api_base(self.api_base()));
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

    /// API base URL used by the github provider in tests. Format:
    /// `file:///path/to/fixture_root/api` (no trailing slash).
    pub fn api_base(&self) -> String {
        let api_dir = self.fixture_root.join("api");
        std::fs::create_dir_all(&api_dir).ok();
        let url = url::Url::from_directory_path(&api_dir).unwrap().to_string();
        url.trim_end_matches('/').to_string()
    }

    /// Pre-populate a fake GitHub release-info JSON. The asset's
    /// browser_download_url points at the local archive.
    pub fn write_github_release_json(
        &self,
        repo: &str,
        tag: &str,
        asset_name: &str,
        archive_path: &Path,
    ) {
        let api_dir = self.fixture_root.join("api");
        let path = api_dir
            .join("repos")
            .join(repo)
            .join("releases")
            .join("tags")
            .join(tag);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let asset_url = url::Url::from_file_path(archive_path).unwrap().to_string();
        let json = format!(
            r#"{{"tag_name":"{tag}","assets":[{{"name":"{asset_name}","browser_download_url":"{asset_url}"}}]}}"#
        );
        std::fs::write(&path, json).unwrap();
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

/// Diff the `Windows Kits/10/` subtree of two directories, comparing files
/// by SHA-256. Returns `Ok(())` if the two subtrees contain the same set
/// of files with the same contents, otherwise `Err(msg)` with a
/// human-readable diff (capped at 50 entries per category).
///
/// Used by the MSI A/B test to confirm `extract_msi_pure` produces the
/// same on-disk output as `msiexec /a` for real SDK installs. Lives
/// here so it stays test-only and is reusable if other A/B comparisons
/// come up later. Will be deleted alongside the A/B test once the new
/// extractor is the only one.
pub fn diff_trees(old: &Path, new: &Path) -> Result<(), String> {
    let kit_old = old.join("Windows Kits").join("10");
    let kit_new = new.join("Windows Kits").join("10");
    let snap_old = snapshot_tree(&kit_old);
    let snap_new = snapshot_tree(&kit_new);
    if snap_old == snap_new {
        return Ok(());
    }

    let old_keys: BTreeSet<&PathBuf> = snap_old.keys().collect();
    let new_keys: BTreeSet<&PathBuf> = snap_new.keys().collect();
    let only_old: Vec<&PathBuf> = old_keys.difference(&new_keys).copied().collect();
    let only_new: Vec<&PathBuf> = new_keys.difference(&old_keys).copied().collect();
    let common: Vec<&PathBuf> = old_keys.intersection(&new_keys).copied().collect();
    let hash_differs: Vec<&PathBuf> = common
        .iter()
        .copied()
        .filter(|k| snap_old.get(*k) != snap_new.get(*k))
        .collect();

    const CAP: usize = 50;
    let mut msg = String::new();
    msg.push_str(&format!(
        "trees differ under Windows Kits/10/:\n  old: {} files\n  new: {} files\n  common keys: {}\n",
        snap_old.len(),
        snap_new.len(),
        common.len(),
    ));
    if !only_old.is_empty() {
        msg.push_str(&format!(
            "\nonly in old ({} total, showing first {}):\n",
            only_old.len(),
            only_old.len().min(CAP)
        ));
        for p in only_old.iter().take(CAP) {
            msg.push_str(&format!("  {}\n", p.display()));
        }
    }
    if !only_new.is_empty() {
        msg.push_str(&format!(
            "\nonly in new ({} total, showing first {}):\n",
            only_new.len(),
            only_new.len().min(CAP)
        ));
        for p in only_new.iter().take(CAP) {
            msg.push_str(&format!("  {}\n", p.display()));
        }
    }
    if !hash_differs.is_empty() {
        msg.push_str(&format!(
            "\nhash differs ({} total, showing first {}):\n",
            hash_differs.len(),
            hash_differs.len().min(CAP)
        ));
        for p in hash_differs.iter().take(CAP) {
            msg.push_str(&format!("  {}\n", p.display()));
        }
    }
    Err(msg)
}

fn snapshot_tree(root: &Path) -> BTreeMap<PathBuf, [u8; 32]> {
    let mut out = BTreeMap::new();
    if !root.exists() {
        return out;
    }
    for entry in jwalk::WalkDir::new(root).into_iter().flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        let abs = entry.path();
        let rel = match abs.strip_prefix(root) {
            Ok(r) => r.to_path_buf(),
            Err(_) => continue,
        };
        let bytes = match std::fs::read(&abs) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let mut sha = [0u8; 32];
        sha.copy_from_slice(&hasher.finalize());
        out.insert(rel, sha);
    }
    out
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
/// responsible for having pre-populated the matching release JSON via
/// `TestEnv::write_github_release_json`.
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
