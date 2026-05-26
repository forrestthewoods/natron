//! Option-parsing helpers shared by the `msvc` and `windows_sdk` providers.
//! Both expose the same `base_install` / `extras` shape and the same
//! required/optional string + string-list accessors over a `toml::Table`.

use anyhow::{Result, anyhow, bail};

/// `base_install` selector: the starting package/MSI set an install builds on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BaseInstall {
    None,
    Default,
    Full,
}

impl BaseInstall {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Default => "default",
            Self::Full => "full",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self> {
        match value {
            "none" => Ok(Self::None),
            "default" => Ok(Self::Default),
            "full" => Ok(Self::Full),
            other => bail!("invalid base_install '{other}'; valid: none, default, full"),
        }
    }
}

/// Required string option. `provider` names the calling provider for errors.
pub(crate) fn required_str<'a>(
    options: &'a toml::Table,
    key: &str,
    provider: &str,
) -> Result<&'a str> {
    options
        .get(key)
        .ok_or_else(|| anyhow!("`{provider}` provider requires options.{key}"))?
        .as_str()
        .ok_or_else(|| anyhow!("`{provider}` option '{key}' must be a string"))
}

/// Optional string option.
pub(crate) fn optional_str<'a>(
    options: &'a toml::Table,
    key: &str,
    provider: &str,
) -> Result<Option<&'a str>> {
    match options.get(key) {
        None => Ok(None),
        Some(v) => v
            .as_str()
            .map(Some)
            .ok_or_else(|| anyhow!("`{provider}` option '{key}' must be a string")),
    }
}

/// Optional list-of-strings option, de-duplicated in first-seen order.
/// Empty entries are rejected.
pub(crate) fn optional_string_list(
    options: &toml::Table,
    key: &str,
    provider: &str,
) -> Result<Vec<String>> {
    let Some(v) = options.get(key) else {
        return Ok(Vec::new());
    };
    let arr = v
        .as_array()
        .ok_or_else(|| anyhow!("`{provider}` option '{key}' must be an array of strings"))?;
    let mut out = Vec::new();
    for item in arr {
        let s = item
            .as_str()
            .ok_or_else(|| anyhow!("`{provider}` option '{key}' entries must be strings"))?;
        if s.is_empty() {
            bail!("`{provider}` option '{key}' entries may not be empty");
        }
        if !out.iter().any(|x: &String| x == s) {
            out.push(s.to_string());
        }
    }
    Ok(out)
}
