//! Hermetic tests for the pure-Rust MSI extractor.
//!
//! Builds synthetic MSIs in-process via `msi::Package::create` +
//! `cab::CabinetBuilder` so the tests run on any platform — no
//! msiexec required. Validates that the extractor produces the
//! correct on-disk layout for the MSI features SDK installers
//! actually use.

use super::extract_msi_pure;

use anyhow::Result;
use cab::{CabinetBuilder, CompressionType};
use msi::{Column, Insert, PackageType, Value};
use std::fs::File;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

// ============================================================================
// Helpers
// ============================================================================

/// Build a CAB containing the given files (key, bytes) using MSZIP
/// compression. The CAB entry name is the same as the key — for MSI
/// CABs that's the `File` table's primary key.
fn build_cab(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut builder = CabinetBuilder::new();
    {
        let folder = builder.add_folder(CompressionType::MsZip);
        for (name, _) in files {
            folder.add_file((*name).to_string());
        }
    }
    let mut writer = builder.build(Cursor::new(Vec::<u8>::new())).unwrap();
    while let Some(mut fw) = writer.next_file().unwrap() {
        let name = fw.file_name().to_string();
        let (_, data) = files
            .iter()
            .find(|(n, _)| *n == name)
            .expect("file referenced in builder must be in payload list");
        fw.write_all(data).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

/// Build a minimal valid MSI at `path` with the given table contents.
///
/// `directories`, `components`, `media`, `files` populate the
/// respective MSI tables. `embedded_streams` is a list of
/// (stream_name, cab_bytes) — the stream_name does NOT include the
/// leading `#`. For tests that need a *missing* stream, just omit it.
fn build_msi(
    path: &Path,
    directories: &[(&str, Option<&str>, &str)],
    components: &[(&str, &str)],
    media: &[(i32, &str)],
    files: &[(&str, &str, &str, i32)],
    embedded_streams: &[(&str, &[u8])],
) -> Result<()> {
    // Package::create needs Read + Write + Seek; File::create gives
    // write-only on Windows, which the underlying CFB writer rejects.
    let f = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    let mut pkg = msi::Package::create(PackageType::Installer, f)?;

    pkg.create_table(
        "Directory",
        vec![
            Column::build("Directory").primary_key().id_string(72),
            Column::build("Directory_Parent").nullable().id_string(72),
            Column::build("DefaultDir").text_string(255),
        ],
    )?;
    for (key, parent, default_dir) in directories {
        pkg.insert_rows(Insert::into("Directory").row(vec![
            Value::Str((*key).to_string()),
            match parent {
                Some(p) => Value::Str((*p).to_string()),
                None => Value::Null,
            },
            Value::Str((*default_dir).to_string()),
        ]))?;
    }

    pkg.create_table(
        "Component",
        vec![
            Column::build("Component").primary_key().id_string(72),
            Column::build("Directory_").id_string(72),
        ],
    )?;
    for (comp_key, dir_key) in components {
        pkg.insert_rows(Insert::into("Component").row(vec![
            Value::Str((*comp_key).to_string()),
            Value::Str((*dir_key).to_string()),
        ]))?;
    }

    pkg.create_table(
        "Media",
        vec![
            Column::build("DiskId").primary_key().int16(),
            Column::build("LastSequence").int16(),
            Column::build("Cabinet").nullable().text_string(255),
        ],
    )?;
    for (idx, (last_seq, cabinet)) in media.iter().enumerate() {
        pkg.insert_rows(Insert::into("Media").row(vec![
            Value::Int((idx + 1) as i32),
            Value::Int(*last_seq),
            Value::Str((*cabinet).to_string()),
        ]))?;
    }

    pkg.create_table(
        "File",
        vec![
            Column::build("File").primary_key().id_string(72),
            Column::build("Component_").id_string(72),
            Column::build("FileName").text_string(255),
            Column::build("Sequence").int16(),
            Column::build("Attributes").nullable().int16(),
        ],
    )?;
    for (file_key, comp_key, filename, sequence) in files {
        pkg.insert_rows(Insert::into("File").row(vec![
            Value::Str((*file_key).to_string()),
            Value::Str((*comp_key).to_string()),
            Value::Str((*filename).to_string()),
            Value::Int(*sequence),
            Value::Null,
        ]))?;
    }

    for (stream_name, bytes) in embedded_streams {
        let mut w = pkg.write_stream(stream_name)?;
        w.write_all(bytes)?;
    }

    pkg.flush()?;
    Ok(())
}

/// Read a file as a UTF-8 string. Convenience for asserting payload
/// contents in tests where payloads are ASCII.
fn read_text(path: &Path) -> String {
    String::from_utf8(std::fs::read(path).unwrap()).unwrap()
}

fn build_paths(tmp: &TempDir) -> (PathBuf, PathBuf) {
    let msi = tmp.path().join("test.msi");
    let dest = tmp.path().join("out");
    (msi, dest)
}

// ============================================================================
// Tests
// ============================================================================

#[test]
fn extracts_single_file_into_targetdir() {
    let tmp = TempDir::new().unwrap();
    let (msi, dest) = build_paths(&tmp);

    build_msi(
        &msi,
        &[("TARGETDIR", None, "SourceDir")],
        &[("c1", "TARGETDIR")],
        &[(1, "#Cab1")],
        &[("f1", "c1", "hello.txt", 1)],
        &[("Cab1", &build_cab(&[("f1", b"hello world")]))],
    )
    .unwrap();

    extract_msi_pure(&msi, &dest).unwrap();
    assert_eq!(read_text(&dest.join("hello.txt")), "hello world");
}

#[test]
fn resolves_short_pipe_long_filename() {
    let tmp = TempDir::new().unwrap();
    let (msi, dest) = build_paths(&tmp);

    build_msi(
        &msi,
        &[("TARGETDIR", None, ".")],
        &[("c1", "TARGETDIR")],
        &[(1, "#Cab1")],
        &[("f1", "c1", "FOO~1.TXT|LongName.txt", 1)],
        &[("Cab1", &build_cab(&[("f1", b"X")]))],
    )
    .unwrap();

    extract_msi_pure(&msi, &dest).unwrap();
    assert!(
        dest.join("LongName.txt").is_file(),
        "expected long-name file; dest contents: {:?}",
        std::fs::read_dir(&dest).unwrap().collect::<Vec<_>>()
    );
    assert!(
        !dest.join("FOO~1.TXT").exists(),
        "short-name file should not have been created"
    );
}

#[test]
fn resolves_nested_directories() {
    let tmp = TempDir::new().unwrap();
    let (msi, dest) = build_paths(&tmp);

    build_msi(
        &msi,
        &[
            ("TARGETDIR", None, "."),
            ("a", Some("TARGETDIR"), "A1|alpha"),
            ("b", Some("a"), "B2|beta"),
            ("c", Some("b"), "C3|gamma"),
        ],
        &[("c1", "c")],
        &[(1, "#Cab1")],
        &[("leaf", "c1", "leaf.txt", 1)],
        &[("Cab1", &build_cab(&[("leaf", b"deep")]))],
    )
    .unwrap();

    extract_msi_pure(&msi, &dest).unwrap();
    let expected = dest.join("alpha").join("beta").join("gamma").join("leaf.txt");
    assert!(expected.is_file(), "expected {} to exist", expected.display());
    assert_eq!(read_text(&expected), "deep");
}

#[test]
fn splits_files_across_two_embedded_cabs() {
    let tmp = TempDir::new().unwrap();
    let (msi, dest) = build_paths(&tmp);

    // Media table: sequences 1 → Cab1, sequences 2..=3 → Cab2.
    build_msi(
        &msi,
        &[("TARGETDIR", None, ".")],
        &[("c1", "TARGETDIR")],
        &[(1, "#Cab1"), (3, "#Cab2")],
        &[
            ("a", "c1", "a.txt", 1),
            ("b", "c1", "b.txt", 2),
            ("c", "c1", "c.txt", 3),
        ],
        &[
            ("Cab1", &build_cab(&[("a", b"AAA")])),
            ("Cab2", &build_cab(&[("b", b"BBB"), ("c", b"CCC")])),
        ],
    )
    .unwrap();

    extract_msi_pure(&msi, &dest).unwrap();
    assert_eq!(read_text(&dest.join("a.txt")), "AAA");
    assert_eq!(read_text(&dest.join("b.txt")), "BBB");
    assert_eq!(read_text(&dest.join("c.txt")), "CCC");
}

#[test]
fn external_cab_resolved_as_sibling() {
    let tmp = TempDir::new().unwrap();
    let (msi, dest) = build_paths(&tmp);

    build_msi(
        &msi,
        &[("TARGETDIR", None, ".")],
        &[("c1", "TARGETDIR")],
        &[(1, "external.cab")],
        &[("f1", "c1", "ext.txt", 1)],
        &[], // no embedded streams; CAB is on disk
    )
    .unwrap();

    std::fs::write(
        msi.parent().unwrap().join("external.cab"),
        build_cab(&[("f1", b"sibling")]),
    )
    .unwrap();

    extract_msi_pure(&msi, &dest).unwrap();
    assert_eq!(read_text(&dest.join("ext.txt")), "sibling");
}

#[test]
fn bails_on_missing_cab_stream() {
    let tmp = TempDir::new().unwrap();
    let (msi, dest) = build_paths(&tmp);

    // Media references #Cab1 but we don't write the stream → bail.
    build_msi(
        &msi,
        &[("TARGETDIR", None, ".")],
        &[("c1", "TARGETDIR")],
        &[(1, "#Cab1")],
        &[("f1", "c1", "missing.txt", 1)],
        &[], // <-- absent
    )
    .unwrap();

    let err = extract_msi_pure(&msi, &dest)
        .expect_err("missing embedded CAB stream must error");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("Cab1") || msg.to_lowercase().contains("stream"),
        "error should mention the missing stream; got: {msg}"
    );
}
