//! Pure-Rust, cross-platform MSI extractor.
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
use std::sync::Mutex;
use std::sync::mpsc;

/// `msidbFileAttributesNoncompressed` — file lives loose in the MSI
/// directory rather than inside a CAB. SDK MSIs don't use this; we
/// bail with a clear message if we ever encounter it.
const FILE_ATTR_NONCOMPRESSED: i32 = 0x2000;

/// Extract every file in `msi` into `dest`, laid out per the MSI's
/// `Directory` table (with the root entry — `TARGETDIR` — mapped to
/// `dest`).
///
/// Creates `dest` if missing. Overwrites existing files silently;
/// callers (e.g. `windows_sdk` install) manage scratch dirs themselves.
pub fn extract_msi(msi: &Path, dest: &Path) -> Result<()> {
    tracing::debug!("extract_msi {} -> {}", msi.display(), dest.display());
    let units = plan_msi(msi, dest)?;
    for unit in &units {
        extract_cab_unit(unit)?;
    }
    Ok(())
}

/// Where a CAB's bytes live: a loose sibling file on disk, or a `#CabN`
/// stream embedded inside the MSI itself.
enum CabSource {
    External(PathBuf),
    Embedded { msi: PathBuf, stream: String },
}

/// One unit of extraction work: a single CAB plus the files to pull from it.
/// CABs are the natural parallelism granularity — each is an independent
/// compressed container — and a single SDK MSI typically references dozens of
/// external CABs, so planning at this level lets the worker pool saturate all
/// cores instead of bottlenecking on one giant MSI.
struct CabUnit {
    source: CabSource,
    cab_name: String,
    jobs: Vec<ExtractJob>,
}

/// Read an MSI's tables and resolve its file layout into a list of per-CAB
/// extraction units, without touching any CAB bytes. Cheap (metadata only).
fn plan_msi(msi: &Path, dest: &Path) -> Result<Vec<CabUnit>> {
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

    let mut units = Vec::with_capacity(jobs_by_cab.len());
    for (cab_name, jobs) in jobs_by_cab {
        let source = if let Some(stream_name) = cab_name.strip_prefix('#') {
            CabSource::Embedded {
                msi: msi.to_path_buf(),
                stream: stream_name.to_string(),
            }
        } else {
            CabSource::External(msi_dir.join(&cab_name))
        };
        units.push(CabUnit {
            source,
            cab_name,
            jobs,
        });
    }
    Ok(units)
}

/// Open a unit's CAB and stream its files out to their resolved dest paths.
fn extract_cab_unit(unit: &CabUnit) -> Result<()> {
    match &unit.source {
        CabSource::External(cab_path) => {
            let cab_file = File::open(cab_path)
                .with_context(|| format!("opening external CAB {}", cab_path.display()))?;
            extract_cab(cab_file, &unit.jobs, &unit.cab_name)
        }
        CabSource::Embedded { msi, stream } => {
            // Re-open the MSI per embedded unit so units stay independent and
            // Send-able across worker threads. SDK MSIs use external sibling
            // CABs, so this path is rarely hit.
            let mut package = msi::open(msi)
                .with_context(|| format!("re-opening MSI {} for embedded CAB", msi.display()))?;
            let reader = package.read_stream(stream).with_context(|| {
                format!("opening embedded CAB stream #{stream} in {}", msi.display())
            })?;
            extract_cab(reader, &unit.jobs, &unit.cab_name)
        }
    }
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
) -> Result<Vec<(i32, Option<String>)>> {
    if !package.has_table("Media") {
        bail!("MSI has no Media table");
    }
    let mut out: Vec<(i32, Option<String>)> = Vec::new();
    let rows = package
        .select_rows(Select::table("Media"))
        .context("select Media")?;
    for row in rows {
        let last_seq = row["LastSequence"].as_int().ok_or_else(|| {
            anyhow!("Media row has null/non-int LastSequence")
        })?;
        // Null/empty Cabinet is valid: SDK header-only MSIs declare an
        // empty `Media` row with `LastSequence=0` and no CAB. If a real
        // `File` row maps to such a row, the error surfaces at
        // file-walk time with file-specific context.
        let cabinet = row["Cabinet"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(String::from);
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
    media: &[(i32, Option<String>)],
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

fn find_cab_for_sequence(media: &[(i32, Option<String>)], sequence: i32) -> Option<&str> {
    media
        .iter()
        .find(|(last_seq, _)| *last_seq >= sequence)
        .and_then(|(_, name)| name.as_deref())
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

// ---- batch / parallel extraction -------------------------------------------

/// Extract every MSI in `msis` into the shared `dest` directory, parallelized
/// across a worker pool at **CAB granularity** rather than per-MSI. SDK MSIs
/// vary wildly in size (a CRT-sources MSI dwarfs a header MSI), so a per-MSI
/// pool leaves cores idle once the small MSIs finish; planning every MSI into
/// its constituent CABs first (each an independent container, and one MSI
/// references dozens of external sibling CABs) gives hundreds of evenly-sized
/// work units that keep all cores busy.
///
/// CABs may safely write to the same `dest` concurrently — files land under
/// their `Directory`-table paths, and the rare case where two CABs touch the
/// same path (a header shared across components) writes identical content, so
/// last-writer-wins is benign.
///
/// On the first failure: drains the queue so in-flight workers exit after
/// their current task, then returns the error.
pub fn extract_msis_in_parallel(msis: &[PathBuf], dest: &Path) -> Result<()> {
    if msis.is_empty() {
        return Ok(());
    }

    // Plan phase: read each MSI's tables (metadata only — fast) and flatten
    // into one big list of per-CAB units.
    let mut units: Vec<CabUnit> = Vec::new();
    for msi in msis {
        let mut planned = plan_msi(msi, dest)
            .with_context(|| format!("planning MSI {}", msi.display()))?;
        units.append(&mut planned);
    }
    let total = units.len();
    if total == 0 {
        return Ok(());
    }
    tracing::info!("extracting {total} CABs across {} MSIs", msis.len());

    let worker_count = crate::fs_util::worker_count(total);
    if worker_count <= 1 {
        for unit in &units {
            extract_cab_unit(unit)
                .with_context(|| format!("extracting CAB {}", unit.cab_name))?;
        }
        return Ok(());
    }

    let queue: Mutex<Vec<usize>> = Mutex::new((0..total).rev().collect());
    let (tx, rx) = mpsc::channel::<Result<()>>();

    std::thread::scope(|s| -> Result<()> {
        for _ in 0..worker_count {
            let tx = tx.clone();
            let queue = &queue;
            let units = &units;
            s.spawn(move || loop {
                let idx = match queue.lock().unwrap().pop() {
                    Some(i) => i,
                    None => return,
                };
                let unit = &units[idx];
                let result = extract_cab_unit(unit)
                    .with_context(|| format!("extracting CAB {}", unit.cab_name));
                if tx.send(result).is_err() {
                    return;
                }
            });
        }
        drop(tx);

        // Cancel = drain the queue so in-flight workers exit after their
        // current task instead of grinding through every remaining job
        // before scope() can join.
        let cancel = || queue.lock().unwrap().clear();

        for msg in rx {
            if let Err(e) = msg {
                cancel();
                return Err(e);
            }
        }
        Ok(())
    })
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
