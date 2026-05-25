//! Pure-Rust MSI extractor — cross-platform replacement for shelling
//! out to `msiexec.exe /a`.
//!
//! Walks an MSI's `Directory` / `Component` / `Media` / `File` tables
//! to figure out the intended on-disk layout, then opens each CAB
//! (embedded as an MSI stream named `#CabN`, or external as a sibling
//! file on disk) and streams contents out via the [`cab`] crate.
//!
//! Scope: SDK-style MSIs (vanilla file-payload installers). Does not
//! evaluate install conditions, run custom actions, or expand
//! `MergeModules` — SDK MSIs don't need any of that. Any MSI feature
//! we don't implement results in a `bail!` with a clear message
//! rather than silently producing a wrong tree.

use anyhow::{Context, Result, anyhow, bail};
use cab::{Cabinet, CompressionType};
use msi::{PackageType, Select};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};

/// `msidbFileAttributesNoncompressed` — file lives loose in the MSI
/// directory rather than inside a CAB. SDK MSIs don't use this; we
/// bail with a clear message if we ever encounter it.
const FILE_ATTR_NONCOMPRESSED: i32 = 0x2000;

/// Extract every file in `msi` into `dest`, laid out per the MSI's
/// `Directory` table (with the root entry — `TARGETDIR` — mapped to
/// `dest`).
///
/// Creates `dest` if missing. Overwrites existing files silently to
/// match msiexec /a behavior; callers (e.g. `windows_sdk` install)
/// manage scratch dirs themselves.
pub fn extract_msi_pure(msi: &Path, dest: &Path) -> Result<()> {
    tracing::debug!("extract_msi_pure {} -> {}", msi.display(), dest.display());
    std::fs::create_dir_all(dest)
        .with_context(|| format!("creating {}", dest.display()))?;

    let msi_dir = msi
        .parent()
        .ok_or_else(|| anyhow!("MSI path has no parent: {}", msi.display()))?;

    let mut package = msi::open(msi)
        .with_context(|| format!("opening MSI {}", msi.display()))?;
    if package.package_type() != PackageType::Installer {
        bail!(
            "{}: not an Installer-type package (got {:?})",
            msi.display(),
            package.package_type()
        );
    }

    let directories = read_directory_rows(&mut package)
        .with_context(|| format!("reading Directory table from {}", msi.display()))?;
    let components = read_component_map(&mut package)
        .with_context(|| format!("reading Component table from {}", msi.display()))?;
    let media = read_media_table(&mut package)
        .with_context(|| format!("reading Media table from {}", msi.display()))?;
    let dir_paths = resolve_directories(&directories, dest)?;

    let jobs_by_cab = enumerate_files(&mut package, &components, &dir_paths, &media)
        .with_context(|| format!("enumerating File rows from {}", msi.display()))?;

    for (cab_name, jobs) in jobs_by_cab {
        if let Some(stream_name) = cab_name.strip_prefix('#') {
            let stream = package.read_stream(stream_name).with_context(|| {
                format!("opening embedded CAB stream {cab_name} in {}", msi.display())
            })?;
            extract_cab(stream, &jobs, &cab_name)?;
        } else {
            let cab_path = msi_dir.join(&cab_name);
            let cab_file = File::open(&cab_path)
                .with_context(|| format!("opening external CAB {}", cab_path.display()))?;
            extract_cab(cab_file, &jobs, &cab_name)?;
        }
    }

    Ok(())
}

// ---- table readers ---------------------------------------------------------

/// One row of the Directory table: (parent key, DefaultDir field).
type DirRow = (Option<String>, String);

fn read_directory_rows<F: Read + Seek>(
    package: &mut msi::Package<F>,
) -> Result<HashMap<String, DirRow>> {
    if !package.has_table("Directory") {
        bail!("MSI has no Directory table");
    }
    let mut out = HashMap::new();
    let rows = package
        .select_rows(Select::table("Directory"))
        .context("select Directory")?;
    for row in rows {
        let key = require_str(&row, "Directory")?.to_string();
        let parent_val = &row["Directory_Parent"];
        let parent = if parent_val.is_null() {
            None
        } else {
            parent_val
                .as_str()
                .filter(|s| !s.is_empty())
                .map(String::from)
        };
        let default_dir = require_str(&row, "DefaultDir")?.to_string();
        out.insert(key, (parent, default_dir));
    }
    Ok(out)
}

fn read_component_map<F: Read + Seek>(
    package: &mut msi::Package<F>,
) -> Result<HashMap<String, String>> {
    if !package.has_table("Component") {
        bail!("MSI has no Component table");
    }
    let mut out = HashMap::new();
    let rows = package
        .select_rows(Select::table("Component"))
        .context("select Component")?;
    for row in rows {
        let key = require_str(&row, "Component")?.to_string();
        let dir = require_str(&row, "Directory_")?.to_string();
        out.insert(key, dir);
    }
    Ok(out)
}

fn read_media_table<F: Read + Seek>(
    package: &mut msi::Package<F>,
) -> Result<Vec<(i32, String)>> {
    if !package.has_table("Media") {
        bail!("MSI has no Media table");
    }
    let mut out: Vec<(i32, String)> = Vec::new();
    let rows = package
        .select_rows(Select::table("Media"))
        .context("select Media")?;
    for row in rows {
        let last_seq = row["LastSequence"].as_int().ok_or_else(|| {
            anyhow!("Media row has null/non-int LastSequence")
        })?;
        let cabinet_val = &row["Cabinet"];
        let cabinet = if cabinet_val.is_null() {
            bail!(
                "Media row (LastSequence={last_seq}) has null Cabinet — \
                 loose-file extraction is not implemented"
            );
        } else {
            cabinet_val
                .as_str()
                .ok_or_else(|| {
                    anyhow!("Media row (LastSequence={last_seq}) has non-string Cabinet")
                })?
                .to_string()
        };
        out.push((last_seq, cabinet));
    }
    out.sort_by_key(|&(seq, _)| seq);
    Ok(out)
}

// ---- directory resolver ----------------------------------------------------

fn resolve_directories(
    rows: &HashMap<String, DirRow>,
    dest: &Path,
) -> Result<HashMap<String, PathBuf>> {
    let mut cache: HashMap<String, PathBuf> = HashMap::new();
    for key in rows.keys() {
        resolve_one_dir(key, rows, dest, &mut cache)?;
    }
    Ok(cache)
}

fn resolve_one_dir(
    key: &str,
    rows: &HashMap<String, DirRow>,
    dest: &Path,
    cache: &mut HashMap<String, PathBuf>,
) -> Result<PathBuf> {
    if let Some(p) = cache.get(key) {
        return Ok(p.clone());
    }
    // TARGETDIR (and SourceDir, for transform-style MSIs) is the root
    // sentinel — the Installer runtime substitutes the caller's target
    // path here regardless of the DefaultDir field. msiexec /a does the
    // same; we map it directly to `dest`.
    if is_root_key(key) {
        cache.insert(key.to_string(), dest.to_path_buf());
        return Ok(dest.to_path_buf());
    }
    let (parent, default_dir) = rows
        .get(key)
        .ok_or_else(|| anyhow!("unknown Directory key '{key}'"))?;
    let parent_path = match parent {
        None => dest.to_path_buf(),
        Some(p) => resolve_one_dir(p, rows, dest, cache)?,
    };
    let segment = parse_dir_segment(default_dir);
    let result = if segment.is_empty() || segment == "." {
        parent_path
    } else {
        parent_path.join(segment)
    };
    cache.insert(key.to_string(), result.clone());
    Ok(result)
}

fn is_root_key(key: &str) -> bool {
    key.eq_ignore_ascii_case("TARGETDIR") || key.eq_ignore_ascii_case("SourceDir")
}

/// Pick the target-side, long-name segment from a `DefaultDir` field.
///
/// MSI format: `target_short[|target_long][:source_short[|source_long]]`.
/// We want `target_long` (or `target_short` if no long is present).
fn parse_dir_segment(s: &str) -> String {
    let target = s.split(':').next().unwrap_or(s);
    pick_long_name(target)
}

/// `"short|long"` → `"long"`; plain `"name"` → `"name"`.
fn pick_long_name(s: &str) -> String {
    match s.split_once('|') {
        Some((_, long)) => long.to_string(),
        None => s.to_string(),
    }
}

// ---- File table walk + CAB grouping ----------------------------------------

struct ExtractJob {
    file_key: String,
    dest_path: PathBuf,
}

fn enumerate_files<F: Read + Seek>(
    package: &mut msi::Package<F>,
    components: &HashMap<String, String>,
    dir_paths: &HashMap<String, PathBuf>,
    media: &[(i32, String)],
) -> Result<HashMap<String, Vec<ExtractJob>>> {
    if !package.has_table("File") {
        bail!("MSI has no File table");
    }
    let mut by_cab: HashMap<String, Vec<ExtractJob>> = HashMap::new();
    let rows = package
        .select_rows(Select::table("File"))
        .context("select File")?;
    for row in rows {
        let file_key = require_str(&row, "File")?.to_string();
        let component_key = require_str(&row, "Component_")
            .with_context(|| format!("File {file_key}"))?
            .to_string();
        let filename = require_str(&row, "FileName")
            .with_context(|| format!("File {file_key}"))?
            .to_string();
        let sequence = row["Sequence"]
            .as_int()
            .ok_or_else(|| anyhow!("File {file_key} has null/non-int Sequence"))?;
        let attributes = row["Attributes"].as_int().unwrap_or(0);

        if attributes & FILE_ATTR_NONCOMPRESSED != 0 {
            bail!(
                "File {file_key} has msidbFileAttributesNoncompressed (0x2000); \
                 loose-file extraction is not implemented"
            );
        }

        let dir_key = components
            .get(&component_key)
            .ok_or_else(|| {
                anyhow!("File {file_key} references unknown Component '{component_key}'")
            })?;
        let dir_path = dir_paths.get(dir_key).ok_or_else(|| {
            anyhow!("File {file_key} resolves to unknown Directory '{dir_key}'")
        })?;
        let long_name = pick_long_name(&filename);
        let dest_path = dir_path.join(&long_name);

        let cab_name = find_cab_for_sequence(media, sequence)
            .ok_or_else(|| {
                anyhow!(
                    "File {file_key} (Sequence={sequence}) has no matching Media row"
                )
            })?
            .to_string();

        by_cab.entry(cab_name).or_default().push(ExtractJob {
            file_key,
            dest_path,
        });
    }
    Ok(by_cab)
}

fn find_cab_for_sequence(media: &[(i32, String)], sequence: i32) -> Option<&str> {
    media
        .iter()
        .find(|(last_seq, _)| *last_seq >= sequence)
        .map(|(_, name)| name.as_str())
}

// ---- per-CAB extraction ----------------------------------------------------

fn extract_cab<R: Read + Seek>(reader: R, jobs: &[ExtractJob], cab_name: &str) -> Result<()> {
    let mut cabinet = Cabinet::new(reader)
        .with_context(|| format!("opening CAB {cab_name}"))?;

    // Surface Quantum up-front rather than waiting for read_file to
    // fail. Quantum is uncommon in Microsoft installers; if we ever hit
    // it the message tells the user to file a report.
    for folder in cabinet.folder_entries() {
        if matches!(folder.compression_type(), CompressionType::Quantum(_, _)) {
            bail!(
                "CAB {cab_name} uses Quantum compression, which the `cab` crate \
                 does not support. Quantum is uncommon in Microsoft installers; \
                 please file a report so we can investigate."
            );
        }
    }

    for job in jobs {
        if let Some(parent) = job.dest_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut reader = cabinet
            .read_file(&job.file_key)
            .with_context(|| format!("reading {} from CAB {cab_name}", job.file_key))?;
        let mut writer = File::create(&job.dest_path)
            .with_context(|| format!("creating {}", job.dest_path.display()))?;
        std::io::copy(&mut reader, &mut writer).with_context(|| {
            format!("writing {} -> {}", job.file_key, job.dest_path.display())
        })?;
    }
    Ok(())
}

// ---- small helpers ---------------------------------------------------------

/// Look up a required string-typed column. Panics from `row[name]` (the
/// `msi` crate's Index impl panics on missing column) are converted to
/// a regular error with table-column context in the caller's chain.
fn require_str<'a>(row: &'a msi::Row, name: &str) -> Result<&'a str> {
    if !row.has_column(name) {
        bail!("row missing required column '{name}'");
    }
    row[name]
        .as_str()
        .ok_or_else(|| anyhow!("column '{name}' is null or non-string"))
}

#[cfg(test)]
#[path = "tests/extract_msi.rs"]
mod tests;
