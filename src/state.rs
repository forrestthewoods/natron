//! Per-deploy state file (`.natron-state.toml`) inside the project's
//! `<settings.deploy_dir>/`. Records what fingerprint each `[[toolchain]].name`
//! is currently deployed under, plus mode and target.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::DeployMode;
use crate::fs_util;

pub const STATE_FILENAME: &str = ".natron-state.toml";
pub const STATE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DeployState {
    pub schema_version: u32,
    #[serde(default)]
    pub deployed: BTreeMap<String, DeployedEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployedEntry {
    pub fingerprint: String,
    pub deploy_dir: String,
    pub mode: DeployMode,
    /// Forward-slash absolute path to the cache install tree.
    pub target: String,
    pub deployed_at: toml::value::Datetime,
}

impl DeployState {
    pub fn new() -> Self {
        Self {
            schema_version: STATE_SCHEMA_VERSION,
            deployed: BTreeMap::new(),
        }
    }

    /// Path to the state file inside a given project deploy dir.
    pub fn path_in(deploy_dir: &Path) -> PathBuf {
        deploy_dir.join(STATE_FILENAME)
    }

    /// Read state from `deploy_dir/.natron-state.toml`. Missing file → empty
    /// state. Corrupt file → error.
    pub fn read(deploy_dir: &Path) -> Result<Self> {
        let path = Self::path_in(deploy_dir);
        if !path.exists() {
            return Ok(Self::new());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let state: DeployState = toml::from_str(&text)
            .with_context(|| format!("parsing {}", path.display()))?;
        if state.schema_version != STATE_SCHEMA_VERSION {
            bail!(
                "{} has schema_version={}, this natron expects {}",
                path.display(),
                state.schema_version,
                STATE_SCHEMA_VERSION
            );
        }
        Ok(state)
    }

    /// Atomically write to `deploy_dir/.natron-state.toml`.
    pub fn write(&self, deploy_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(deploy_dir)
            .with_context(|| format!("creating {}", deploy_dir.display()))?;
        let text = toml::to_string_pretty(self)
            .context("serializing deploy state")?;
        let path = Self::path_in(deploy_dir);
        fs_util::atomic_write(&path, text.as_bytes())
    }

    pub fn get(&self, name: &str) -> Option<&DeployedEntry> {
        self.deployed.get(name)
    }

    pub fn upsert(&mut self, name: impl Into<String>, entry: DeployedEntry) {
        self.deployed.insert(name.into(), entry);
    }

    pub fn remove(&mut self, name: &str) -> Option<DeployedEntry> {
        self.deployed.remove(name)
    }

    /// Result of comparing a planned install against current state.
    pub fn diff(
        &self,
        name: &str,
        fingerprint: &str,
        deploy_dir: &str,
        mode: DeployMode,
        deploy_path: &Path,
    ) -> StateDiff {
        let Some(existing) = self.deployed.get(name) else {
            return StateDiff::NotDeployed;
        };
        if existing.fingerprint != fingerprint
            || existing.deploy_dir != deploy_dir
            || existing.mode != mode
        {
            return StateDiff::Drift {
                old_deploy_dir: existing.deploy_dir.clone(),
            };
        }
        if !deploy_path.exists() {
            return StateDiff::DeployMissing;
        }
        if mode == DeployMode::Symlink {
            // The state's `target` is the canonical deploy target. Use it.
            let target = PathBuf::from(&existing.target);
            if !fs_util::symlink_points_to(deploy_path, &target) {
                return StateDiff::SymlinkBroken;
            }
        }
        StateDiff::UpToDate
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateDiff {
    /// Entry not present in state — never deployed before.
    NotDeployed,
    /// Fingerprint, mode, or deploy_dir changed — must clean up old and
    /// redeploy.
    Drift { old_deploy_dir: String },
    /// State says it's deployed but the on-disk dir is gone.
    DeployMissing,
    /// Symlink mode but the link no longer resolves to the recorded target.
    SymlinkBroken,
    /// All checks pass; skip this entry.
    UpToDate,
}

impl StateDiff {
    pub fn needs_redeploy(&self) -> bool {
        !matches!(self, StateDiff::UpToDate)
    }
}
#[cfg(test)]
#[path = "tests/state.rs"]
mod tests;
