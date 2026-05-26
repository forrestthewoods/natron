//! Tests for `src/config.rs` (split out so the production
//! file shows only the implementation).

use super::*;
use tempfile::TempDir;

fn write_config(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join(CONFIG_FILENAME);
    std::fs::write(&path, body).unwrap();
    path
}

fn minimal_entry(name: &str, deploy_dir: &str) -> String {
    format!(
        r#"
[[toolchain]]
name = "{name}"
deploy_dir = "{deploy_dir}"
provider = "url"
[toolchain.options]
url = "https://example.com/{name}.zip"
"#
    )
}

#[test]
fn parses_minimal_config() {
    let tmp = TempDir::new().unwrap();
    let body = format!("[settings]\n{}", minimal_entry("foo", "foo"));
    let path = write_config(tmp.path(), &body);
    let cfg = Config::load(&path).unwrap();
    assert_eq!(cfg.toolchains.len(), 1);
    assert_eq!(cfg.toolchains[0].name, "foo");
    assert_eq!(cfg.settings.deploy_mode, DeployMode::Symlink);
    assert_eq!(cfg.config_dir, tmp.path().canonicalize().unwrap_or(tmp.path().to_path_buf()).parent().map(|_| tmp.path().to_path_buf()).unwrap_or_else(|| tmp.path().to_path_buf()));
}

#[test]
fn config_dir_is_set() {
    let tmp = TempDir::new().unwrap();
    let body = minimal_entry("foo", "foo");
    let path = write_config(tmp.path(), &body);
    let cfg = Config::load(&path).unwrap();
    assert_eq!(cfg.config_dir, tmp.path());
}

#[test]
fn rejects_duplicate_name() {
    let tmp = TempDir::new().unwrap();
    let body = format!(
        "{}{}",
        minimal_entry("foo", "foo"),
        minimal_entry("foo", "bar")
    );
    let path = write_config(tmp.path(), &body);
    let err = Config::load(&path).unwrap_err();
    assert!(err.to_string().contains("duplicate toolchain name"));
}

#[test]
fn rejects_duplicate_deploy_dir() {
    let tmp = TempDir::new().unwrap();
    let body = format!(
        "{}{}",
        minimal_entry("foo", "shared"),
        minimal_entry("bar", "shared")
    );
    let path = write_config(tmp.path(), &body);
    let err = Config::load(&path).unwrap_err();
    assert!(err.to_string().contains("duplicate deploy_dir"));
}

#[test]
fn rejects_malformed_toml() {
    let tmp = TempDir::new().unwrap();
    let path = write_config(tmp.path(), "[[toolchain\nname = \"x\"\n");
    let err = Config::load(&path).unwrap_err();
    assert!(err.to_string().contains("parsing"));
}

#[test]
fn rejects_invalid_provider_id() {
    let tmp = TempDir::new().unwrap();
    let body = r#"
[[toolchain]]
name = "x"
deploy_dir = "x"
provider = "GitHub-Capital"
[toolchain.options]
"#;
    let path = write_config(tmp.path(), body);
    let err = Config::load(&path).unwrap_err();
    assert!(err.to_string().contains("invalid provider id"));
}

#[test]
fn rejects_empty_name() {
    let tmp = TempDir::new().unwrap();
    let body = r#"
[[toolchain]]
name = ""
deploy_dir = "x"
provider = "url"
[toolchain.options]
"#;
    let path = write_config(tmp.path(), body);
    let err = Config::load(&path).unwrap_err();
    assert!(err.to_string().contains("empty `name`"));
}

#[test]
fn discovers_config_upward() {
    let tmp = TempDir::new().unwrap();
    let body = minimal_entry("foo", "foo");
    write_config(tmp.path(), &body);
    let nested = tmp.path().join("a").join("b").join("c");
    std::fs::create_dir_all(&nested).unwrap();
    let cfg = Config::load(&nested).unwrap();
    assert_eq!(cfg.toolchains[0].name, "foo");
}

#[test]
fn load_errors_when_no_config_found_upward() {
    let tmp = TempDir::new().unwrap();
    let err = Config::load(tmp.path()).unwrap_err();
    assert!(err.to_string().contains("no `natron.toml`"));
}

#[test]
fn errors_when_config_path_does_not_exist() {
    let err = Config::load(Path::new("/this/definitely/does/not/exist")).unwrap_err();
    assert!(err.to_string().contains("does not exist"));
}

#[test]
fn expands_tilde_in_cache_dir() {
    let tmp = TempDir::new().unwrap();
    let body = format!(
        r#"
[settings]
cache_dir = "~/some/where"
{}
"#,
        minimal_entry("foo", "foo")
    );
    let path = write_config(tmp.path(), &body);
    let cfg = Config::load(&path).unwrap();
    let cache = cfg.settings.cache_dir.unwrap();
    let cache_str = cache.to_string_lossy();
    assert!(!cache_str.starts_with('~'));
    assert!(cache_str.ends_with("some/where") || cache_str.ends_with("some\\where"));
}

#[test]
fn expands_dollar_brace_var() {
    let env_key = "NATRON_TEST_EXPAND_VAR_42";
    // SAFETY: tests run in parallel but each uses a unique env var name.
    unsafe {
        std::env::set_var(env_key, "/tmp/expanded");
    }
    let expanded = expand_user_str("${NATRON_TEST_EXPAND_VAR_42}/sub").unwrap();
    assert_eq!(expanded, "/tmp/expanded/sub");
    unsafe {
        std::env::remove_var(env_key);
    }
}

#[test]
fn errors_on_unset_env_var() {
    let err = expand_user_str("${THIS_VAR_IS_NOT_SET_84938}/x").unwrap_err();
    assert!(err.to_string().contains("not set"));
}

#[test]
fn deploy_mode_default_is_symlink() {
    assert_eq!(DeployMode::default(), DeployMode::Symlink);
}

#[test]
fn archive_kind_parse_and_infer() {
    assert_eq!(ArchiveKind::parse("zip").unwrap(), ArchiveKind::Zip);
    assert_eq!(ArchiveKind::parse("tar.xz").unwrap(), ArchiveKind::TarXz);
    assert_eq!(ArchiveKind::parse("tar.gz").unwrap(), ArchiveKind::TarGz);
    assert!(ArchiveKind::parse("rar").is_err());

    assert_eq!(
        ArchiveKind::infer_from_filename("foo.tar.xz"),
        Some(ArchiveKind::TarXz)
    );
    assert_eq!(
        ArchiveKind::infer_from_filename("foo.zip"),
        Some(ArchiveKind::Zip)
    );
    assert_eq!(
        ArchiveKind::infer_from_filename("foo.tgz"),
        Some(ArchiveKind::TarGz)
    );
    assert_eq!(ArchiveKind::infer_from_filename("foo.bin"), None);
}

#[test]
fn effective_mode_uses_entry_override() {
    let cfg = Config {
        settings: Settings::default(),
        toolchains: vec![],
        config_dir: PathBuf::new(),
    };
    let entry = ToolchainEntry {
        name: "x".into(),
        deploy_dir: "x".into(),
        provider: "url".into(),
        deploy_mode: Some(DeployMode::Copy),
        options: toml::Table::new(),
    };
    assert_eq!(cfg.effective_mode(&entry), DeployMode::Copy);

    let entry2 = ToolchainEntry { deploy_mode: None, ..entry };
    assert_eq!(cfg.effective_mode(&entry2), DeployMode::Symlink);
}
