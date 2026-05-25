//! `msvc` provider: install the MSVC compiler + CRT + extras from one
//! Microsoft VS build, identified by its exact `build_version` string.
//!
//! Pinning a `build_version` resolves (via the roblabla mirror) to one
//! commit_sha → one immutable `manifest.json` → fixed CDN payload URLs.
//! Reproducible forever.
//!
//! The provider produces ONLY the MSVC tree. The Windows SDK is the
//! `windows_sdk` provider; they share the manifest source but nothing else.

use anyhow::{Context, Result, anyhow, bail};
use std::collections::BTreeSet;
use std::path::Path;
use xxhash_rust::xxh3::xxh3_64;

use super::vs_manifest::{self, MirrorUrls, Package, VsManifest, VsVersion};
use super::{InstallCtx, Installed, Provider};
use crate::cache::sanitize_fingerprint;
use crate::extract;

pub const ID: &str = "msvc";

const HOST: &str = "x64";
const TARGET: &str = "x64";

/// Family-relative globs that constitute `base_install = "default"`. Picked
/// to match all three Microsoft compiler-package naming schemes (modern
/// `.base`, older without tail, legacy `.Msi`).
const DEFAULT_PATTERNS: &[&str] = &[
    "Tools.HostX64.TargetX64*", // compiler + Res* locale resources
    "CRT.Headers*",
    "CRT.x64.Desktop*",
    "CRT.x64.Store*",
    "CRT.Redist.X64*",
];

// ---- option types ----------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BaseInstall {
    None,
    Default,
    Full,
}

impl BaseInstall {
    fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Default => "default",
            Self::Full => "full",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "none" => Ok(Self::None),
            "default" => Ok(Self::Default),
            "full" => Ok(Self::Full),
            other => bail!("invalid base_install '{other}'; valid: none, default, full"),
        }
    }
}

#[derive(Debug)]
struct Options {
    build_version: String,
    base: BaseInstall,
    extras: Vec<String>,
}

impl Options {
    fn parse(options: &toml::Table) -> Result<Self> {
        let build_version = required_str(options, "build_version")?.to_string();
        // Validate now so config-time typos surface without needing network.
        let major = vs_manifest::build_version_major(&build_version)
            .map_err(|e| anyhow!("`msvc` provider: {e}"))?;
        VsVersion::from_channel(major).map_err(|e| anyhow!("`msvc` provider: {e}"))?;
        let base = match optional_str(options, "base_install")? {
            Some(v) => BaseInstall::parse(v)?,
            None => BaseInstall::Default,
        };
        let extras = optional_string_list(options, "extras")?;

        if base == BaseInstall::None && extras.is_empty() {
            bail!("`msvc`: base_install='none' with empty extras would install nothing");
        }
        if base == BaseInstall::Full && !extras.is_empty() {
            bail!("`msvc`: base_install='full' already selects every package; remove extras");
        }
        Ok(Self {
            build_version,
            base,
            extras,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct PackageRequest {
    id: String,
    version: String,
    language: Option<String>,
}

// ---- provider --------------------------------------------------------------

pub struct MsvcProvider {
    urls: MirrorUrls,
}

impl MsvcProvider {
    pub fn new() -> Self {
        Self {
            urls: MirrorUrls::default(),
        }
    }

    pub fn with_urls(urls: MirrorUrls) -> Self {
        Self { urls }
    }
}

impl Default for MsvcProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for MsvcProvider {
    fn id(&self) -> &'static str {
        ID
    }

    fn install(&self, options: &toml::Table, ctx: &mut InstallCtx) -> Result<Installed> {
        let opts = Options::parse(options)?;
        let fp = fingerprint(&opts);

        if ctx.cache().install_present(&fp) {
            return Ok(Installed {
                fingerprint: fp,
                display: display_for(&opts),
                options: resolved_options(&opts),
                freshly_extracted: false,
            });
        }

        let entry = vs_manifest::resolve_build_version(&self.urls, &opts.build_version, ctx)?;
        let manifest = vs_manifest::fetch_manifest_at(&self.urls.raw_base, &entry.commit.sha, ctx)?;
        let compiler = find_primary_compiler(&manifest, entry.vs)
            .with_context(|| format!("locating primary compiler for build {}", opts.build_version))?;
        let family = family_prefix(&compiler.id)?;

        let selected = select_packages(&manifest, &family, &opts)?;
        let staging = ctx.staging_dir()?;
        for request in &selected {
            let pkg = lookup_package(&manifest, request)?;
            for payload in &pkg.payloads {
                let filename = payload_filename(payload);
                let archive = ctx
                    .download(&payload.url, payload.sha256.as_deref())
                    .with_context(|| format!("downloading {filename} for {}", pkg.id))?;
                extract_payload(&archive, &filename, &staging)
                    .with_context(|| format!("extracting {filename} for {}", pkg.id))?;
            }
        }

        Ok(Installed {
            fingerprint: fp,
            display: display_for(&opts),
            options: resolved_options(&opts),
            freshly_extracted: true,
        })
    }
}

// ---- primary-compiler detection --------------------------------------------

/// Find the snapshot's PRIMARY compiler-base package: the highest-version
/// `Microsoft.VC.<family>.<vs_major>.<vs_minor>.Tools.Host<H>.Target<T>.base`
/// whose `<vs_major>` matches the snapshot's channel.
///
/// Returns `&Package` so `family_prefix` can read the literal id. Legacy
/// `Microsoft.VisualC.*` / `Microsoft.VisualCpp.*` packages don't carry a
/// VS major in their id and are never primary — they're installable only
/// via `base_install = "full"` or an explicit `Microsoft.*` raw extra.
pub fn find_primary_compiler(manifest: &VsManifest, vs: VsVersion) -> Result<&Package> {
    let infix = format!(".tools.host{HOST}.target{TARGET}.base");
    let major_dot = format!("{}.", vs.channel());
    let mut best: Option<(&Package, Vec<u64>)> = None;
    for pkg in &manifest.packages {
        let id_lower = pkg.id.to_lowercase();
        if !id_lower.starts_with("microsoft.vc.") {
            continue;
        }
        let Some(tools_at) = id_lower.find(&infix) else {
            continue;
        };
        let body = &id_lower["microsoft.vc.".len()..tools_at];
        // Reject Preview/Premium and require a `.<vs_major>.<vs_minor>` suffix.
        if body.ends_with(".premium") || body.ends_with(".prem") {
            continue;
        }
        // body = "<msvc_family>.<vs_major>.<vs_minor>" (e.g., "14.41.17.11").
        // Verify the last two dot-segments form `.<vs.channel()>.<vs_minor>`.
        let segs: Vec<&str> = body.split('.').collect();
        if segs.len() < 4 {
            continue;
        }
        let vs_maj = segs[segs.len() - 2];
        let vs_min = segs[segs.len() - 1];
        if vs_maj != vs.channel().to_string() || vs_min.parse::<u64>().is_err() {
            continue;
        }
        let _ = major_dot; // silence unused — we already split.
        let Some(ver) = &pkg.version else { continue };
        let key = version_key(ver);
        let take = match &best {
            None => true,
            Some((_, k)) => &key > k,
        };
        if take {
            best = Some((pkg, key));
        }
    }
    best.map(|(p, _)| p).ok_or_else(|| {
        anyhow!(
            "no primary MSVC compiler-base package (Microsoft.VC.*.{0}.x.Tools.Host{HOST}.Target{TARGET}.base) found in this snapshot's manifest for {0}",
            vs.channel()
        )
    })
}

/// Derive the package-family prefix (e.g., `Microsoft.VC.14.51.17.11.`)
/// from a compiler-base package id.
pub fn family_prefix(compiler_id: &str) -> Result<String> {
    let lower = compiler_id.to_lowercase();
    let infix = format!(".tools.host{HOST}.target{TARGET}");
    let tools_at = lower
        .find(&infix)
        .ok_or_else(|| anyhow!("not a compiler-base package id: {compiler_id}"))?;
    Ok(format!("{}.", &compiler_id[..tools_at]))
}

fn version_key(v: &str) -> Vec<u64> {
    v.split('.').map(|s| s.parse::<u64>().unwrap_or(0)).collect()
}

// ---- selection -------------------------------------------------------------

fn select_packages(
    manifest: &VsManifest,
    family_prefix: &str,
    opts: &Options,
) -> Result<Vec<PackageRequest>> {
    let mut selected: BTreeSet<PackageRequest> = BTreeSet::new();

    match opts.base {
        BaseInstall::Full => {
            // Every package in the snapshot. Snapshots also contain LEGACY
            // compat toolsets at different family versions; "full" picks
            // those up too because users opting in want the full snapshot.
            for pkg in &manifest.packages {
                if pkg.version.is_some() {
                    selected.insert(package_request(pkg));
                }
            }
        }
        BaseInstall::Default => {
            for pattern in DEFAULT_PATTERNS {
                match_pattern_into(&mut selected, manifest, family_prefix, pattern)?;
            }
            for pattern in &opts.extras {
                match_pattern_into(&mut selected, manifest, family_prefix, pattern)?;
            }
        }
        BaseInstall::None => {
            for pattern in &opts.extras {
                match_pattern_into(&mut selected, manifest, family_prefix, pattern)?;
            }
        }
    }

    Ok(selected.into_iter().collect())
}

fn match_pattern_into(
    selected: &mut BTreeSet<PackageRequest>,
    manifest: &VsManifest,
    family_prefix: &str,
    pattern: &str,
) -> Result<()> {
    if pattern.is_empty() {
        bail!("msvc package pattern may not be empty");
    }
    let raw = starts_with_ignore_ascii_case(pattern, "Microsoft.");
    let compiled = glob::Pattern::new(pattern)
        .with_context(|| format!("msvc package pattern '{pattern}' is not a valid glob"))?;
    let opts = glob::MatchOptions {
        case_sensitive: false,
        require_literal_separator: false,
        require_literal_leading_dot: false,
    };

    // Family-prefix is the scoping mechanism inside a snapshot. We do NOT
    // filter by version: Microsoft routinely patches the compiler (cl.exe)
    // without bumping CRT/ATL/MFC versions in the same release. Restricting
    // to the family prefix already excludes legacy compat toolsets (which
    // have a different family prefix) and unrelated workloads.
    let mut matched = 0usize;
    for pkg in &manifest.packages {
        let in_family = starts_with_ignore_ascii_case(&pkg.id, family_prefix);
        let candidate = if raw {
            pkg.id.as_str()
        } else if in_family {
            &pkg.id[family_prefix.len()..]
        } else {
            continue;
        };
        if compiled.matches_with(candidate, opts) {
            matched += 1;
            selected.insert(package_request(pkg));
        }
    }
    if matched == 0 {
        let family = family_prefix.trim_end_matches('.');
        bail!("msvc package pattern '{pattern}' matched no packages in family {family}");
    }
    Ok(())
}

// ---- payload + manifest helpers --------------------------------------------

fn package_request(pkg: &Package) -> PackageRequest {
    PackageRequest {
        id: pkg.id.clone(),
        version: pkg.version.clone().unwrap_or_default(),
        language: pkg.language.clone(),
    }
}

fn lookup_package<'a>(manifest: &'a VsManifest, request: &PackageRequest) -> Result<&'a Package> {
    let matches_id_version = |p: &&Package| {
        p.id.eq_ignore_ascii_case(&request.id)
            && p.version.as_deref() == Some(request.version.as_str())
    };
    // Prefer exact-language match. When the request is languageless, prefer
    // the languageless manifest entry (some compiler-base packages are
    // languageless but appear after language-tagged variants).
    if let Some(want_lang) = request.language.as_deref() {
        if let Some(p) = manifest.packages.iter().find(|p| {
            matches_id_version(p) && p.language.as_deref() == Some(want_lang)
        }) {
            return Ok(p);
        }
    } else if let Some(p) = manifest
        .packages
        .iter()
        .find(|p| matches_id_version(p) && p.language.is_none())
    {
        return Ok(p);
    }
    // Fall back to first id+version match (covers language-mismatch edge cases).
    manifest
        .packages
        .iter()
        .find(matches_id_version)
        .ok_or_else(|| {
            anyhow!(
                "manifest no longer contains {} (version {}{})",
                request.id,
                request.version,
                request
                    .language
                    .as_deref()
                    .map(|l| format!(", language {l}"))
                    .unwrap_or_default()
            )
        })
}

fn payload_filename(payload: &vs_manifest::Payload) -> String {
    if let Some(name) = &payload.file_name {
        return name.clone();
    }
    if let Ok(parsed) = url::Url::parse(&payload.url) {
        if let Some(seg) = parsed.path_segments().and_then(|mut s| s.next_back()) {
            if !seg.is_empty() {
                return seg.to_string();
            }
        }
    }
    "unknown.bin".to_string()
}

fn extract_payload(archive: &Path, filename: &str, dest: &Path) -> Result<()> {
    let lower = filename.to_lowercase();
    if lower.ends_with(".vsix") || lower.ends_with(".zip") {
        extract::extract_vsix(archive, dest)?;
    } else if lower.ends_with(".msi") {
        extract::extract_msi(archive, dest)?;
    } else {
        tracing::warn!("skipping MSVC payload of unknown type: {filename}");
    }
    Ok(())
}

fn starts_with_ignore_ascii_case(value: &str, prefix: &str) -> bool {
    value.len() >= prefix.len() && value[..prefix.len()].eq_ignore_ascii_case(prefix)
}

// ---- option helpers --------------------------------------------------------

fn required_str<'a>(options: &'a toml::Table, key: &str) -> Result<&'a str> {
    options
        .get(key)
        .ok_or_else(|| anyhow!("`msvc` provider requires options.{key}"))?
        .as_str()
        .ok_or_else(|| anyhow!("`msvc` option '{key}' must be a string"))
}

fn optional_str<'a>(options: &'a toml::Table, key: &str) -> Result<Option<&'a str>> {
    match options.get(key) {
        None => Ok(None),
        Some(v) => v
            .as_str()
            .map(Some)
            .ok_or_else(|| anyhow!("`msvc` option '{key}' must be a string")),
    }
}

fn optional_string_list(options: &toml::Table, key: &str) -> Result<Vec<String>> {
    let Some(v) = options.get(key) else {
        return Ok(Vec::new());
    };
    let arr = v
        .as_array()
        .ok_or_else(|| anyhow!("`msvc` option '{key}' must be an array of strings"))?;
    let mut out = Vec::new();
    for item in arr {
        let s = item
            .as_str()
            .ok_or_else(|| anyhow!("`msvc` option '{key}' entries must be strings"))?;
        if s.is_empty() {
            bail!("`msvc` option '{key}' entries may not be empty");
        }
        if !out.iter().any(|x: &String| x == s) {
            out.push(s.to_string());
        }
    }
    Ok(out)
}

// ---- fingerprint + display -------------------------------------------------

fn fingerprint(opts: &Options) -> String {
    let mut key = String::new();
    key.push_str(&opts.build_version);
    key.push('\n');
    key.push_str(opts.base.as_str());
    key.push('\n');
    let mut extras = opts.extras.clone();
    extras.sort_by_key(|e| e.to_ascii_lowercase());
    for extra in extras {
        key.push_str("extra\t");
        key.push_str(&extra.to_ascii_lowercase());
        key.push('\n');
    }
    let hash = xxh3_64(key.as_bytes());
    sanitize_fingerprint(&format!("msvc-{}-{hash:016x}", opts.build_version))
}

fn display_for(opts: &Options) -> String {
    format!("msvc build {} (base={})", opts.build_version, opts.base.as_str())
}

fn resolved_options(opts: &Options) -> toml::Table {
    let mut o = toml::Table::new();
    o.insert(
        "build_version".into(),
        toml::Value::String(opts.build_version.clone()),
    );
    o.insert(
        "base_install".into(),
        toml::Value::String(opts.base.as_str().to_string()),
    );
    if !opts.extras.is_empty() {
        o.insert(
            "extras".into(),
            toml::Value::Array(
                opts.extras
                    .iter()
                    .map(|s| toml::Value::String(s.clone()))
                    .collect(),
            ),
        );
    }
    o
}

#[cfg(test)]
#[path = "tests/msvc.rs"]
mod tests;
