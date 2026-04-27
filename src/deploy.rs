//! Deploy a cache install tree into a project's `<settings.deploy_dir>/<name>/`
//! using one of three modes: `hardlink`, `symlink`, `copy`.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

use crate::config::DeployMode;
use crate::fs_util;

/// Deploy `cache_tree` (an install's `tree/` directory) into `dest` using
/// `mode`. `dest` is removed-and-recreated; callers are expected to do this
/// only when the fast path determined a redeploy is needed.
pub fn deploy(cache_tree: &Path, dest: &Path, mode: DeployMode) -> Result<()> {
    if dest.exists() || dest.is_symlink() {
        fs_util::remove_dir_all_writable(dest).with_context(|| {
            format!("removing existing deploy dir {}", dest.display())
        })?;
        // remove_dir_all on a symlink-as-dir may not delete on Windows; we
        // also explicitly try to remove the link.
        let _ = std::fs::remove_dir(dest);
        let _ = std::fs::remove_file(dest);
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    match mode {
        DeployMode::Symlink => deploy_symlink(cache_tree, dest),
        DeployMode::Hardlink => deploy_hardlink(cache_tree, dest),
        DeployMode::Copy => deploy_copy(cache_tree, dest),
    }
}

fn deploy_symlink(cache_tree: &Path, dest: &Path) -> Result<()> {
    fs_util::dir_symlink(cache_tree, dest)
}

fn deploy_hardlink(cache_tree: &Path, dest: &Path) -> Result<()> {
    // Pre-flight same-volume check.
    let parent = dest
        .parent()
        .ok_or_else(|| anyhow::anyhow!("no parent for {}", dest.display()))?;
    if !fs_util::same_volume(cache_tree, parent)? {
        bail!(
            "hardlink mode requires deploy dir {} to be on the same filesystem as the cache ({}); use --mode copy or --mode symlink",
            parent.display(),
            cache_tree.display()
        );
    }
    std::fs::create_dir_all(dest)?;
    walk_and_apply(cache_tree, dest, &|src, dst, ft| {
        if ft.is_dir() {
            std::fs::create_dir_all(dst)?;
        } else if ft.is_symlink() {
            reproduce_symlink(src, dst)?;
        } else if ft.is_file() {
            // Don't try to hardlink a symlink — the walk routes symlinks
            // through reproduce_symlink. Plain files only here.
            fs_util::hard_link(src, dst)?;
        }
        Ok(())
    })
}

fn deploy_copy(cache_tree: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    walk_and_apply(cache_tree, dest, &|src, dst, ft| {
        if ft.is_dir() {
            std::fs::create_dir_all(dst)?;
        } else if ft.is_symlink() {
            reproduce_symlink(src, dst)?;
        } else if ft.is_file() {
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(src, dst).with_context(|| {
                format!("copy {} -> {}", src.display(), dst.display())
            })?;
            // Copy mode: explicitly clear readonly so user can edit/commit.
            let _ = fs_util::clear_readonly(dst);
        }
        Ok(())
    })
}

/// Walk `from` and call `apply(src, dst, file_type)` for every entry.
/// `dst` is the path inside `to` mirroring `src`'s position in `from`.
fn walk_and_apply(
    from: &Path,
    to: &Path,
    apply: &dyn Fn(&Path, &Path, FileTypeKind) -> Result<()>,
) -> Result<()> {
    for entry in jwalk::WalkDir::new(from)
        .skip_hidden(false)
        .follow_links(false)
        .sort(true)
    {
        let entry = entry?;
        let src = entry.path();
        let rel = src.strip_prefix(from)?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        let dst = to.join(rel);
        let ft = FileTypeKind::from(&entry);
        apply(&src, &dst, ft)?;
    }
    Ok(())
}

#[derive(Copy, Clone)]
enum FileTypeKind {
    Dir,
    File,
    Symlink,
    Other,
}

impl FileTypeKind {
    fn is_dir(&self) -> bool {
        matches!(self, FileTypeKind::Dir)
    }
    fn is_file(&self) -> bool {
        matches!(self, FileTypeKind::File)
    }
    fn is_symlink(&self) -> bool {
        matches!(self, FileTypeKind::Symlink)
    }
}

impl<C: jwalk::ClientState> From<&jwalk::DirEntry<C>> for FileTypeKind {
    fn from(e: &jwalk::DirEntry<C>) -> Self {
        let ft = e.file_type();
        if ft.is_symlink() {
            FileTypeKind::Symlink
        } else if ft.is_dir() {
            FileTypeKind::Dir
        } else if ft.is_file() {
            FileTypeKind::File
        } else {
            FileTypeKind::Other
        }
    }
}

fn reproduce_symlink(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let target = std::fs::read_link(src)
        .with_context(|| format!("read_link {}", src.display()))?;
    let _ = std::fs::remove_file(dst);
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&target, dst).with_context(|| {
            format!("symlink {} -> {}", dst.display(), target.display())
        })?;
    }
    #[cfg(windows)]
    {
        let r = std::os::windows::fs::symlink_file(&target, dst)
            .or_else(|_| std::os::windows::fs::symlink_dir(&target, dst));
        if let Err(err) = r {
            tracing::warn!(
                "could not reproduce symlink {} -> {}: {err}",
                dst.display(),
                target.display()
            );
        }
    }
    Ok(())
}

/// Remove a deployed dir cleanly. Used when an entry is removed from config
/// or its `deploy_dir`/`mode` changed.
pub fn undeploy(dest: &Path) -> Result<()> {
    if !dest.exists() && !dest.is_symlink() {
        return Ok(());
    }
    fs_util::remove_dir_all_writable(dest).ok();
    let _ = std::fs::remove_dir(dest);
    let _ = std::fs::remove_file(dest);
    if dest.exists() || dest.is_symlink() {
        bail!("could not remove deploy dir {}", dest.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
