//! Content-addressed store. Walks a staging tree, hashes each regular file
//! with xxhash3-128, places exactly one copy in `cache/cas/<aa>/<bb...>`,
//! and hardlinks each install-tree path to that CAS file.
//!
//! Atomicity: each CAS insertion is a `rename(staging_file, cas_path)`. If
//! the target already exists (peer beat us, or a previous install already
//! owned this content), we byte-compare to confirm equality and discard our
//! staging copy. Hash collisions are detected — never silently corrupt.

use anyhow::{Context, Result, anyhow};
use std::fs::File;
use std::io::Read;
use std::path::Path;

use crate::cache::Cache;
use crate::fs_util;

/// Result of running the CAS pass over a staging tree.
#[derive(Debug, Clone, Default)]
pub struct CasReport {
    pub files_processed: usize,
    /// Files whose hash matched an existing CAS blob and were deduplicated.
    pub dedupe_hits: usize,
    pub bytes_freed: u64,
    pub symlinks: usize,
    pub directories: usize,
}

/// Run the CAS pass:
///   1. walk `staging_raw` (the dir the provider wrote to)
///   2. for each regular file: xxhash3-128, rename into CAS (byte-compare on
///      EEXIST), hardlink CAS→`staging_tree/<relpath>`
///   3. for each directory: mkdir under `staging_tree`
///   4. for each symlink: reproduce under `staging_tree`
///
/// After this returns, `staging_raw` will be empty of regular files (all
/// moved into CAS) and the engine should `remove_dir_all` it. The install
/// tree at `staging_tree/` is what gets atomically renamed into
/// `installs/<fingerprint>/tree/`.
pub fn run(cache: &Cache, staging_raw: &Path, staging_tree: &Path) -> Result<CasReport> {
    let mut report = CasReport::default();
    std::fs::create_dir_all(staging_tree)
        .with_context(|| format!("creating {}", staging_tree.display()))?;

    for entry in jwalk::WalkDir::new(staging_raw)
        .skip_hidden(false)
        .follow_links(false)
        .sort(true)
    {
        let entry = entry.with_context(|| format!("walking {}", staging_raw.display()))?;
        let src = entry.path();
        let rel = src
            .strip_prefix(staging_raw)
            .with_context(|| format!("stripping prefix {}", staging_raw.display()))?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        let dst = staging_tree.join(rel);

        let ft = entry.file_type();
        if ft.is_dir() {
            std::fs::create_dir_all(&dst)
                .with_context(|| format!("mkdir {}", dst.display()))?;
            report.directories += 1;
        } else if ft.is_symlink() {
            reproduce_symlink(&src, &dst)?;
            report.symlinks += 1;
        } else if ft.is_file() {
            cas_file(cache, &src, &dst, &mut report)?;
        }
        // Other file types (block/char/socket/fifo) are not expected in our
        // archives; ignore.
    }
    Ok(report)
}

/// Like `run` but skips CAS — moves staging files directly into the install
/// tree without hardlinking through the CAS dir. Used for `--no-cas`.
pub fn run_no_cas(staging_raw: &Path, staging_tree: &Path) -> Result<CasReport> {
    let mut report = CasReport::default();
    std::fs::create_dir_all(staging_tree)?;

    for entry in jwalk::WalkDir::new(staging_raw)
        .skip_hidden(false)
        .follow_links(false)
        .sort(true)
    {
        let entry = entry?;
        let src = entry.path();
        let rel = src.strip_prefix(staging_raw)?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        let dst = staging_tree.join(rel);
        let ft = entry.file_type();
        if ft.is_dir() {
            std::fs::create_dir_all(&dst)?;
            report.directories += 1;
        } else if ft.is_symlink() {
            reproduce_symlink(&src, &dst)?;
            report.symlinks += 1;
        } else if ft.is_file() {
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(&src, &dst).with_context(|| {
                format!("rename {} -> {}", src.display(), dst.display())
            })?;
            report.files_processed += 1;
        }
    }
    Ok(report)
}

fn cas_file(
    cache: &Cache,
    src: &Path,
    dst: &Path,
    report: &mut CasReport,
) -> Result<()> {
    let hex = hash_file_xxh3_128(src)?;
    let cas_path = cache.cas_path(&hex);

    // Make sure the parent dir exists before any rename attempt.
    if let Some(parent) = cas_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Try to claim the CAS slot.
    let cas_existed = cas_path.is_file();
    let mut just_inserted = false;
    if cas_existed {
        // Byte-compare to detect a (vanishingly rare) hash collision.
        if !files_equal(src, &cas_path)? {
            // Collision: keep src in place at dst, log loudly. Never
            // silently corrupt.
            tracing::error!(
                "xxh3-128 collision on {} (cas slot {}); keeping staging copy in install tree without CAS link",
                src.display(),
                cas_path.display()
            );
            move_into_install_tree(src, dst)?;
            report.files_processed += 1;
            return Ok(());
        }
        let size = std::fs::metadata(src).map(|m| m.len()).unwrap_or(0);
        report.dedupe_hits += 1;
        report.bytes_freed += size;
        // Drop our staging duplicate.
        std::fs::remove_file(src).with_context(|| {
            format!("removing duplicate staging file {}", src.display())
        })?;
    } else {
        // Try atomic rename; on EEXIST fall through to byte-compare branch
        // (peer raced us between our existence check and rename).
        match std::fs::rename(src, &cas_path) {
            Ok(()) => {
                just_inserted = true;
            }
            Err(err) => {
                if cas_path.is_file() {
                    if !files_equal(src, &cas_path)? {
                        tracing::error!(
                            "xxh3-128 collision (race) on {} (cas slot {}); keeping staging copy",
                            src.display(),
                            cas_path.display()
                        );
                        move_into_install_tree(src, dst)?;
                        report.files_processed += 1;
                        return Ok(());
                    }
                    let size = std::fs::metadata(src).map(|m| m.len()).unwrap_or(0);
                    report.dedupe_hits += 1;
                    report.bytes_freed += size;
                    let _ = std::fs::remove_file(src);
                } else {
                    return Err(anyhow!(
                        "rename {} -> {}: {err}",
                        src.display(),
                        cas_path.display()
                    ));
                }
            }
        }
    }

    // Hardlink CAS → install tree position. Do this BEFORE marking the CAS
    // file readonly: on Windows, marking a file's attributes momentarily
    // contends with concurrent CreateHardLinkW from a peer thread. We
    // mark readonly AFTER successfully creating our hardlink so future
    // peers linking to the same CAS blob can do so without contention
    // with our SetFileAttributes call. (And the hardlink helper itself
    // retries on transient access-denied for the same reason.)
    fs_util::hard_link(&cas_path, dst)?;

    if just_inserted {
        // Best-effort readonly on the CAS blob. The hardlink we just
        // created shares the inode, so this also makes our install-tree
        // entry readonly. Failure here is non-fatal.
        let _ = fs_util::clear_readonly(&cas_path); // first clear in case set
        let _ = mark_readonly(&cas_path);
    }

    report.files_processed += 1;
    Ok(())
}

fn mark_readonly(path: &Path) -> Result<()> {
    let md = std::fs::metadata(path)?;
    let mut perms = md.permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

fn move_into_install_tree(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(src, dst)
        .with_context(|| format!("rename {} -> {}", src.display(), dst.display()))?;
    Ok(())
}

fn reproduce_symlink(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let target = std::fs::read_link(src)
        .with_context(|| format!("read_link {}", src.display()))?;
    let _ = std::fs::remove_file(dst); // tolerate prior partial run
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&target, dst)
            .with_context(|| format!("symlink {} -> {}", dst.display(), target.display()))?;
    }
    #[cfg(windows)]
    {
        // We don't know if the link is to a file or dir; try file first.
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

/// Hash a file with xxhash3-128. Returns a 32-char lowercase hex string
/// formatted big-endian (high u64 first, low u64 second). Stable across
/// little-endian and big-endian hosts.
pub fn hash_file_xxh3_128(path: &Path) -> Result<String> {
    use xxhash_rust::xxh3::Xxh3;
    let mut hasher = Xxh3::new();
    let mut f = File::open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let h = hasher.digest128();
    let hi = (h >> 64) as u64;
    let lo = (h & 0xFFFF_FFFF_FFFF_FFFF) as u64;
    Ok(format!("{hi:016x}{lo:016x}"))
}

fn files_equal(a: &Path, b: &Path) -> Result<bool> {
    let am = std::fs::metadata(a)?;
    let bm = std::fs::metadata(b)?;
    if am.len() != bm.len() {
        return Ok(false);
    }
    let mut fa = File::open(a)?;
    let mut fb = File::open(b)?;
    let mut ba = vec![0u8; 64 * 1024];
    let mut bb = vec![0u8; 64 * 1024];
    loop {
        let na = fa.read(&mut ba)?;
        let nb = fb.read(&mut bb)?;
        if na != nb {
            return Ok(false);
        }
        if na == 0 {
            return Ok(true);
        }
        if ba[..na] != bb[..nb] {
            return Ok(false);
        }
    }
}

/// Clear the read-only attr on a CAS file briefly so callers can do things
/// like add a hardlink (some platforms refuse to mutate readonly inodes).
/// Only hardlinking into a readonly inode actually works on every platform
/// in practice — this helper is reserved for emergencies.
#[allow(dead_code)]
pub fn ensure_writable(path: &Path) -> Result<()> {
    fs_util::clear_readonly(path)
}
#[cfg(test)]
#[path = "cas_tests.rs"]
mod tests;
