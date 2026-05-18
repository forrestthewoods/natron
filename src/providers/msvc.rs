//! `msvc` provider: downloads the MSVC compiler + CRT packages from a
//! Visual Studio channel manifest. Windows-specific install (uses
//! `msiexec.exe` / VSIX zip extraction).
//!
//! NOTE: this provider produces ONLY the compiler + CRT tree. The Windows
//! SDK is a separate provider (`windows_sdk`). The two are logically
//! independent.

use anyhow::{anyhow, bail, Context, Result};
use std::collections::BTreeSet;

use super::vs_manifest::{self, MsvcCandidate, VsManifest};
use super::{InstallCtx, Installed, Provider};
use crate::cache::sanitize_fingerprint;
use crate::extract;

pub const ID: &str = "msvc";

const HOST: &str = "x64";
const TARGET: &str = "x64";
const DEFAULT_LOCALE: &str = "en-US";

/// Archive mirror for old MSVC manifests. This is an implementation detail
/// for pinned `msvc_version`: Microsoft live channel manifests are moving
/// targets and may stop listing older toolsets.
const ARCHIVE_MANIFEST_URL_TEMPLATE: &str =
    "https://raw.githubusercontent.com/roblabla/msvc-manifest-history/release-{channel}/manifest.json";

pub struct MsvcProvider {
    channel_url_template: String,
    archive_manifest_url_template: String,
}

#[derive(Debug)]
struct ResolvedMsvcToolset {
    manifest: VsManifest,
    package_version: String,
    package_id_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MsvcSelection {
    profile: MsvcProfile,
    hosts: Vec<String>,
    targets: Vec<String>,
    locales: LocaleSelection,
    crt_libs: Vec<String>,
    runtimes: Vec<String>,
    features: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MsvcProfile {
    Standard,
    Custom,
    Full,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LocaleSelection {
    All,
    Some(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct PackageRequest {
    id: String,
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
        let vs_channel = options
            .get("vs_channel")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("`msvc` provider requires options.vs_channel (string)"))?;
        let pinned_version = options.get("msvc_version").and_then(|v| v.as_str());
        let selection = MsvcSelection::from_options(options)?;

        // Fast-path when version is pinned: deterministic fingerprint, no
        // network needed.
        if let Some(ver) = pinned_version {
            let fp = msvc_fingerprint(ver, vs_channel, &selection);
            if ctx.cache().install_present(&fp) {
                return Ok(Installed {
                    fingerprint: fp,
                    display: format!("msvc {ver} (vs{vs_channel})"),
                    options: resolved_options(options, ver, &selection),
                    freshly_extracted: false,
                });
            }
        }

        let resolved = self.resolve_toolset(vs_channel, pinned_version, ctx)?;

        let fp = msvc_fingerprint(&resolved.package_version, vs_channel, &selection);
        if ctx.cache().install_present(&fp) {
            return Ok(Installed {
                fingerprint: fp,
                display: format!("msvc {} (vs{vs_channel})", resolved.package_version),
                options: resolved_options(options, &resolved.package_version, &selection),
                freshly_extracted: false,
            });
        }

        let packages = select_msvc_packages(&resolved, &selection)?;
        let payloads = collect_payloads(&resolved.manifest, &packages)?;

        // Download + extract into staging.
        let staging_raw = ctx.staging_dir()?;
        for (url, sha, filename) in &payloads {
            let archive_path = ctx.download(url, sha.as_deref())?;
            extract_payload(&archive_path, filename, &staging_raw)?;
        }

        Ok(Installed {
            fingerprint: fp,
            display: format!("msvc {} (vs{vs_channel})", resolved.package_version),
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
            return Ok(resolved_toolset(manifest, candidate));
        };

        let mut notes = Vec::new();
        match live {
            Ok(manifest) => {
                if let Some(candidate) = select_pinned_candidate(&manifest, requested) {
                    return Ok(resolved_toolset(manifest, candidate));
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
                    return Ok(resolved_toolset(manifest, candidate));
                }
                notes.push(format!(
                    "archived manifest did not contain {requested}; available versions: {}",
                    format_available_versions(&manifest)
                ));
            }
            Err(err) => notes.push(format!("archived manifest failed: {err:#}")),
        }

        bail!(
            "could not resolve pinned msvc_version='{requested}' for vs_channel='{vs_channel}'; {}",
            notes.join("; ")
        )
    }

    fn fetch_live_manifest(&self, vs_channel: &str, ctx: &InstallCtx) -> Result<VsManifest> {
        vs_manifest::fetch_vs_manifest(&self.channel_url_template, vs_channel, ctx).with_context(
            || format!("resolving Microsoft live VS manifest for channel {vs_channel}"),
        )
    }

    fn fetch_archive_manifest(&self, vs_channel: &str, ctx: &InstallCtx) -> Result<VsManifest> {
        let url = self
            .archive_manifest_url_template
            .replace("{channel}", vs_channel);
        let path = ctx
            .download(&url, None)
            .with_context(|| format!("fetching archived VS manifest from {url}"))?;
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading archived VS manifest {}", path.display()))?;
        serde_json::from_str(&text)
            .with_context(|| format!("parsing archived VS manifest {}", path.display()))
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

fn resolved_toolset(manifest: VsManifest, candidate: MsvcCandidate) -> ResolvedMsvcToolset {
    ResolvedMsvcToolset {
        manifest,
        package_version: candidate.package_version,
        package_id_version: candidate.package_id_version,
    }
}

impl MsvcSelection {
    fn from_options(options: &toml::Table) -> Result<Self> {
        let profile = match option_str(options, "profile")?.unwrap_or("standard") {
            "standard" => MsvcProfile::Standard,
            "custom" => MsvcProfile::Custom,
            "full" => MsvcProfile::Full,
            other => {
                bail!("invalid msvc profile '{other}'; valid profiles: standard, custom, full")
            }
        };

        let hosts = option_string_list(options, "hosts", &["x64"])?;
        let targets = option_string_list(options, "targets", &["x64"])?;
        if hosts.is_empty() {
            bail!("msvc option 'hosts' must contain at least one host architecture");
        }
        if targets.is_empty() {
            bail!("msvc option 'targets' must contain at least one target architecture");
        }
        validate_values("hosts", &hosts, &["x64", "x86", "arm64"])?;
        validate_values("targets", &targets, &["x64", "x86", "arm64"])?;

        let locale_values = option_string_list(options, "locales", &[DEFAULT_LOCALE])?;
        if locale_values.is_empty() {
            bail!("msvc option 'locales' must contain at least one locale or \"all\"");
        }
        let locales = if locale_values.iter().any(|value| value == "all") {
            if locale_values.len() != 1 {
                bail!("msvc locales may be [\"all\"] or a list of concrete locales, not both");
            }
            LocaleSelection::All
        } else {
            LocaleSelection::Some(locale_values)
        };

        let (default_crt_libs, default_runtimes, default_features): (&[&str], &[&str], &[&str]) =
            match profile {
                MsvcProfile::Standard => (&["desktop", "store"], &["crt"], &[]),
                MsvcProfile::Custom => (&[], &[], &[]),
                MsvcProfile::Full => (&[], &[], &[]),
            };
        let crt_libs = option_string_list(options, "crt_libs", default_crt_libs)?;
        let runtimes = option_string_list(options, "runtimes", default_runtimes)?;
        let features = option_string_list(options, "features", default_features)?;

        validate_values(
            "crt_libs",
            &crt_libs,
            &["desktop", "store", "onecore", "spectre", "debug"],
        )?;
        validate_values(
            "runtimes",
            &runtimes,
            &["crt", "crt_spectre", "mfc", "mfc_spectre"],
        )?;
        validate_values(
            "features",
            &features,
            &[
                "atl",
                "atl_spectre",
                "mfc",
                "mfc_spectre",
                "mfc_mbcs",
                "asan",
                "pgo",
                "cli",
                "code_analysis",
                "dia_sdk",
                "source",
            ],
        )?;

        Ok(Self {
            profile,
            hosts,
            targets,
            locales,
            crt_libs,
            runtimes,
            features,
        })
    }

    fn normalized_key(&self) -> String {
        if self.profile == MsvcProfile::Full {
            return "profile=full".to_string();
        }
        format!(
            "profile={};hosts={};targets={};locales={};crt_libs={};runtimes={};features={}",
            self.profile.as_str(),
            self.hosts.join(","),
            self.targets.join(","),
            self.locales.as_key(),
            self.crt_libs.join(","),
            self.runtimes.join(","),
            self.features.join(",")
        )
    }

    fn resolved_options(&self, options: &toml::Table, resolved_ver: &str) -> toml::Table {
        let mut o = options.clone();
        o.insert(
            "msvc_version".into(),
            toml::Value::String(resolved_ver.to_string()),
        );
        o.insert(
            "profile".into(),
            toml::Value::String(self.profile.as_str().to_string()),
        );
        if self.profile == MsvcProfile::Full {
            return o;
        }
        insert_string_array(&mut o, "hosts", &self.hosts);
        insert_string_array(&mut o, "targets", &self.targets);
        match &self.locales {
            LocaleSelection::All => insert_string_array(&mut o, "locales", &["all".to_string()]),
            LocaleSelection::Some(locales) => insert_string_array(&mut o, "locales", locales),
        }
        insert_string_array(&mut o, "crt_libs", &self.crt_libs);
        insert_string_array(&mut o, "runtimes", &self.runtimes);
        insert_string_array(&mut o, "features", &self.features);
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

impl LocaleSelection {
    fn as_key(&self) -> String {
        match self {
            LocaleSelection::All => "all".to_string(),
            LocaleSelection::Some(locales) => locales.join(","),
        }
    }

    fn package_languages(
        &self,
        manifest: &VsManifest,
        package_id: &str,
    ) -> Result<Vec<Option<String>>> {
        match self {
            LocaleSelection::All => {
                let mut languages = manifest
                    .packages
                    .iter()
                    .filter(|pkg| pkg.id.eq_ignore_ascii_case(package_id))
                    .map(|pkg| pkg.language.clone())
                    .collect::<Vec<_>>();
                languages.sort();
                languages.dedup();
                if languages.is_empty() {
                    bail!("package not found in manifest: {package_id}");
                }
                Ok(languages)
            }
            LocaleSelection::Some(locales) => {
                let mut languages = Vec::new();
                for locale in locales {
                    if manifest.packages.iter().any(|pkg| {
                        pkg.id.eq_ignore_ascii_case(package_id)
                            && pkg.language.as_deref() == Some(locale.as_str())
                    }) {
                        languages.push(Some(locale.clone()));
                    } else if manifest.packages.iter().any(|pkg| {
                        pkg.id.eq_ignore_ascii_case(package_id) && pkg.language.is_none()
                    }) {
                        languages.push(None);
                    } else {
                        languages.push(Some(locale.clone()));
                    }
                }
                Ok(languages)
            }
        }
    }
}

fn select_msvc_packages(
    resolved: &ResolvedMsvcToolset,
    selection: &MsvcSelection,
) -> Result<Vec<PackageRequest>> {
    if selection.profile == MsvcProfile::Full {
        return select_full_family_packages(resolved);
    }

    let mut selected = BTreeSet::new();
    add_standard_packages(&mut selected, resolved, selection)?;
    add_custom_packages(&mut selected, resolved, selection)?;
    include_declared_metadata_dependencies(&mut selected, resolved, selection)?;
    Ok(selected.into_iter().collect())
}

fn select_full_family_packages(resolved: &ResolvedMsvcToolset) -> Result<Vec<PackageRequest>> {
    let prefix = format!("Microsoft.VC.{}.", resolved.package_id_version).to_lowercase();
    let selected: BTreeSet<_> = resolved
        .manifest
        .packages
        .iter()
        .filter(|pkg| pkg.id.to_lowercase().starts_with(&prefix))
        .map(|pkg| PackageRequest {
            id: pkg.id.clone(),
            language: pkg.language.clone(),
        })
        .collect();
    if selected.is_empty() {
        bail!(
            "no MSVC packages found for package family {}",
            resolved.package_id_version
        );
    }
    Ok(selected.into_iter().collect())
}

fn add_standard_packages(
    selected: &mut BTreeSet<PackageRequest>,
    resolved: &ResolvedMsvcToolset,
    selection: &MsvcSelection,
) -> Result<()> {
    let ver = &resolved.package_id_version;
    add_package(selected, format!("Microsoft.VC.{ver}.CRT.Headers.base"));
    for host in &selection.hosts {
        for target in &selection.targets {
            add_package(
                selected,
                format!(
                    "Microsoft.VC.{ver}.Tools.Host{}.Target{}.base",
                    tool_arch(host),
                    tool_arch(target)
                ),
            );
        }
    }
    for target in &selection.targets {
        let arch = crt_arch(target);
        for crt_lib in &selection.crt_libs {
            match crt_lib.as_str() {
                "desktop" => add_package(
                    selected,
                    format!("Microsoft.VC.{ver}.CRT.{arch}.Desktop.base"),
                ),
                "store" => add_package(
                    selected,
                    format!("Microsoft.VC.{ver}.CRT.{arch}.Store.base"),
                ),
                "onecore" => add_package(
                    selected,
                    format!("Microsoft.VC.{ver}.CRT.{arch}.OneCore.Desktop.base"),
                ),
                "spectre" => add_package(
                    selected,
                    format!("Microsoft.VC.{ver}.CRT.{arch}.Desktop.spectre.base"),
                ),
                "debug" => add_package(
                    selected,
                    format!("Microsoft.VC.{ver}.CRT.{arch}.Desktop.debug.base"),
                ),
                _ => unreachable!("validated crt_lib"),
            }
        }
        for runtime in &selection.runtimes {
            let redist_arch = redist_arch(target);
            match runtime.as_str() {
                "crt" => add_package(
                    selected,
                    format!("Microsoft.VC.{ver}.CRT.Redist.{redist_arch}.base"),
                ),
                "crt_spectre" => add_package(
                    selected,
                    format!("Microsoft.VC.{ver}.CRT.Redist.{redist_arch}.spectre.base"),
                ),
                "mfc" => add_package(
                    selected,
                    format!("Microsoft.VC.{ver}.MFC.Redist.{redist_arch}.base"),
                ),
                "mfc_spectre" => add_package(
                    selected,
                    format!("Microsoft.VC.{ver}.MFC.Redist.{redist_arch}.Spectre.base"),
                ),
                _ => unreachable!("validated runtime"),
            }
        }
    }
    Ok(())
}

fn add_custom_packages(
    selected: &mut BTreeSet<PackageRequest>,
    resolved: &ResolvedMsvcToolset,
    selection: &MsvcSelection,
) -> Result<()> {
    let ver = &resolved.package_id_version;
    for feature in &selection.features {
        match feature.as_str() {
            "atl" => {
                add_package(selected, format!("Microsoft.VC.{ver}.ATL.Headers.base"));
                for target in &selection.targets {
                    add_package(
                        selected,
                        format!("Microsoft.VC.{ver}.ATL.{}.base", library_arch(target)),
                    );
                }
            }
            "atl_spectre" => {
                add_package(selected, format!("Microsoft.VC.{ver}.ATL.Headers.base"));
                for target in &selection.targets {
                    add_package(
                        selected,
                        format!(
                            "Microsoft.VC.{ver}.ATL.{}.Spectre.base",
                            library_arch(target)
                        ),
                    );
                }
            }
            "mfc" => {
                add_package(selected, format!("Microsoft.VC.{ver}.MFC.Headers.base"));
                for target in &selection.targets {
                    add_package(
                        selected,
                        format!("Microsoft.VC.{ver}.MFC.{}.base", library_arch(target)),
                    );
                }
            }
            "mfc_spectre" => {
                add_package(selected, format!("Microsoft.VC.{ver}.MFC.Headers.base"));
                for target in &selection.targets {
                    add_package(
                        selected,
                        format!(
                            "Microsoft.VC.{ver}.MFC.{}.Spectre.base",
                            library_arch(target)
                        ),
                    );
                }
            }
            "mfc_mbcs" => {
                add_package(selected, format!("Microsoft.VC.{ver}.MFC.MBCS.base"));
                for target in &selection.targets {
                    add_package(
                        selected,
                        format!("Microsoft.VC.{ver}.MFC.MBCS.{}.base", library_arch(target)),
                    );
                }
            }
            "asan" => {
                add_package(selected, format!("Microsoft.VC.{ver}.ASAN.Headers.base"));
                for target in &selection.targets {
                    add_package(
                        selected,
                        format!("Microsoft.VC.{ver}.ASAN.{}.base", library_arch(target)),
                    );
                }
            }
            "pgo" => {
                add_package(selected, format!("Microsoft.VC.{ver}.PGO.Headers.base"));
                for target in &selection.targets {
                    add_package(
                        selected,
                        format!("Microsoft.VC.{ver}.PGO.{}.base", library_arch(target)),
                    );
                }
                for host in &selection.hosts {
                    for target in &selection.targets {
                        add_package(
                            selected,
                            format!(
                                "Microsoft.VC.{ver}.Premium.Tools.Host{}.Target{}.base",
                                tool_arch(host),
                                tool_arch(target)
                            ),
                        );
                    }
                }
            }
            "cli" => {
                add_package(selected, format!("Microsoft.VC.{ver}.CLI.Source.base"));
                for target in &selection.targets {
                    add_package(
                        selected,
                        format!("Microsoft.VC.{ver}.CLI.{}.base", library_arch(target)),
                    );
                }
            }
            "code_analysis" => {
                add_package(selected, format!("Microsoft.VC.{ver}.CA.Rulesets.base"));
                for host in &selection.hosts {
                    for target in &selection.targets {
                        add_package(
                            selected,
                            format!(
                                "Microsoft.VC.{ver}.CA.Ext.Host{}.Target{}.base",
                                tool_arch(host),
                                tool_arch(target)
                            ),
                        );
                    }
                }
            }
            "dia_sdk" => add_package(selected, format!("Microsoft.VC.{ver}.DIA.SDK")),
            "source" => {
                add_package(selected, format!("Microsoft.VC.{ver}.CRT.Source.base"));
                add_package(selected, format!("Microsoft.VC.{ver}.ATL.Source.base"));
                add_package(selected, format!("Microsoft.VC.{ver}.MFC.Source.base"));
                add_package(selected, format!("Microsoft.VC.{ver}.CLI.Source.base"));
            }
            _ => unreachable!("validated feature"),
        }
    }
    Ok(())
}

fn include_declared_metadata_dependencies(
    selected: &mut BTreeSet<PackageRequest>,
    resolved: &ResolvedMsvcToolset,
    selection: &MsvcSelection,
) -> Result<()> {
    loop {
        let current: Vec<_> = selected.iter().cloned().collect();
        let mut changed = false;
        for request in current {
            let Some(pkg) = find_requested_package(&resolved.manifest, &request)? else {
                bail!("package not found in manifest: {}", request.id);
            };
            let mut has_resource_dependency = false;
            for dep_id in pkg.dependencies.keys() {
                let dep_lower = dep_id.to_lowercase();
                if is_resource_dependency(&dep_lower) {
                    has_resource_dependency = true;
                    for language in selection
                        .locales
                        .package_languages(&resolved.manifest, dep_id)?
                    {
                        changed |= selected.insert(PackageRequest {
                            id: dep_id.clone(),
                            language,
                        });
                    }
                } else if is_metadata_dependency(&dep_lower) {
                    changed |= selected.insert(PackageRequest {
                        id: dep_id.clone(),
                        language: None,
                    });
                }
            }
            if is_compiler_tools_package(&request.id) && !has_resource_dependency {
                bail!(
                    "MSVC package {} has no resource package dependency",
                    request.id
                );
            }
        }
        if !changed {
            break;
        }
    }
    Ok(())
}

fn add_package(selected: &mut BTreeSet<PackageRequest>, id: String) {
    selected.insert(PackageRequest { id, language: None });
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
                pkg.id.eq_ignore_ascii_case(&request.id)
                    && pkg.language.as_deref() == Some(language.as_str())
            })
            .or_else(|| {
                manifest
                    .packages
                    .iter()
                    .find(|pkg| pkg.id.eq_ignore_ascii_case(&request.id) && pkg.language.is_none())
            }));
    }
    Ok(manifest.find_package(&request.id))
}

fn is_resource_dependency(dep_lower: &str) -> bool {
    dep_lower.starts_with("microsoft.vc.")
        && (dep_lower.contains(".res.") || dep_lower.contains(".resources"))
}

fn is_compiler_tools_package(package_id: &str) -> bool {
    let lower = package_id.to_lowercase();
    lower.starts_with("microsoft.vc.")
        && lower.contains(".tools.host")
        && lower.ends_with(".base")
        && !lower.contains(".premium.tools.")
        && !lower.contains(".res.")
}

fn is_metadata_dependency(dep_lower: &str) -> bool {
    dep_lower.starts_with("microsoft.vc.")
        && (dep_lower.contains(".props") || dep_lower.contains(".servicing"))
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

fn validate_values(key: &str, values: &[String], valid: &[&str]) -> Result<()> {
    for value in values {
        if !valid.contains(&value.as_str()) {
            bail!(
                "invalid msvc {key} value '{value}'; valid values: {}",
                valid.join(", ")
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

fn tool_arch(arch: &str) -> &'static str {
    match arch {
        "x64" => "X64",
        "x86" => "X86",
        "arm64" => "ARM64",
        _ => unreachable!("validated architecture"),
    }
}

fn crt_arch(arch: &str) -> &'static str {
    match arch {
        "x64" => "x64",
        "x86" => "x86",
        "arm64" => "ARM64",
        _ => unreachable!("validated architecture"),
    }
}

fn library_arch(arch: &str) -> &'static str {
    match arch {
        "x64" => "X64",
        "x86" => "X86",
        "arm64" => "ARM64",
        _ => unreachable!("validated architecture"),
    }
}

fn redist_arch(arch: &str) -> &'static str {
    match arch {
        "x64" => "X64",
        "x86" => "X86",
        "arm64" => "ARM64",
        _ => unreachable!("validated architecture"),
    }
}

fn msvc_fingerprint(version: &str, vs_channel: &str, selection: &MsvcSelection) -> String {
    sanitize_fingerprint(&format!(
        "msvc-{version}-{vs_channel}-{}",
        selection.normalized_key()
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
