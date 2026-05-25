//! Tests for `src/state.rs` (split out so the production
//! file shows only the implementation).

use super::*;
use tempfile::TempDir;

fn sample_entry(fp: &str, mode: DeployMode) -> DeployedEntry {
    DeployedEntry {
        fingerprint: fp.into(),
        deploy_dir: "llvm21".into(),
        mode,
        target: "/tmp/cache/installs/zz/tree".into(),
        deployed_at: "2026-04-26T00:00:00Z".parse().unwrap(),
    }
}

#[test]
fn read_missing_returns_empty() {
    let tmp = TempDir::new().unwrap();
    let state = DeployState::read(tmp.path()).unwrap();
    assert!(state.deployed.is_empty());
    assert_eq!(state.schema_version, STATE_SCHEMA_VERSION);
}

#[test]
fn write_then_read_round_trips() {
    let tmp = TempDir::new().unwrap();
    let mut state = DeployState::new();
    state.upsert("llvm21", sample_entry("github-fp-1", DeployMode::Hardlink));
    state.write(tmp.path()).unwrap();

    let loaded = DeployState::read(tmp.path()).unwrap();
    assert_eq!(loaded.deployed.len(), 1);
    let e = loaded.get("llvm21").unwrap();
    assert_eq!(e.fingerprint, "github-fp-1");
    assert_eq!(e.mode, DeployMode::Hardlink);
}

#[test]
fn diff_reports_not_deployed() {
    let state = DeployState::new();
    let d = state.diff("zig", "fp", "zig", DeployMode::Hardlink, Path::new("/nope"));
    assert_eq!(d, StateDiff::NotDeployed);
}

#[test]
fn diff_reports_drift_on_fingerprint_change() {
    let mut state = DeployState::new();
    state.upsert("llvm21", sample_entry("old-fp", DeployMode::Hardlink));
    let d = state.diff(
        "llvm21",
        "new-fp",
        "llvm21",
        DeployMode::Hardlink,
        Path::new("/nope"),
    );
    assert!(matches!(d, StateDiff::Drift { .. }));
}

#[test]
fn diff_reports_drift_on_mode_change() {
    let mut state = DeployState::new();
    state.upsert("llvm21", sample_entry("fp", DeployMode::Hardlink));
    let d = state.diff(
        "llvm21",
        "fp",
        "llvm21",
        DeployMode::Copy,
        Path::new("/nope"),
    );
    assert!(matches!(d, StateDiff::Drift { .. }));
}

#[test]
fn diff_reports_deploy_missing() {
    let mut state = DeployState::new();
    state.upsert("llvm21", sample_entry("fp", DeployMode::Hardlink));
    let d = state.diff(
        "llvm21",
        "fp",
        "llvm21",
        DeployMode::Hardlink,
        Path::new("/this/does/not/exist"),
    );
    assert_eq!(d, StateDiff::DeployMissing);
}

#[test]
fn diff_reports_up_to_date_when_dir_exists_for_hardlink() {
    let tmp = TempDir::new().unwrap();
    let deploy = tmp.path().join("dep");
    std::fs::create_dir_all(&deploy).unwrap();
    let mut state = DeployState::new();
    state.upsert("llvm21", sample_entry("fp", DeployMode::Hardlink));
    let d = state.diff("llvm21", "fp", "llvm21", DeployMode::Hardlink, &deploy);
    assert_eq!(d, StateDiff::UpToDate);
}

#[test]
fn rejects_unknown_schema_version() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(
        tmp.path().join(STATE_FILENAME),
        "schema_version = 999\n[deployed]\n",
    )
    .unwrap();
    let err = DeployState::read(tmp.path()).unwrap_err();
    assert!(err.to_string().contains("schema_version=999"));
}

#[test]
fn upsert_and_remove() {
    let mut state = DeployState::new();
    state.upsert("a", sample_entry("fp", DeployMode::Hardlink));
    assert!(state.get("a").is_some());
    let removed = state.remove("a").unwrap();
    assert_eq!(removed.fingerprint, "fp");
    assert!(state.get("a").is_none());
}
