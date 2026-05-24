//! `msvc` provider: downloads MSVC packages from a Visual Studio channel
//! manifest. Windows-specific install uses `msiexec.exe` / VSIX zip
//! extraction.
//!
//! NOTE: this provider produces ONLY the MSVC tree. The Windows SDK is a
//! separate provider (`windows_sdk`). The two are logically independent.

use anyhow::{anyhow, bail, Context, Result};
use std::collections::BTreeSet;
use xxhash_rust::xxh3::xxh3_64;

use super::vs_manifest::{self, MsvcCandidate, VsManifest, VsVersion};
use super::{InstallCtx, Installed, Provider};
use crate::cache::sanitize_fingerprint;
use crate::extract;

pub const ID: &str = "msvc";

const HOST: &str = "x64";
const TARGET: &str = "x64";
const COMPILER_PACKAGE_SUFFIX: &str = ".Tools.HostX64.TargetX64.base";
const COMPILER_PACKAGE_SUFFIX_LOWER: &str = ".tools.hostx64.targetx64.base";

const STANDARD_PATTERNS: &[&str] = &[
    "Tools.HostX64.TargetX64.base",
    // Compiler resource packages are tiny, so standard installs every
    // manifest locale instead of requiring a locale-selection option.
    "Tools.HostX64.TargetX64.Res*",
    "CRT.Headers.base",
    "CRT.x64.Desktop.base",
    "CRT.x64.Store.base",
    "CRT.Redist.X64.base",
];

/// Archive mirror for old MSVC manifests. Microsoft live channel manifests
/// are moving targets and may stop listing older toolsets; pinned
/// `msvc_version` falls back to this mirror. Also used by the `msvc`
/// debug CLI to enumerate historical versions.
pub const ARCHIVE_MANIFEST_URL_TEMPLATE: &str =
    "https://raw.githubusercontent.com/roblabla/msvc-manifest-history/release-{channel}/manifest.json";

pub struct MsvcProvider {
    channel_url_template: String,
    archive_manifest_url_template: String,
}

#[derive(Debug)]
struct ResolvedMsvcToolset {
    manifest: VsManifest,
    package_version: String,
    family_prefix: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MsvcSelection {
    vs: VsVersion,
    profile: MsvcProfile,
    include: Vec<String>,
    extras: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MsvcProfile {
    Standard,
    Custom,
    Full,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct PackageRequest {
    id: String,
    version: Option<String>,
    language: Option<String>,
}

impl MsvcProvider {
    pub fn new() -> Self {
        Self {
            channel_url_template: vs_manifest::DEFAULT_CHANNEL_URL_TEMPLATE.to_string(),
            archive_manifest_url_template: ARCHIVE_MANIFEST_URL_TEMPLATE.to_string(),
        }
    }

    pub fn with_channel_url_template(template: impl Into<String>) -> Self {
        Self {
            channel_url_template: template.into(),
            ..Self::new()
        }
    }

    #[cfg(test)]
    fn with_archive_manifest_url_template(mut self, template: impl Into<String>) -> Self {
        self.archive_manifest_url_template = template.into();
        self
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
        let selection = MsvcSelection::from_options(options)?;
        let pinned_version = options.get("msvc_version").and_then(|v| v.as_str());

        if let Some(version) = pinned_version {
            let fp = msvc_fingerprint(version, &selection);
            if ctx.cache().install_present(&fp) {
                return Ok(Installed {
                    fingerprint: fp,
                    display: format!("msvc {version} ({})", selection.vs.as_str()),
                    options: resolved_options(options, version, &selection),
                    freshly_extracted: false,
                });
            }
        }

        let resolved = self.resolve_toolset(selection.vs.channel(), pinned_version, ctx)?;
        let packages = select_msvc_packages(&resolved, &selection)?;
        let fp = msvc_fingerprint(&resolved.package_version, &selection);

        if ctx.cache().install_present(&fp) {
            return Ok(Installed {
                fingerprint: fp,
                display: format!(
                    "msvc {} ({})",
                    resolved.package_version,
                    selection.vs.as_str()
                ),
                options: resolved_options(options, &resolved.package_version, &selection),
                freshly_extracted: false,
            });
        }

        let payloads = collect_payloads(&resolved.manifest, &packages)?;

        let staging_raw = ctx.staging_dir()?;
        for (url, sha, filename) in &payloads {
            let archive_path = ctx.download(url, sha.as_deref())?;
            extract_payload(&archive_path, filename, &staging_raw)?;
        }

        Ok(Installed {
            fingerprint: fp,
            display: format!(
                "msvc {} ({})",
                resolved.package_version,
                selection.vs.as_str()
            ),
            options: resolved_options(options, &resolved.package_version, &selection),
            freshly_extracted: true,
        })
    }
}

impl MsvcProvider {
    fn resolve_toolset(
        &self,
        vs_channel: &str,
        pinned_version: Option<&str>,
        ctx: &InstallCtx,
    ) -> Result<ResolvedMsvcToolset> {
        let live = self.fetch_live_manifest(vs_channel, ctx);

        let Some(requested) = pinned_version else {
            let manifest = live?;
            let candidate = select_latest_candidate(&manifest)?;
            return resolved_toolset(manifest, candidate);
        };

        let mut notes = Vec::new();
        match live {
            Ok(manifest) => {
                if let Some(candidate) = select_pinned_candidate(&manifest, requested) {
                    return resolved_toolset(manifest, candidate);
                }
                notes.push(format!(
                    "Microsoft live manifest did not contain {requested}; available versions: {}",
                    format_available_versions(&manifest)
                ));
            }
            Err(err) => notes.push(format!("Microsoft live manifest failed: {err:#}")),
        }

        match self.fetch_archive_manifest(vs_channel, ctx) {
            Ok(manifest) => {
                if let Some(candidate) = select_pinned_candidate(&manifest, requested) {
                    return resolved_toolset(manifest, candidate);
                }
                notes.push(format!(
                    "archived manifest did not contain {requested}; available versions: {}",
                    format_available_versions(&manifest)
                ));
            }
            Err(err) => notes.push(format!("archived manifest failed: {err:#}")),
        }

        bail!(
            "could not resolve pinned msvc_version='{requested}' for vs channel '{vs_channel}'; {}",
            notes.join("; ")
        )
    }

    fn fetch_live_manifest(&self, vs_channel: &str, ctx: &InstallCtx) -> Result<VsManifest> {
        vs_manifest::fetch_vs_manifest(&self.channel_url_template, vs_channel, ctx).with_context(
            || format!("resolving Microsoft live VS manifest for channel {vs_channel}"),
        )
    }

    fn fetch_archive_manifest(&self, vs_channel: &str, ctx: &InstallCtx) -> Result<VsManifest> {
        vs_manifest::fetch_archive_manifest(&self.archive_manifest_url_template, vs_channel, ctx)
    }
}

fn select_latest_candidate(manifest: &VsManifest) -> Result<MsvcCandidate> {
    let candidates = manifest.find_msvc_candidates(HOST, TARGET);
    if candidates.is_empty() {
        bail!("no MSVC packages found in VS manifest for host={HOST} target={TARGET}");
    }
    Ok(candidates[0].clone())
}

fn select_pinned_candidate(manifest: &VsManifest, requested: &str) -> Option<MsvcCandidate> {
    manifest
        .find_msvc_candidates(HOST, TARGET)
        .into_iter()
        .find(|candidate| candidate.package_version == requested)
}

fn format_available_versions(manifest: &VsManifest) -> String {
    let versions: Vec<_> = manifest
        .find_msvc_candidates(HOST, TARGET)
        .into_iter()
        .map(|candidate| candidate.package_version)
        .collect();
    if versions.is_empty() {
        "<none>".to_string()
    } else {
        versions.join(", ")
    }
}

fn resolved_toolset(manifest: VsManifest, candidate: MsvcCandidate) -> Result<ResolvedMsvcToolset> {
    let family_prefix = family_prefix_from_compiler_package(&candidate.package_id)?;
    Ok(ResolvedMsvcToolset {
        manifest,
        package_version: candidate.package_version,
        family_prefix,
    })
}

impl MsvcSelection {
    fn from_options(options: &toml::Table) -> Result<Self> {
        let vs = options
            .get("vs")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                anyhow!("`msvc` provider requires options.vs (vs2019, vs2022, or vs2026)")
            })
            .and_then(VsVersion::parse)?;

        reject_removed_options(options)?;

        let profile = match option_str(options, "profile")?.unwrap_or("standard") {
            "standard" => MsvcProfile::Standard,
            "custom" => MsvcProfile::Custom,
            "full" => MsvcProfile::Full,
            other => {
                bail!("invalid msvc profile '{other}'; valid profiles: standard, custom, full")
            }
        };

        let include = option_string_list(options, "include", &[])?;
        let extras = option_string_list(options, "extras", &[])?;

        match profile {
            MsvcProfile::Standard => {
                if options.contains_key("include") {
                    bail!("msvc profile 'standard' uses built-in patterns plus optional 'extras'; remove 'include' or use profile='custom'");
                }
            }
            MsvcProfile::Custom => {
                if include.is_empty() {
                    bail!("msvc profile 'custom' requires a non-empty 'include' pattern list");
                }
                if options.contains_key("extras") {
                    bail!("msvc profile 'custom' uses 'include'; 'extras' only applies to profile='standard'");
                }
            }
            MsvcProfile::Full => {
                if options.contains_key("include") || options.contains_key("extras") {
                    bail!("msvc profile 'full' selects every package in the resolved family and does not accept 'include' or 'extras'");
                }
            }
        }

        Ok(Self {
            vs,
            profile,
            include,
            extras,
        })
    }

    fn resolved_options(&self, options: &toml::Table, resolved_ver: &str) -> toml::Table {
        let mut o = options.clone();
        o.insert(
            "vs".into(),
            toml::Value::String(self.vs.as_str().to_string()),
        );
        o.insert(
            "msvc_version".into(),
            toml::Value::String(resolved_ver.to_string()),
        );
        o.insert(
            "profile".into(),
            toml::Value::String(self.profile.as_str().to_string()),
        );
        match self.profile {
            MsvcProfile::Standard => {
                if !self.extras.is_empty() {
                    insert_string_array(&mut o, "extras", &self.extras);
                }
            }
            MsvcProfile::Custom => insert_string_array(&mut o, "include", &self.include),
            MsvcProfile::Full => {}
        }
        o
    }
}

impl MsvcProfile {
    fn as_str(self) -> &'static str {
        match self {
            MsvcProfile::Standard => "standard",
            MsvcProfile::Custom => "custom",
            MsvcProfile::Full => "full",
        }
    }
}

fn select_msvc_packages(
    resolved: &ResolvedMsvcToolset,
    selection: &MsvcSelection,
) -> Result<Vec<PackageRequest>> {
    let mut selected = BTreeSet::new();

    match selection.profile {
        MsvcProfile::Standard => {
            for pattern in STANDARD_PATTERNS {
                add_pattern_matches(&mut selected, resolved, pattern)?;
            }
            for pattern in &selection.extras {
                add_pattern_matches(&mut selected, resolved, pattern)?;
            }
        }
        MsvcProfile::Custom => {
            for pattern in &selection.include {
                add_pattern_matches(&mut selected, resolved, pattern)?;
            }
        }
        MsvcProfile::Full => add_full_family_matches(&mut selected, resolved)?,
    }

    include_declared_metadata_dependencies(&mut selected, resolved)?;
    Ok(selected.into_iter().collect())
}

fn add_full_family_matches(
    selected: &mut BTreeSet<PackageRequest>,
    resolved: &ResolvedMsvcToolset,
) -> Result<()> {
    let before = selected.len();
    for pkg in &resolved.manifest.packages {
        if is_resolved_package(pkg, resolved)
            && starts_with_ignore_ascii_case(&pkg.id, &resolved.family_prefix)
        {
            selected.insert(package_request(pkg));
        }
    }
    if selected.len() == before {
        bail!(
            "no MSVC packages found for package family prefix {}",
            resolved.family_prefix
        );
    }
    Ok(())
}

fn add_pattern_matches(
    selected: &mut BTreeSet<PackageRequest>,
    resolved: &ResolvedMsvcToolset,
    pattern: &str,
) -> Result<()> {
    if pattern.is_empty() {
        bail!("msvc package patterns may not be empty");
    }

    let compiled = glob::Pattern::new(pattern)
        .with_context(|| format!("msvc package pattern '{pattern}' is not a valid glob"))?;
    let raw_pattern = starts_with_ignore_ascii_case(pattern, "Microsoft.");
    let mut matched = 0usize;
    for pkg in &resolved.manifest.packages {
        if !is_resolved_package(pkg, resolved) {
            continue;
        }

        let matches = if raw_pattern {
            glob_match(&compiled, &pkg.id)
        } else if starts_with_ignore_ascii_case(&pkg.id, &resolved.family_prefix) {
            let key = &pkg.id[resolved.family_prefix.len()..];
            glob_match(&compiled, key)
        } else {
            false
        };

        if matches {
            matched += 1;
            selected.insert(package_request(pkg));
        }
    }

    if matched == 0 {
        bail!(
            "msvc package pattern '{pattern}' matched no packages for resolved family {} ({})",
            resolved.family_prefix,
            resolved.package_version
        );
    }

    Ok(())
}

fn glob_match(pattern: &glob::Pattern, text: &str) -> bool {
    // Package ids look like dot-separated identifiers, not paths. Disable
    // glob's path-aware behaviors so `*` matches dots and a leading dot is
    // not special.
    pattern.matches_with(
        text,
        glob::MatchOptions {
            case_sensitive: false,
            require_literal_separator: false,
            require_literal_leading_dot: false,
        },
    )
}

fn include_declared_metadata_dependencies(
    selected: &mut BTreeSet<PackageRequest>,
    resolved: &ResolvedMsvcToolset,
) -> Result<()> {
    loop {
        let current: Vec<_> = selected.iter().cloned().collect();
        let mut changed = false;
        for request in current {
            let Some(pkg) = find_requested_package(&resolved.manifest, &request)? else {
                bail!("package not found in manifest: {}", request.id);
            };
            for dep_id in pkg.dependencies.keys() {
                let dep_lower = dep_id.to_lowercase();
                if is_metadata_dependency(&dep_lower) {
                    changed |= add_exact_manifest_id(selected, resolved, dep_id)?;
                }
            }
        }
        if !changed {
            break;
        }
    }
    Ok(())
}

fn add_exact_manifest_id(
    selected: &mut BTreeSet<PackageRequest>,
    resolved: &ResolvedMsvcToolset,
    id: &str,
) -> Result<bool> {
    let mut found = false;
    let mut changed = false;
    for pkg in &resolved.manifest.packages {
        if is_resolved_package(pkg, resolved) && pkg.id.eq_ignore_ascii_case(id) {
            found = true;
            changed |= selected.insert(package_request(pkg));
        }
    }
    if !found {
        bail!("declared MSVC metadata dependency not found in manifest: {id}");
    }
    Ok(changed)
}

fn package_request(pkg: &vs_manifest::Package) -> PackageRequest {
    PackageRequest {
        id: pkg.id.clone(),
        version: pkg.version.clone(),
        language: pkg.language.clone(),
    }
}

fn is_resolved_package(pkg: &vs_manifest::Package, resolved: &ResolvedMsvcToolset) -> bool {
    pkg.version.as_deref() == Some(resolved.package_version.as_str())
}

/// Look up each package id in the manifest and gather all its payloads.
fn collect_payloads(
    manifest: &VsManifest,
    packages: &[PackageRequest],
) -> Result<Vec<(String, Option<String>, String)>> {
    let mut out = Vec::new();
    for request in packages {
        let pkg = find_requested_package(manifest, request)?
            .ok_or_else(|| anyhow!("package not found in manifest: {}", request.id))?;
        for p in &pkg.payloads {
            let filename = p
                .file_name
                .clone()
                .or_else(|| filename_from_url(&p.url))
                .unwrap_or_else(|| "unknown.bin".to_string());
            out.push((p.url.clone(), p.sha256.clone(), filename));
        }
    }
    Ok(out)
}

fn filename_from_url(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    parsed
        .path_segments()
        .and_then(|mut s| s.next_back())
        .map(|s| s.to_string())
}

fn find_requested_package<'a>(
    manifest: &'a VsManifest,
    request: &PackageRequest,
) -> Result<Option<&'a vs_manifest::Package>> {
    if let Some(language) = &request.language {
        return Ok(manifest
            .packages
            .iter()
            .find(|pkg| {
                request_matches_package(request, pkg)
                    && pkg.language.as_deref() == Some(language.as_str())
            })
            .or_else(|| {
                manifest
                    .packages
                    .iter()
                    .find(|pkg| request_matches_package(request, pkg) && pkg.language.is_none())
            }));
    }
    // Languageless request: prefer the languageless manifest entry over any
    // language-tagged variant that happens to appear earlier in the list.
    Ok(manifest
        .packages
        .iter()
        .find(|pkg| request_matches_package(request, pkg) && pkg.language.is_none())
        .or_else(|| {
            manifest
                .packages
                .iter()
                .find(|pkg| request_matches_package(request, pkg))
        }))
}

fn request_matches_package(request: &PackageRequest, pkg: &vs_manifest::Package) -> bool {
    pkg.id.eq_ignore_ascii_case(&request.id)
        && match request.version.as_deref() {
            Some(version) => pkg.version.as_deref() == Some(version),
            None => true,
        }
}

fn is_metadata_dependency(dep_lower: &str) -> bool {
    dep_lower.starts_with("microsoft.vc.")
        && (dep_lower.contains(".res.")
            || dep_lower.contains(".resources")
            || dep_lower.contains(".props")
            || dep_lower.contains(".servicing"))
}

pub fn family_prefix_from_compiler_package(package_id: &str) -> Result<String> {
    let lower = package_id.to_lowercase();
    if !lower.starts_with("microsoft.vc.") || !lower.ends_with(COMPILER_PACKAGE_SUFFIX_LOWER) {
        bail!(
            "unsupported MSVC compiler package id '{package_id}'; expected Microsoft.VC.<family>{COMPILER_PACKAGE_SUFFIX}"
        );
    }
    let family = &package_id[..package_id.len() - COMPILER_PACKAGE_SUFFIX.len()];
    Ok(format!("{family}."))
}

fn starts_with_ignore_ascii_case(value: &str, prefix: &str) -> bool {
    value.len() >= prefix.len() && value[..prefix.len()].eq_ignore_ascii_case(prefix)
}

fn option_str<'a>(options: &'a toml::Table, key: &str) -> Result<Option<&'a str>> {
    match options.get(key) {
        None => Ok(None),
        Some(value) => value
            .as_str()
            .map(Some)
            .ok_or_else(|| anyhow!("msvc option '{key}' must be a string")),
    }
}

fn option_string_list(options: &toml::Table, key: &str, default: &[&str]) -> Result<Vec<String>> {
    let Some(value) = options.get(key) else {
        return Ok(default.iter().map(|value| value.to_string()).collect());
    };
    let values = value
        .as_array()
        .ok_or_else(|| anyhow!("msvc option '{key}' must be an array of strings"))?;
    let mut out = Vec::new();
    for value in values {
        let s = value
            .as_str()
            .ok_or_else(|| anyhow!("msvc option '{key}' must be an array of strings"))?;
        if s.is_empty() {
            bail!("msvc option '{key}' may not contain empty strings");
        }
        if !out.iter().any(|existing| existing == s) {
            out.push(s.to_string());
        }
    }
    Ok(out)
}

fn reject_removed_options(options: &toml::Table) -> Result<()> {
    for key in [
        "vs_channel",
        "hosts",
        "targets",
        "locales",
        "crt_libs",
        "runtimes",
        "features",
    ] {
        if options.contains_key(key) {
            bail!(
                "msvc option '{key}' is not supported; use profile='standard' with 'extras', profile='custom' with 'include', or profile='full'"
            );
        }
    }
    Ok(())
}

fn insert_string_array(options: &mut toml::Table, key: &str, values: &[String]) {
    options.insert(
        key.to_string(),
        toml::Value::Array(
            values
                .iter()
                .map(|value| toml::Value::String(value.clone()))
                .collect(),
        ),
    );
}

fn msvc_fingerprint(version: &str, selection: &MsvcSelection) -> String {
    let mut selection_key = String::new();
    selection_key.push_str(selection.vs.as_str());
    selection_key.push('\n');
    selection_key.push_str(selection.profile.as_str());
    selection_key.push('\n');

    let mut include = selection.include.clone();
    include.sort_by_key(|value| value.to_ascii_lowercase());
    for pattern in include {
        selection_key.push_str("include\t");
        selection_key.push_str(&pattern.to_ascii_lowercase());
        selection_key.push('\n');
    }

    let mut extras = selection.extras.clone();
    extras.sort_by_key(|value| value.to_ascii_lowercase());
    for pattern in extras {
        selection_key.push_str("extras\t");
        selection_key.push_str(&pattern.to_ascii_lowercase());
        selection_key.push('\n');
    }

    let selection_hash = xxh3_64(selection_key.as_bytes());
    sanitize_fingerprint(&format!(
        "msvc-{version}-{}-{selection_hash:016x}",
        selection.vs.as_str()
    ))
}

/// Extract a single MSVC payload (vsix or msi) into `dest`. VSIX = zip with
/// a `Contents/` prefix to strip; MSI = msiexec /a (Windows-only).
fn extract_payload(
    archive: &std::path::Path,
    filename: &str,
    dest: &std::path::Path,
) -> Result<()> {
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

fn resolved_options(
    options: &toml::Table,
    resolved_ver: &str,
    selection: &MsvcSelection,
) -> toml::Table {
    selection.resolved_options(options, resolved_ver)
}

#[cfg(test)]
#[path = "msvc_tests.rs"]
mod tests;
