//! Mark of the Web: files unpacked from a downloaded archive carry the
//! archive's "from the internet" tag, as they do with WinRAR and 7-Zip.
//! Windows (NTFS stream) and macOS (quarantine attribute) only: other
//! systems have no such tag.

#![cfg(any(windows, target_os = "macos"))]
#![allow(clippy::unwrap_used, clippy::panic)]

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use common::*;

#[cfg(windows)]
const MARK: &str = "[ZoneTransfer]\r\nZoneId=3\r\nHostUrl=https://t.me/\r\n";
#[cfg(target_os = "macos")]
const MARK: &str = "0083;6738f1a2;Telegram;";

#[cfg(windows)]
fn set_mark(file: &Path, mark: &str) {
    fs::write(format!("{}:Zone.Identifier", file.display()), mark).unwrap();
}

#[cfg(windows)]
fn mark_of(file: &Path) -> Option<String> {
    fs::read_to_string(format!("{}:Zone.Identifier", file.display())).ok()
}

#[cfg(target_os = "macos")]
fn set_mark(file: &Path, mark: &str) {
    let status = Command::new("xattr")
        .args(["-w", "com.apple.quarantine", mark])
        .arg(file)
        .status()
        .unwrap();
    assert!(status.success());
}

#[cfg(target_os = "macos")]
fn mark_of(file: &Path) -> Option<String> {
    let out = Command::new("xattr")
        .args(["-p", "com.apple.quarantine"])
        .arg(file)
        .output()
        .unwrap();
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim_end().to_owned())
}

/// Unpacks every archive in `dir` like a double-click does, keeping them.
fn unpack_all(dir: &Path) {
    let output = Command::new(env!("CARGO_BIN_EXE_unpacker"))
        .arg("--keep")
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{out}");
}

/// Every regular file under `dir`, except the archives we put there.
fn unpacked(dir: &Path, archives: &[&str]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for entry in fs::read_dir(d).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                todo.push(path);
            } else if !archives
                .iter()
                .any(|a| path.file_name() == Path::new(a).file_name())
            {
                files.push(path);
            }
        }
    }
    assert!(!files.is_empty(), "nothing was unpacked");
    files
}

const RAR_VOLS: &[&str] = &[
    "rar/rar5-vols.part1.rar",
    "rar/rar5-vols.part2.rar",
    "rar/rar5-vols.part3.rar",
];
const SEVENZ_SPLIT: &[&str] = &["7z/archive.7z.001", "7z/archive.7z.002"];
const TAR_GZ: &[&str] = &["tar/show.tar.gz"];

#[test]
fn every_unpacked_file_carries_the_archive_tag() {
    // One per writer: UnRAR writes its own files, the others go through ours.
    for archives in [RAR_VOLS, SEVENZ_SPLIT, TAR_GZ] {
        let tmp = copy_fixtures(archives);
        for a in archives {
            set_mark(&tmp.path().join(Path::new(a).file_name().unwrap()), MARK);
        }
        unpack_all(tmp.path());
        for file in unpacked(tmp.path(), archives) {
            assert_eq!(mark_of(&file).as_deref(), Some(MARK), "{}", file.display());
        }
    }
}

#[test]
fn one_tagged_part_is_enough() {
    let tmp = copy_fixtures(RAR_VOLS);
    set_mark(&tmp.path().join("rar5-vols.part2.rar"), MARK);
    unpack_all(tmp.path());
    for file in unpacked(tmp.path(), RAR_VOLS) {
        assert_eq!(mark_of(&file).as_deref(), Some(MARK), "{}", file.display());
    }
}

#[test]
fn local_archives_stay_untagged() {
    let tmp = copy_fixtures(SEVENZ_SPLIT);
    unpack_all(tmp.path());
    for file in unpacked(tmp.path(), SEVENZ_SPLIT) {
        assert_eq!(mark_of(&file), None, "{}", file.display());
    }
}

#[cfg(windows)]
#[test]
fn the_strictest_zone_wins() {
    let tmp = copy_fixtures(SEVENZ_SPLIT);
    let trusted = "[ZoneTransfer]\r\nZoneId=2\r\n";
    let restricted = "[ZoneTransfer]\r\nZoneId=4\r\n";
    set_mark(&tmp.path().join("archive.7z.001"), trusted);
    set_mark(&tmp.path().join("archive.7z.002"), restricted);
    unpack_all(tmp.path());
    for file in unpacked(tmp.path(), SEVENZ_SPLIT) {
        assert_eq!(
            mark_of(&file).as_deref(),
            Some(restricted),
            "{}",
            file.display()
        );
    }
}
