//! Tests for `src/fs_util.rs` (split out so the production
//! file shows only the implementation).

use super::*;
use std::path::PathBuf;
use tempfile::TempDir;

#[test]
fn atomic_write_creates_file() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("sub").join("file.txt");
    atomic_write(&path, b"hello").unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
}

#[test]
fn atomic_write_overwrites() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("file.txt");
    atomic_write(&path, b"first").unwrap();
    atomic_write(&path, b"second").unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
}

#[test]
fn try_rename_succeeds_when_target_missing() {
    let tmp = TempDir::new().unwrap();
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    std::fs::write(&a, "x").unwrap();
    let renamed = try_rename(&a, &b).unwrap();
    assert!(renamed);
    assert!(!a.exists());
    assert!(b.exists());
}

#[test]
fn try_rename_returns_false_on_collision() {
    let tmp = TempDir::new().unwrap();
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    std::fs::write(b.join("existing"), "x").unwrap();
    let renamed = try_rename(&a, &b).unwrap();
    assert!(!renamed);
}

#[test]
fn mark_file_readonly_sets_attr() {
    let tmp = TempDir::new().unwrap();
    let f = tmp.path().join("a.txt");
    std::fs::write(&f, "x").unwrap();
    mark_file_readonly(&f).unwrap();
    let md = std::fs::metadata(&f).unwrap();
    assert!(md.permissions().readonly());
}

#[test]
fn remove_dir_all_writable_handles_readonly() {
    let tmp = TempDir::new().unwrap();
    let inner = tmp.path().join("inner");
    std::fs::create_dir_all(&inner).unwrap();
    let f = inner.join("a.txt");
    std::fs::write(&f, "x").unwrap();
    mark_file_readonly(&f).unwrap();
    remove_dir_all_writable(&inner).unwrap();
    assert!(!inner.exists());
}

#[test]
fn hard_link_creates_link() {
    let tmp = TempDir::new().unwrap();
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    std::fs::write(&a, "data").unwrap();
    hard_link(&a, &b).unwrap();
    assert_eq!(std::fs::read_to_string(&b).unwrap(), "data");
}

#[test]
fn slash_str_normalizes() {
    let p = PathBuf::from(r"C:\foo\bar");
    let s = slash_str(&p);
    assert!(s.contains('/') || !s.contains('\\'));
}

#[test]
fn dir_symlink_works() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.path().join("target");
    let link = tmp.path().join("link");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("inside.txt"), "hi").unwrap();
    dir_symlink(&target, &link).unwrap();
    // Confirm we can read through the link.
    let read = std::fs::read_to_string(link.join("inside.txt"));
    assert!(read.is_ok(), "could not read through link: {:?}", read);
}
