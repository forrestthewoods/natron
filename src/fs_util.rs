//! Low-level filesystem helpers: read-only marking, atomic rename, directory
//! symlinks/junctions on Windows, etc.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Recursively mark every regular file under `path` as read-only.
///
/// Uses `jwalk` for parallel walking. Errors on individual files are logged
/// at warn level rather than failing the whole operation — readonly marking
/// is best-effort.
pub fn set_readonly_recursive(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    for entry in jwalk::WalkDir::new(path)
        .skip_hidden(false)
        .follow_links(false)
    {
        match entry {
            Ok(e) if e.file_type().is_file() => {
                let p = e.path();
                if let Err(err) = mark_file_readonly(&p) {
                    tracing::warn!("failed to mark readonly: {}: {err}", p.display());
                }
            }
            Ok(_) => {}
            Err(err) => tracing::warn!("walk error in {}: {err}", path.display()),
        }
    }
    Ok(())
}

fn mark_file_readonly(path: &Path) -> Result<()> {
    let md = std::fs::metadata(path)?;
    let mut perms = md.permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

/// Clear the read-only attribute on a single file. Used when we need to
/// remove a file that was previously marked read-only (Windows refuses to
/// delete read-only files).
pub fn clear_readonly(path: &Path) -> Result<()> {
    let md = std::fs::symlink_metadata(path)?;
    let mut perms = md.permissions();
    if perms.readonly() {
        perms.set_readonly(false);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

/// Recursively delete a directory tree, clearing read-only attrs as needed.
/// On Windows, plain `remove_dir_all` fails on read-only files; we walk and
/// clear first.
pub fn remove_dir_all_writable(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    // First pass: clear readonly on every file.
    for entry in jwalk::WalkDir::new(path)
        .skip_hidden(false)
        .follow_links(false)
    {
        if let Ok(e) = entry {
            if e.file_type().is_file() {
                let _ = clear_readonly(&e.path());
            }
        }
    }
    std::fs::remove_dir_all(path)
        .with_context(|| format!("removing {}", path.display()))?;
    Ok(())
}

/// Atomically write `bytes` to `path` via temp-then-rename.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("no parent dir for {}", path.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating {}", parent.display()))?;
    let tmp = tempfile::Builder::new()
        .prefix(".natron-tmp-")
        .tempfile_in(parent)
        .with_context(|| format!("creating tempfile in {}", parent.display()))?;
    std::fs::write(tmp.path(), bytes)
        .with_context(|| format!("writing tempfile {}", tmp.path().display()))?;
    let tmp_path = tmp.into_temp_path();
    // Use persist to atomic-rename. On Windows, rename across volumes fails;
    // we only ever write within `parent`, so this is fine.
    tmp_path
        .persist(path)
        .map_err(|e| anyhow::anyhow!("persisting to {}: {e}", path.display()))?;
    Ok(())
}

/// Try to atomically rename `from` to `to`. Returns `Ok(true)` on success,
/// `Ok(false)` if the target already exists (peer beat us / collision case),
/// and `Err` on other failures.
///
/// POSIX `rename` of a directory onto a non-empty existing directory fails
/// with `ENOTEMPTY`. Windows `rename` always fails when target exists. Both
/// yield the desired "loser drops" outcome.
pub fn try_rename(from: &Path, to: &Path) -> Result<bool> {
    match std::fs::rename(from, to) {
        Ok(()) => Ok(true),
        Err(err) => {
            // If the target now exists (peer beat us, or it was always
            // there), treat as collision rather than error.
            if to.exists() {
                Ok(false)
            } else {
                Err(anyhow::anyhow!(
                    "rename {} -> {}: {err}",
                    from.display(),
                    to.display()
                ))
            }
        }
    }
}

/// Create a hardlink. Wraps `std::fs::hard_link` with better error context.
pub fn hard_link(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::hard_link(src, dst).with_context(|| {
        format!("hardlink {} -> {}", src.display(), dst.display())
    })?;
    Ok(())
}

/// Create a directory symlink. On Windows, falls back to a junction if
/// symlink creation lacks privilege.
pub fn dir_symlink(target: &Path, link: &Path) -> Result<()> {
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::symlink_dir;
        match symlink_dir(target, link) {
            Ok(()) => return Ok(()),
            Err(err) => {
                // ERROR_PRIVILEGE_NOT_HELD = 1314
                let raw = err.raw_os_error();
                if raw == Some(1314) {
                    return create_junction(target, link);
                }
                return Err(anyhow::anyhow!(
                    "symlink {} -> {}: {err}",
                    link.display(),
                    target.display()
                ));
            }
        }
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).with_context(|| {
            format!("symlink {} -> {}", link.display(), target.display())
        })?;
        Ok(())
    }
}

/// Create a Windows directory junction. Junctions don't require any
/// privilege but are local-volume-only.
#[cfg(windows)]
fn create_junction(target: &Path, link: &Path) -> Result<()> {
    // Use mklink /J via cmd.exe. The Win32 API for junctions is fiddly
    // (DeviceIoControl with REPARSE_DATA_BUFFER); shelling out is the
    // pragmatic choice and matches what most Rust toolchain managers do.
    let target_abs = std::fs::canonicalize(target).with_context(|| {
        format!("canonicalizing junction target {}", target.display())
    })?;
    // Strip the \\?\ prefix that canonicalize adds; mklink rejects it.
    let target_str = target_abs.to_string_lossy();
    let target_clean = target_str
        .strip_prefix(r"\\?\")
        .unwrap_or(&target_str);

    let status = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target_clean)
        .status()
        .with_context(|| "spawning cmd for mklink /J")?;
    if !status.success() {
        anyhow::bail!(
            "mklink /J {} {} failed with {}",
            link.display(),
            target_clean,
            status
        );
    }
    Ok(())
}

#[cfg(not(windows))]
#[allow(dead_code)]
fn create_junction(_target: &Path, _link: &Path) -> Result<()> {
    anyhow::bail!("junctions are Windows-only");
}

/// Return true if `link` is a symlink (or junction on Windows) that resolves
/// to `expected_target`. Returns false on any kind of mismatch or error.
pub fn symlink_points_to(link: &Path, expected_target: &Path) -> bool {
    let Ok(actual) = std::fs::read_link(link) else {
        return false;
    };
    let canon_actual = std::fs::canonicalize(&actual).unwrap_or(actual);
    let canon_expected = std::fs::canonicalize(expected_target)
        .unwrap_or_else(|_| expected_target.to_path_buf());
    canon_actual == canon_expected
}

/// Return the device/volume identifier for a path. Used for same-volume
/// detection (hardlinks require single-filesystem).
#[cfg(unix)]
pub fn volume_id(path: &Path) -> Result<u64> {
    use std::os::unix::fs::MetadataExt;
    let md = std::fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?;
    Ok(md.dev())
}

#[cfg(windows)]
pub fn volume_id(path: &Path) -> Result<u64> {
    // GetVolumePathNameW gives us the volume root (e.g. "C:\"). We hash the
    // resulting string to get a comparable id. This is sufficient: two paths
    // share a volume iff their volume roots match.
    use std::os::windows::ffi::OsStrExt;
    use std::path::PathBuf;
    use windows_sys::Win32::Storage::FileSystem::GetVolumePathNameW;

    let abs: PathBuf = std::fs::canonicalize(path)
        .with_context(|| format!("canonicalizing {}", path.display()))?;
    let wide: Vec<u16> = abs.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut buf = vec![0u16; 260];
    let ok = unsafe {
        GetVolumePathNameW(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32)
    };
    if ok == 0 {
        anyhow::bail!("GetVolumePathNameW failed for {}", path.display());
    }
    // Find null terminator.
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let s = String::from_utf16_lossy(&buf[..len]).to_lowercase();
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    Ok(h.finish())
}

/// Return true if both paths live on the same filesystem volume.
pub fn same_volume(a: &Path, b: &Path) -> Result<bool> {
    Ok(volume_id(a)? == volume_id(b)?)
}

/// Walk a directory and return the most-recent mtime found across any file
/// (recursively). Used for stale-staging GC: we want to know whether anyone
/// has touched anything inside the dir recently, not just the dir's own mtime.
pub fn latest_inside_mtime(dir: &Path) -> Result<std::time::SystemTime> {
    let mut latest = std::fs::metadata(dir)
        .with_context(|| format!("stat {}", dir.display()))?
        .modified()?;
    for entry in jwalk::WalkDir::new(dir)
        .skip_hidden(false)
        .follow_links(false)
    {
        let Ok(e) = entry else { continue };
        let Ok(md) = std::fs::symlink_metadata(e.path()) else { continue };
        let Ok(m) = md.modified() else { continue };
        if m > latest {
            latest = m;
        }
    }
    Ok(latest)
}

/// Return forward-slash version of a path (best-effort UTF-8). Used when
/// serializing paths into TOML / JSON state files.
pub fn slash_str(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Like `slash_str` but takes a `String` slot directly.
pub fn slash_path_buf(path: PathBuf) -> String {
    slash_str(&path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn atomic_write_creates_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("sub").join("file.txt");
        atomic_write(&path, b"hello").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
    }

    #[test]
    fn atomic_write_overwrites() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("file.txt");
        atomic_write(&path, b"first").unwrap();
        atomic_write(&path, b"second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
    }

    #[test]
    fn try_rename_succeeds_when_target_missing() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::write(&a, "x").unwrap();
        let renamed = try_rename(&a, &b).unwrap();
        assert!(renamed);
        assert!(!a.exists());
        assert!(b.exists());
    }

    #[test]
    fn try_rename_returns_false_on_collision() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(b.join("existing"), "x").unwrap();
        let renamed = try_rename(&a, &b).unwrap();
        assert!(!renamed);
    }

    #[test]
    fn set_readonly_recursive_marks_files() {
        let tmp = TempDir::new().unwrap();
        let f = tmp.path().join("a.txt");
        std::fs::write(&f, "x").unwrap();
        set_readonly_recursive(tmp.path()).unwrap();
        let md = std::fs::metadata(&f).unwrap();
        assert!(md.permissions().readonly());
    }

    #[test]
    fn remove_dir_all_writable_handles_readonly() {
        let tmp = TempDir::new().unwrap();
        let inner = tmp.path().join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(inner.join("a.txt"), "x").unwrap();
        set_readonly_recursive(&inner).unwrap();
        remove_dir_all_writable(&inner).unwrap();
        assert!(!inner.exists());
    }

    #[test]
    fn hard_link_creates_link() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::write(&a, "data").unwrap();
        hard_link(&a, &b).unwrap();
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "data");
    }

    #[test]
    fn same_volume_within_tempdir() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        assert!(same_volume(&a, &b).unwrap());
    }

    #[test]
    fn slash_str_normalizes() {
        let p = PathBuf::from(r"C:\foo\bar");
        let s = slash_str(&p);
        assert!(s.contains('/') || !s.contains('\\'));
    }

    #[test]
    fn dir_symlink_works() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("target");
        let link = tmp.path().join("link");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("inside.txt"), "hi").unwrap();
        dir_symlink(&target, &link).unwrap();
        // Confirm we can read through the link.
        let read = std::fs::read_to_string(link.join("inside.txt"));
        assert!(read.is_ok(), "could not read through link: {:?}", read);
    }
}
