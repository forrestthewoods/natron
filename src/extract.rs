//! Archive extraction: zip, tar.xz, tar.gz, vsix (zip with `Contents/`
//! prefix), and msi.
//!
//! MSI extraction is pure-Rust (cross-platform) via the [`msi`] + [`cab`]
//! crates — implementation in [`crate::extract_msi`], re-exported here as
//! [`extract_msi`] for call-site symmetry with the other archive
//! extractors.
//!
//! Zip-related code uses `zip::ZipFile::enclosed_name()` to defend
//! against zip-slip (entries like `../../../etc/passwd`).

use anyhow::{Context, Result, anyhow, bail};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::config::ArchiveKind;

pub use crate::extract_msi::{extract_msi, extract_msis_in_parallel};

/// Extract the archive at `archive` into `dest`. If `strip_prefix` is set
/// and an entry's path starts with it, that prefix is removed before
/// placement (used for archives that nest everything in a top-level dir
/// like `clang+llvm-21.1.6-.../bin/clang.exe`).
pub fn extract_archive(
    archive: &Path,
    kind: ArchiveKind,
    dest: &Path,
    strip_prefix: Option<&str>,
) -> Result<()> {
    std::fs::create_dir_all(dest)
        .with_context(|| format!("creating dest {}", dest.display()))?;
    match kind {
        ArchiveKind::Zip => extract_zip(archive, dest, strip_prefix),
        ArchiveKind::TarXz => extract_tar_xz(archive, dest, strip_prefix),
        ArchiveKind::TarGz => extract_tar_gz(archive, dest, strip_prefix),
    }
}

fn extract_zip(archive: &Path, dest: &Path, strip_prefix: Option<&str>) -> Result<()> {
    tracing::debug!(
        "extracting zip {} -> {}",
        archive.display(),
        dest.display()
    );
    let f = File::open(archive)
        .with_context(|| format!("opening {}", archive.display()))?;
    let mut zip = zip::ZipArchive::new(f)
        .with_context(|| format!("reading zip {}", archive.display()))?;

    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        let raw = match entry.enclosed_name() {
            Some(p) => p.to_path_buf(),
            None => {
                bail!(
                    "zip entry has invalid path (zip-slip?): {}",
                    entry.name()
                );
            }
        };
        let rel = match apply_strip_prefix(&raw, strip_prefix) {
            Some(r) => r,
            None => continue, // entry lives outside strip_prefix; skip
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        let out = dest.join(&rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out)
                .with_context(|| format!("mkdir {}", out.display()))?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut outf = File::create(&out)
            .with_context(|| format!("creating {}", out.display()))?;
        std::io::copy(&mut entry, &mut outf)
            .with_context(|| format!("writing {}", out.display()))?;
    }
    Ok(())
}

fn extract_tar_xz(archive: &Path, dest: &Path, strip_prefix: Option<&str>) -> Result<()> {
    tracing::debug!(
        "extracting tar.xz {} -> {}",
        archive.display(),
        dest.display()
    );
    let f = File::open(archive)
        .with_context(|| format!("opening {}", archive.display()))?;
    let dec = mt_xz_decoder(f)?;
    extract_tar_inner(dec, dest, strip_prefix)
}

/// Build a multithreaded .xz decoder over `reader`. xz files written by `xz`
/// (e.g. LLVM's release tarballs) are split into independent ~64 MiB blocks,
/// so liblzma can decode them across threads. Falls back to a single-threaded
/// auto-decoder if the MT decoder can't be initialized.
fn mt_xz_decoder<R: std::io::Read>(reader: R) -> Result<liblzma::read::XzDecoder<R>> {
    let threads = crate::fs_util::worker_count(usize::MAX) as u32;
    if threads <= 1 {
        return Ok(liblzma::read::XzDecoder::new(reader));
    }
    // memlimit_stop = MAX disables the hard decode limit; memlimit_threading is
    // the soft budget below which liblzma keeps blocks single-threaded, so it
    // must comfortably hold several in-flight 64 MiB blocks per worker.
    let threading_budget = (threads as u64 + 1) * 256 * 1024 * 1024;
    match liblzma::stream::MtStreamBuilder::new()
        .threads(threads)
        .memlimit_stop(u64::MAX)
        .memlimit_threading(threading_budget)
        .timeout_ms(0)
        .decoder()
    {
        Ok(stream) => Ok(liblzma::read::XzDecoder::new_stream(reader, stream)),
        Err(err) => {
            tracing::warn!("MT xz decoder init failed ({err}); falling back to single-threaded");
            Ok(liblzma::read::XzDecoder::new(reader))
        }
    }
}

fn extract_tar_gz(archive: &Path, dest: &Path, strip_prefix: Option<&str>) -> Result<()> {
    tracing::debug!(
        "extracting tar.gz {} -> {}",
        archive.display(),
        dest.display()
    );
    // We don't currently depend on flate2 or libflate. Inline a trivial
    // gz reader via the `tar` crate's `GzDecoder`? `tar` only handles tar,
    // not gz. We need a gz decoder. The simplest path is to add `flate2`
    // as a dep. Defer: if a real gz consumer appears, add the dep then.
    //
    // For now: bail with a helpful error so a user who picks `archive =
    // "tar.gz"` gets a clear message rather than silent breakage.
    let _ = (archive, dest, strip_prefix);
    bail!(
        "tar.gz extraction is not yet implemented (add `flate2` dep when needed)"
    );
}

/// Files at or above this size are written inline on the reader thread
/// (streamed, never buffered whole); smaller files are handed to the writer
/// pool as an in-memory blob. This bounds the writer pool's memory to
/// roughly `channel_capacity * THRESHOLD` while still parallelizing the many
/// small-file creates — the per-file syscall + AV-scan cost that dominates on
/// Windows.
const TAR_INLINE_WRITE_THRESHOLD: u64 = 1 << 20; // 1 MiB

fn extract_tar_inner<R: Read>(
    reader: R,
    dest: &Path,
    strip_prefix: Option<&str>,
) -> Result<()> {
    let mut archive = tar::Archive::new(reader);
    archive.set_overwrite(true);

    let workers = crate::fs_util::worker_count(usize::MAX);
    // tar is a sequential stream, so a single reader thread parses entries and
    // either writes large files inline or dispatches small ones to the pool.
    let (tx, rx) = std::sync::mpsc::sync_channel::<(PathBuf, Vec<u8>)>(workers * 4);
    let rx = std::sync::Mutex::new(rx);
    let first_err: std::sync::Mutex<Option<anyhow::Error>> = std::sync::Mutex::new(None);

    std::thread::scope(|s| -> Result<()> {
        for _ in 0..workers {
            let rx = &rx;
            let first_err = &first_err;
            s.spawn(move || loop {
                // Hold the lock only for the dequeue; the write happens after
                // releasing it so workers write in parallel.
                let job = {
                    let guard = rx.lock().unwrap();
                    guard.recv()
                };
                let (path, bytes) = match job {
                    Ok(j) => j,
                    Err(_) => return, // channel closed: no more work
                };
                if let Err(e) = write_file_bytes(&path, &bytes) {
                    let mut g = first_err.lock().unwrap();
                    if g.is_none() {
                        *g = Some(e);
                    }
                }
            });
        }

        let mut produce = || -> Result<()> {
            for entry in archive.entries()? {
                let mut entry = entry?;
                let raw_path = entry.path()?.to_path_buf();
                // Defend against tar-slip: reject absolute paths and `..`.
                if raw_path.is_absolute()
                    || raw_path
                        .components()
                        .any(|c| matches!(c, std::path::Component::ParentDir))
                {
                    bail!("tar entry has unsafe path (tar-slip?): {}", raw_path.display());
                }
                let rel = match apply_strip_prefix(&raw_path, strip_prefix) {
                    Some(r) => r,
                    None => continue,
                };
                if rel.as_os_str().is_empty() {
                    continue;
                }
                let out = dest.join(&rel);
                let kind = entry.header().entry_type();
                if kind.is_dir() {
                    std::fs::create_dir_all(&out)?;
                    continue;
                }
                if kind.is_symlink() {
                    let link_target = entry
                        .link_name()?
                        .ok_or_else(|| anyhow!("symlink entry without link_name"))?
                        .into_owned();
                    reproduce_tar_symlink(&out, &link_target)?;
                    continue;
                }
                let size = entry.header().size().unwrap_or(0);
                if size >= TAR_INLINE_WRITE_THRESHOLD {
                    if let Some(parent) = out.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    let mut outf = File::create(&out)
                        .with_context(|| format!("creating {}", out.display()))?;
                    std::io::copy(&mut entry, &mut outf)
                        .with_context(|| format!("writing {}", out.display()))?;
                } else {
                    let mut buf = Vec::with_capacity(size as usize);
                    entry
                        .read_to_end(&mut buf)
                        .with_context(|| format!("reading {}", out.display()))?;
                    // A send error means all workers died; surface their error.
                    if tx.send((out, buf)).is_err() {
                        break;
                    }
                }
                // Bail early if a worker has already failed.
                if first_err.lock().unwrap().is_some() {
                    break;
                }
            }
            Ok(())
        };

        let result = produce();
        drop(tx); // close the channel so workers drain and exit
        result
    })?;

    if let Some(e) = first_err.into_inner().unwrap() {
        return Err(e);
    }
    Ok(())
}

/// Write `bytes` to `path`, creating parent directories. Used by the tar
/// writer pool. `create_dir_all` is race-tolerant across workers.
fn write_file_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, bytes)
        .with_context(|| format!("writing {}", path.display()))
}

fn reproduce_tar_symlink(out: &Path, link_target: &Path) -> Result<()> {
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Best-effort: skip if symlink already exists.
    let _ = std::fs::remove_file(out);
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(link_target, out)?;
    }
    #[cfg(windows)]
    {
        // Windows: try a file symlink; fall back silently if not allowed.
        if let Err(err) = std::os::windows::fs::symlink_file(link_target, out) {
            tracing::warn!(
                "could not create symlink {} -> {}: {err}",
                out.display(),
                link_target.display()
            );
        }
    }
    Ok(())
}

/// Strip the leading `prefix` (a path component, may be multi-component) from
/// `path`. Returns `None` if the path doesn't start with the prefix (such
/// entries are dropped during extraction).
fn apply_strip_prefix(path: &Path, prefix: Option<&str>) -> Option<PathBuf> {
    let Some(prefix) = prefix else {
        return Some(path.to_path_buf());
    };
    let prefix = Path::new(prefix);
    match path.strip_prefix(prefix) {
        Ok(rest) => Some(rest.to_path_buf()),
        Err(_) => None,
    }
}

/// VSIX is a zip whose payload lives under a `Contents/` prefix. Anything
/// outside `Contents/` is metadata we don't want.
#[allow(dead_code)] // Used by msvc provider in step 11
pub fn extract_vsix(archive: &Path, dest: &Path) -> Result<()> {
    tracing::debug!("extracting vsix {} -> {}", archive.display(), dest.display());
    std::fs::create_dir_all(dest)?;
    let f = File::open(archive)
        .with_context(|| format!("opening {}", archive.display()))?;
    let mut zip = zip::ZipArchive::new(f)?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        let name = entry.name().to_string();
        let Some(rel) = name.strip_prefix("Contents/") else {
            continue;
        };
        if rel.is_empty() {
            continue;
        }
        // Re-use enclosed_name on a synthetic path to defend against slip.
        let rel_path = Path::new(rel);
        if rel_path.is_absolute()
            || rel_path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            bail!("vsix entry has unsafe path: {name}");
        }
        let out = dest.join(rel_path);
        if entry.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut outf = File::create(&out)?;
        std::io::copy(&mut entry, &mut outf)?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "tests/extract.rs"]
mod tests;
