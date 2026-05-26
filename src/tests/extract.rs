//! Tests for `src/extract.rs` (split out so the production
//! file shows only the implementation).

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
    let enc = liblzma::write::XzEncoder::new(f, 0);
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
