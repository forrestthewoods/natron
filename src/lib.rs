//! Natron — vendor compiler toolchains into source-controlled projects.

pub mod cache;
pub mod cas;
pub mod config;
pub mod deploy;
pub mod download;
pub mod engine;
pub mod extract;
pub mod fs_util;
pub mod providers;
pub mod state;

pub use cache::{Cache, InstallMetadata, sanitize_fingerprint};
pub use config::{ArchiveKind, Config, DeployMode, Settings, ToolchainEntry};
pub use engine::{
    EntryError, EntryOutcome, Natron, SyncAction, SyncOptions, SyncReport,
};
pub use providers::{
    GithubProvider, InstallCtx, Installed, Provider, ProviderRegistry, UrlProvider,
    ZigProvider,
};
pub use state::{DeployState, DeployedEntry, StateDiff};
