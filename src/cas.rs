//! Content-addressed store. Walks a staging tree, hashes each regular file
//! with xxhash3-128, places exactly one copy in `cache/cas/<aa>/<bb...>`,
//! and hardlinks each install-tree path to that CAS file.
//!
//! Atomicity: each CAS insertion publishes the staging file by
//! `hard_link(staging_file, cas_path)`, then drops the staging name. `link`
//! never overwrites: if the target already exists (peer beat us, or a previous
//! install already owned this content) it fails with `AlreadyExists` and we
//! byte-compare to confirm equality and discard our staging copy. Because a
//! published blob's dentry is never replaced, a peer concurrently hardlinking
//! that blob into its install tree can't observe a torn/missing slot. Hash
//! collisions are detected — never silently corrupt.

use anyhow::{Context, Result, anyhow};
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::Mutex;
use std::sync::mpsc;

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
///   2. for each regular file: xxhash3-128, publish into CAS via hardlink
///      (byte-compare on EEXIST), hardlink CAS→`staging_tree/<relpath>`
///   3. for each directory: mkdir under `staging_tree`
///   4. for each symlink: reproduce under `staging_tree`
///
/// After this returns, `staging_raw` will be empty of regular files (all
/// moved into CAS) and the engine should `remove_dir_all` it. The install
/// tree at `staging_tree/` is what gets atomically renamed into
/// `installs/<fingerprint>/tree/`.
pub fn run(cache: &Cache, staging_raw: &Path, staging_tree: &Path) -> Result<CasReport> {
    std::fs::create_dir_all(staging_tree)
        .with_context(|| format!("creating {}", staging_tree.display()))?;

    // Walk once, partitioning entries by kind. Directories and symlinks are
    // cheap and order-sensitive (parents before children), so we materialize
    // them serially; the per-file CAS work (hash + two hardlinks + a readonly
    // mark — the bulk of the cost, especially on Windows where each syscall is
    // expensive and an AV scanner sits in the path) is dispatched to a worker
    // pool below.
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    let mut symlinks: Vec<(std::path::PathBuf, std::path::PathBuf)> = Vec::new();
    let mut files: Vec<(std::path::PathBuf, std::path::PathBuf)> = Vec::new();
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
            dirs.push(dst);
        } else if ft.is_symlink() {
            symlinks.push((src, dst));
        } else if ft.is_file() {
            files.push((src, dst));
        }
        // Other file types (block/char/socket/fifo) are not expected in our
        // archives; ignore.
    }

    let mut report = CasReport::default();
    // `sort(true)` on the walk yields parents before children, so a plain
    // create_dir_all per entry suffices to reproduce the tree skeleton.
    for d in &dirs {
        std::fs::create_dir_all(d)
            .with_context(|| format!("mkdir {}", d.display()))?;
    }
    report.directories = dirs.len();
    for (src, dst) in &symlinks {
        reproduce_symlink(src, dst)?;
    }
    report.symlinks = symlinks.len();

    let partials = cas_files_parallel(cache, &files)?;
    for p in partials {
        report.files_processed += p.files_processed;
        report.dedupe_hits += p.dedupe_hits;
        report.bytes_freed += p.bytes_freed;
    }
    Ok(report)
}

/// Per-file CAS outcome accumulated by a worker.
#[derive(Default)]
struct FilePartial {
    files_processed: usize,
    dedupe_hits: usize,
    bytes_freed: u64,
}

/// Process `files` (src -> dst pairs) through the CAS across a worker pool.
/// Each CAS insertion is independently race-safe (see module docs: hardlink
/// publish + byte-compare on EEXIST), so concurrent workers — both within this
/// pass and against peer processes sharing the cache — can't corrupt a blob.
fn cas_files_parallel(
    cache: &Cache,
    files: &[(std::path::PathBuf, std::path::PathBuf)],
) -> Result<Vec<FilePartial>> {
    if files.is_empty() {
        return Ok(Vec::new());
    }
    let worker_count = crate::fs_util::worker_count(files.len());
    if worker_count <= 1 {
        let mut partial = FilePartial::default();
        for (src, dst) in files {
            cas_file(cache, src, dst, &mut partial)?;
        }
        return Ok(vec![partial]);
    }

    let queue: Mutex<Vec<usize>> = Mutex::new((0..files.len()).rev().collect());
    let (tx, rx) = mpsc::channel::<Result<FilePartial>>();
    std::thread::scope(|s| -> Result<Vec<FilePartial>> {
        for _ in 0..worker_count {
            let tx = tx.clone();
            let queue = &queue;
            s.spawn(move || {
                let mut partial = FilePartial::default();
                let result = loop {
                    let idx = match queue.lock().unwrap().pop() {
                        Some(i) => i,
                        None => break Ok(partial),
                    };
                    let (src, dst) = &files[idx];
                    if let Err(e) = cas_file(cache, src, dst, &mut partial) {
                        // Drain the queue so peers stop pulling new work.
                        queue.lock().unwrap().clear();
                        break Err(e);
                    }
                };
                let _ = tx.send(result);
            });
        }
        drop(tx);
        let mut partials = Vec::with_capacity(worker_count);
        let mut first_err: Option<anyhow::Error> = None;
        for msg in rx {
            match msg {
                Ok(p) => partials.push(p),
                Err(e) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(partials),
        }
    })
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
    report: &mut FilePartial,
) -> Result<()> {
    let hex = hash_file_xxh3_128(src)?;
    let cas_path = cache.cas_path(&hex);

    // Make sure the parent dir exists before any link attempt.
    if let Some(parent) = cas_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Publish our staging copy into the CAS by hardlinking it onto the cas
    // slot. `hard_link` is non-destructive: if the slot is already populated
    // (a peer raced us, or a prior install owns this content) it fails with
    // `AlreadyExists` and we leave the existing blob untouched. A `rename`
    // here would *overwrite* on POSIX, churning the blob's dentry — and a
    // peer mid-`hard_link(cas -> install tree)` against that same slot can
    // observe a transient ENOENT during the swap. Once a cas blob exists its
    // dentry is now never replaced, which keeps concurrent links race-free.
    let mut just_inserted = false;
    match std::fs::hard_link(src, &cas_path) {
        Ok(()) => {
            just_inserted = true;
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            // Slot already populated. Byte-compare to detect a (vanishingly
            // rare) xxh3-128 collision; never silently corrupt.
            if !files_equal(src, &cas_path)? {
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
        }
        Err(err) => {
            return Err(anyhow!(
                "publishing {} -> {}: {err}",
                src.display(),
                cas_path.display()
            ));
        }
    }

    // The CAS owns the content now; drop our staging copy. On the
    // just-inserted path `src` and `cas_path` are two names for one inode, so
    // this just removes the redundant name; on the dedupe path `src` was a
    // distinct copy. Best-effort: the content is already safely in the CAS,
    // and the engine reaps the whole staging dir afterward, so a transient
    // unlink failure (e.g. an AV scanner briefly holding `src` on Windows)
    // must not abort an otherwise-successful install.
    let _ = std::fs::remove_file(src);

    // Hardlink CAS → install tree position. Do this BEFORE marking the CAS
    // file readonly: on Windows, marking a file's attributes momentarily
    // contends with concurrent CreateHardLinkW from a peer thread. We
    // mark readonly AFTER successfully creating our hardlink so future
    // peers linking to the same CAS blob can do so without contention
    // with our SetFileAttributes call. (And the hardlink helper itself
    // retries on transient access-denied for the same reason.)
    fs_util::hard_link(&cas_path, dst)?;

    if just_inserted {
        // Best-effort readonly on the CAS blob. The hardlink we just created
        // shares the inode, so this also makes our install-tree entry
        // readonly. A blob we just published is a fresh hardlink off a
        // writable, freshly-extracted file, so it's never already readonly —
        // no need to clear first. Failure here is non-fatal.
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
#[path = "tests/cas.rs"]
mod tests;
