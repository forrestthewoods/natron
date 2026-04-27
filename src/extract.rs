//! Archive extraction: zip, tar.xz, tar.gz, vsix (zip with `Contents/` prefix),
//! and msi (Windows-only via `msiexec /a`).
//!
//! Uses `zip::ZipFile::enclosed_name()` to defend against zip-slip
//! (entries like `../../../etc/passwd`).

use anyhow::{Context, Result, anyhow, bail};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::config::ArchiveKind;

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

/// Run `msiexec /a` to do an "administrative" extraction of an MSI into a
/// destination directory. Windows-only.
///
/// msiexec is picky about path formatting: it wants all-backslash native
/// paths, not mixed `/` and `\`. We normalize before invocation. We also
/// avoid the `\\?\` prefix that `canonicalize` adds — msiexec rejects that
/// too.
#[cfg(windows)]
#[allow(dead_code)] // Used by msvc + windows_sdk providers in steps 11/12
pub fn extract_msi(msi: &Path, dest: &Path) -> Result<()> {
    tracing::debug!("extracting msi {} -> {}", msi.display(), dest.display());
    std::fs::create_dir_all(dest)?;
    let abs_msi = if msi.is_absolute() {
        msi.to_path_buf()
    } else {
        std::env::current_dir()?.join(msi)
    };
    let abs_dest = if dest.is_absolute() {
        dest.to_path_buf()
    } else {
        std::env::current_dir()?.join(dest)
    };
    let msi_native = to_native_windows_path(&abs_msi);
    let dest_native = to_native_windows_path(&abs_dest);
    // TARGETDIR must end with a backslash for msiexec.
    let target = format!("{dest_native}\\");
    let log_path = abs_dest.join("msi_install.log");
    let log_native = to_native_windows_path(&log_path);
    let output = std::process::Command::new("msiexec.exe")
        .arg("/a")
        .arg(&msi_native)
        .arg("/qn")
        .arg(format!("TARGETDIR={target}"))
        .arg("/L*V")
        .arg(&log_native)
        .output()
        .with_context(|| "running msiexec.exe")?;
    if !output.status.success() {
        let log = std::fs::read_to_string(&log_path)
            .unwrap_or_else(|_| "<could not read msi log>".to_string());
        let tail: Vec<_> = log.lines().rev().take(40).collect();
        bail!(
            "msiexec /a failed (msi={msi_native}): status={} stderr={}\n--- log tail ---\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr),
            tail.into_iter().rev().collect::<Vec<_>>().join("\n")
        );
    }
    // Clean up the log file we asked msiexec to write.
    let _ = std::fs::remove_file(&log_path);
    Ok(())
}

/// Convert a Path to a native Windows path string with all backslashes and
/// no `\\?\` prefix (which msiexec rejects).
#[cfg(windows)]
fn to_native_windows_path(p: &Path) -> String {
    let s = p.to_string_lossy().replace('/', "\\");
    s.strip_prefix(r"\\?\").map(|s| s.to_string()).unwrap_or(s)
}

#[cfg(not(windows))]
#[allow(dead_code)]
pub fn extract_msi(_msi: &Path, _dest: &Path) -> Result<()> {
    bail!("MSI extraction requires Windows (msiexec.exe)");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use tempfile::TempDir;

    fn build_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let f = File::create(path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opts = zip::write::FileOptions::default();
        for (name, data) in entries {
            zw.start_file(*name, opts).unwrap();
            zw.write_all(data).unwrap();
        }
        zw.finish().unwrap();
    }

    fn build_tar_xz(path: &Path, entries: &[(&str, &[u8])]) {
        let f = File::create(path).unwrap();
        let enc = xz2::write::XzEncoder::new(f, 0);
        let mut tar = tar::Builder::new(enc);
        for (name, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, name, *data).unwrap();
        }
        tar.finish().unwrap();
        // Drop the builder to flush the encoder.
    }

    #[test]
    fn extracts_zip_no_strip() {
        let tmp = TempDir::new().unwrap();
        let archive = tmp.path().join("a.zip");
        build_zip(
            &archive,
            &[
                ("a.txt", b"AAA"),
                ("d/b.txt", b"BBB"),
            ],
        );
        let dest = tmp.path().join("out");
        extract_archive(&archive, ArchiveKind::Zip, &dest, None).unwrap();
        assert_eq!(std::fs::read(dest.join("a.txt")).unwrap(), b"AAA");
        assert_eq!(std::fs::read(dest.join("d").join("b.txt")).unwrap(), b"BBB");
    }

    #[test]
    fn extracts_zip_with_strip_prefix() {
        let tmp = TempDir::new().unwrap();
        let archive = tmp.path().join("a.zip");
        build_zip(
            &archive,
            &[
                ("clang+llvm-21/bin/clang.exe", b"BIN"),
                ("clang+llvm-21/LICENSE", b"LIC"),
                ("README", b"discard-me"),
            ],
        );
        let dest = tmp.path().join("out");
        extract_archive(&archive, ArchiveKind::Zip, &dest, Some("clang+llvm-21")).unwrap();
        assert_eq!(std::fs::read(dest.join("bin").join("clang.exe")).unwrap(), b"BIN");
        assert_eq!(std::fs::read(dest.join("LICENSE")).unwrap(), b"LIC");
        // Out-of-prefix entries are dropped silently.
        assert!(!dest.join("README").exists());
    }

    #[test]
    fn extracts_tar_xz() {
        let tmp = TempDir::new().unwrap();
        let archive = tmp.path().join("a.tar.xz");
        build_tar_xz(
            &archive,
            &[
                ("clang+llvm/bin/clang", b"!!"),
                ("clang+llvm/LICENSE", b"L"),
            ],
        );
        let dest = tmp.path().join("out");
        extract_archive(&archive, ArchiveKind::TarXz, &dest, Some("clang+llvm")).unwrap();
        assert_eq!(std::fs::read(dest.join("bin").join("clang")).unwrap(), b"!!");
        assert_eq!(std::fs::read(dest.join("LICENSE")).unwrap(), b"L");
    }

    #[test]
    fn rejects_zip_slip_in_zip() {
        let tmp = TempDir::new().unwrap();
        let archive = tmp.path().join("a.zip");
        let f = File::create(&archive).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opts = zip::write::FileOptions::default();
        // The `zip` crate normalizes names on writing; simulate slip by
        // using a name with backslash on a non-Windows system. On Windows
        // it'll likely be rejected by enclosed_name() too.
        zw.start_file("../escape.txt", opts).unwrap();
        zw.write_all(b"x").unwrap();
        zw.finish().unwrap();

        let dest = tmp.path().join("out");
        let res = extract_archive(&archive, ArchiveKind::Zip, &dest, None);
        // Either: extraction errored (zip-slip detected), or the entry's
        // sanitized form landed inside dest. Either is safe.
        if let Ok(()) = res {
            // Verify nothing escaped above dest.
            let parent = dest.parent().unwrap();
            assert!(!parent.join("escape.txt").exists(), "zip-slip succeeded!");
        }
    }

    // Note: testing tar-slip end-to-end would require manually constructing
    // raw tar bytes — the `tar` crate's Builder refuses to write `..` paths.
    // The defense in `extract_tar_inner` (rejecting absolute paths and
    // `ParentDir` components) is still load-bearing for hand-crafted tars
    // produced by external tools.

    #[test]
    fn apply_strip_prefix_basic() {
        // Match
        assert_eq!(
            apply_strip_prefix(Path::new("foo/bar/baz"), Some("foo")),
            Some(PathBuf::from("bar/baz"))
        );
        // No prefix (None) returns input unchanged
        assert_eq!(
            apply_strip_prefix(Path::new("foo/bar"), None),
            Some(PathBuf::from("foo/bar"))
        );
        // Mismatch -> None (entry dropped)
        assert_eq!(
            apply_strip_prefix(Path::new("other/file"), Some("foo")),
            None
        );
        // Multi-segment prefix
        assert_eq!(
            apply_strip_prefix(Path::new("a/b/c/d.txt"), Some("a/b")),
            Some(PathBuf::from("c/d.txt"))
        );
    }

    #[test]
    fn vsix_strips_contents_prefix() {
        let tmp = TempDir::new().unwrap();
        let archive = tmp.path().join("a.vsix");
        build_zip(
            &archive,
            &[
                ("Contents/bin/cl.exe", b"CL"),
                ("[Content_Types].xml", b"meta"),
                ("Contents/lib/foo.lib", b"FOO"),
            ],
        );
        let dest = tmp.path().join("out");
        extract_vsix(&archive, &dest).unwrap();
        assert_eq!(std::fs::read(dest.join("bin").join("cl.exe")).unwrap(), b"CL");
        assert_eq!(std::fs::read(dest.join("lib").join("foo.lib")).unwrap(), b"FOO");
        assert!(!dest.join("[Content_Types].xml").exists());
    }

    #[test]
    fn tar_gz_currently_errors() {
        let tmp = TempDir::new().unwrap();
        let dummy = tmp.path().join("a.tar.gz");
        std::fs::write(&dummy, b"").unwrap();
        let dest = tmp.path().join("out");
        let err = extract_archive(&dummy, ArchiveKind::TarGz, &dest, None).unwrap_err();
        assert!(err.to_string().contains("tar.gz"));
    }
}
