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

pub use crate::extract_msi::extract_msi;

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
    let dec = xz2::read::XzDecoder::new(f);
    extract_tar_inner(dec, dest, strip_prefix)
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

fn extract_tar_inner<R: Read>(
    reader: R,
    dest: &Path,
    strip_prefix: Option<&str>,
) -> Result<()> {
    let mut archive = tar::Archive::new(reader);
    archive.set_overwrite(true);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let raw_path = entry.path()?.to_path_buf();
        // Defend against tar-slip: reject absolute paths and `..` components.
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
            // Reproduce as a symlink. The `tar` crate's unpack would do this;
            // we replicate manually so strip_prefix works.
            let link_target = entry
                .link_name()?
                .ok_or_else(|| anyhow!("symlink entry without link_name"))?
                .into_owned();
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // Best-effort: skip if symlink already exists.
            let _ = std::fs::remove_file(&out);
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(&link_target, &out)?;
            }
            #[cfg(windows)]
            {
                // Windows: try a file symlink; fall back silently if not allowed.
                if let Err(err) = std::os::windows::fs::symlink_file(&link_target, &out) {
                    tracing::warn!(
                        "could not create symlink {} -> {}: {err}",
                        out.display(),
                        link_target.display()
                    );
                }
            }
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
