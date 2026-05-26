//! Deploy a cache install tree into a project's `<settings.deploy_dir>/<name>/`
//! using one of three modes: `hardlink`, `symlink`, `copy`.

use anyhow::{Context, Result, bail};
use std::path::Path;

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
        DeployMode::Copy => deploy_copy(cache_tree, dest),
    }
}

fn deploy_symlink(cache_tree: &Path, dest: &Path) -> Result<()> {
    fs_util::dir_symlink(cache_tree, dest)
}

fn deploy_copy(cache_tree: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    walk_and_apply(cache_tree, dest, &|src, dst, ft| {
        if ft.is_dir() {
            std::fs::create_dir_all(dst)?;
        } else if ft.is_symlink() {
            fs_util::reproduce_symlink(src, dst)?;
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
#[path = "tests/deploy.rs"]
mod tests;
