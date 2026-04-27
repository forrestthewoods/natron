//! Tests for `src/deploy.rs` (split out so the production
//! file shows only the implementation).

use super::*;
use std::path::PathBuf;
use tempfile::TempDir;

fn make_install_tree(root: &Path) -> PathBuf {
    let tree = root.join("tree");
    std::fs::create_dir_all(tree.join("bin")).unwrap();
    std::fs::write(tree.join("bin").join("clang"), b"BIN").unwrap();
    std::fs::write(tree.join("LICENSE"), b"LIC").unwrap();
    std::fs::create_dir_all(tree.join("share").join("doc")).unwrap();
    std::fs::write(tree.join("share").join("doc").join("README"), b"DOC").unwrap();
    tree
}

#[test]
fn deploy_copy_produces_independent_files() {
    let tmp = TempDir::new().unwrap();
    let tree = make_install_tree(tmp.path());
    let dest = tmp.path().join("project").join("toolchains").join("llvm");
    deploy(&tree, &dest, DeployMode::Copy).unwrap();

    // Verify contents.
    assert_eq!(std::fs::read(dest.join("bin").join("clang")).unwrap(), b"BIN");
    assert_eq!(std::fs::read(dest.join("LICENSE")).unwrap(), b"LIC");
    // Mutate dest, source must not change.
    std::fs::write(dest.join("LICENSE"), b"MODIFIED").unwrap();
    assert_eq!(std::fs::read(tree.join("LICENSE")).unwrap(), b"LIC");
    // Copy mode files must be writable (no readonly attr).
    let md = std::fs::metadata(dest.join("LICENSE")).unwrap();
    assert!(!md.permissions().readonly());
}

#[test]
fn deploy_hardlink_shares_inodes_on_unix() {
    let tmp = TempDir::new().unwrap();
    let tree = make_install_tree(tmp.path());
    let dest = tmp.path().join("project").join("toolchains").join("llvm");
    deploy(&tree, &dest, DeployMode::Hardlink).unwrap();

    assert_eq!(std::fs::read(dest.join("bin").join("clang")).unwrap(), b"BIN");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let a = std::fs::metadata(tree.join("LICENSE")).unwrap();
        let b = std::fs::metadata(dest.join("LICENSE")).unwrap();
        assert_eq!(a.ino(), b.ino(), "hardlinked files share inode");
    }
}

#[test]
fn deploy_symlink_makes_directory_link() {
    let tmp = TempDir::new().unwrap();
    let tree = make_install_tree(tmp.path());
    let dest = tmp.path().join("project").join("toolchains").join("llvm");
    deploy(&tree, &dest, DeployMode::Symlink).unwrap();
    // Read through the link.
    assert_eq!(
        std::fs::read(dest.join("bin").join("clang")).unwrap(),
        b"BIN"
    );
    // The link metadata is a symlink (or junction).
    let md = std::fs::symlink_metadata(&dest).unwrap();
    let ft = md.file_type();
    assert!(ft.is_symlink() || cfg!(windows));
}

#[test]
fn deploy_replaces_existing_dest() {
    let tmp = TempDir::new().unwrap();
    let tree = make_install_tree(tmp.path());
    let dest = tmp.path().join("dest");
    // Pre-existing junk at dest.
    std::fs::create_dir_all(&dest).unwrap();
    std::fs::write(dest.join("stale.txt"), b"old").unwrap();

    deploy(&tree, &dest, DeployMode::Copy).unwrap();
    assert!(!dest.join("stale.txt").exists());
    assert_eq!(std::fs::read(dest.join("LICENSE")).unwrap(), b"LIC");
}

#[test]
fn deploy_can_switch_mode() {
    let tmp = TempDir::new().unwrap();
    let tree = make_install_tree(tmp.path());
    let dest = tmp.path().join("dest");

    deploy(&tree, &dest, DeployMode::Copy).unwrap();
    assert!(dest.join("LICENSE").is_file());

    deploy(&tree, &dest, DeployMode::Symlink).unwrap();
    // After switch, dest should be a symlink resolving to tree.
    assert!(std::fs::symlink_metadata(&dest).unwrap().file_type().is_symlink() || cfg!(windows));
    assert_eq!(std::fs::read(dest.join("LICENSE")).unwrap(), b"LIC");

    deploy(&tree, &dest, DeployMode::Hardlink).unwrap();
    assert_eq!(std::fs::read(dest.join("LICENSE")).unwrap(), b"LIC");
}

#[test]
fn undeploy_removes_dest() {
    let tmp = TempDir::new().unwrap();
    let tree = make_install_tree(tmp.path());
    let dest = tmp.path().join("dest");
    deploy(&tree, &dest, DeployMode::Copy).unwrap();
    undeploy(&dest).unwrap();
    assert!(!dest.exists());
}

#[test]
fn undeploy_handles_missing_path() {
    let tmp = TempDir::new().unwrap();
    let dest = tmp.path().join("does-not-exist");
    undeploy(&dest).unwrap();
}

#[test]
fn undeploy_removes_symlink() {
    let tmp = TempDir::new().unwrap();
    let tree = make_install_tree(tmp.path());
    let dest = tmp.path().join("dest");
    deploy(&tree, &dest, DeployMode::Symlink).unwrap();
    undeploy(&dest).unwrap();
    assert!(!dest.exists() && std::fs::symlink_metadata(&dest).is_err());
}
