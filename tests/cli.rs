//! The program as users run it: double-click (no files), and what happens to
//! the archive files afterwards.

#![allow(clippy::unwrap_used, clippy::panic)]

mod common;

use std::fs;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use common::*;

const RAR_VOLS: &[&str] = &[
    "rar/rar5-vols.part1.rar",
    "rar/rar5-vols.part2.rar",
    "rar/rar5-vols.part3.rar",
];
const SEVENZ_SPLIT: &[&str] = &["7z/archive.7z.001", "7z/archive.7z.002"];

/// Runs the program in `dir` with `args`, with no console to answer on.
fn run_in(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_unpacker"))
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn exists(dir: &Path, names: &[&str]) -> Vec<bool> {
    names
        .iter()
        .map(|n| dir.join(Path::new(n).file_name().unwrap()).exists())
        .collect()
}

#[test]
fn no_files_unpacks_every_archive_in_the_folder() {
    let all: Vec<&str> = RAR_VOLS.iter().chain(SEVENZ_SPLIT).copied().collect();
    let tmp = copy_fixtures(&all);
    fs::write(tmp.path().join("movie.mkv"), "not an archive").unwrap();

    let output = run_in(tmp.path(), &["--keep"]);
    let out = stdout(&output);
    assert!(output.status.success(), "{out}");
    assert!(out.contains("Found 2 archive(s)"), "{out}");
    assert_eq!(out.matches("✓ Extracted").count(), 2, "{out}");
    assert!(tmp.path().join("rar5-vols/bigfile.txt").exists(), "{out}");
    assert!(tmp.path().join("archive").is_dir(), "{out}");
    // --keep: everything is still there.
    assert!(exists(tmp.path(), &all).iter().all(|&e| e));
}

#[test]
fn no_archives_says_how_to_use_it() {
    let tmp = tempfile::TempDir::new().unwrap();
    fs::write(tmp.path().join("movie.mkv"), "x").unwrap();
    let output = run_in(tmp.path(), &[]);
    assert!(output.status.success());
    assert!(stdout(&output).contains("No archives found"));
}

#[test]
fn delete_removes_all_parts_permanently() {
    let tmp = copy_fixtures(RAR_VOLS);
    let output = run_in(tmp.path(), &["--delete", "rar5-vols.part2.rar"]);
    let out = stdout(&output);
    assert!(output.status.success(), "{out}");
    assert!(out.contains("Deleted the 3 archive files"), "{out}");
    assert_eq!(exists(tmp.path(), RAR_VOLS), [false; 3]);
    assert!(tmp.path().join("rar5-vols/bigfile.txt").exists());
}

#[test]
fn archives_are_kept_when_a_file_was_skipped() {
    let tmp = copy_fixtures(RAR_VOLS);
    // A file of ours is in the way, so not everything gets unpacked.
    fs::create_dir(tmp.path().join("rar5-vols")).unwrap();
    fs::write(tmp.path().join("rar5-vols/smallfile.txt"), "mine").unwrap();

    let output = run_in(tmp.path(), &["--delete", "rar5-vols.part1.rar"]);
    let out = stdout(&output);
    assert!(output.status.success(), "{out}");
    assert!(out.contains("archive files are kept"), "{out}");
    assert_eq!(exists(tmp.path(), RAR_VOLS), [true; 3]);
    assert_eq!(read(tmp.path().join("rar5-vols/smallfile.txt")), "mine");
}

#[test]
fn without_a_console_nobody_is_asked_and_archives_stay() {
    let tmp = copy_fixtures(SEVENZ_SPLIT);
    let output = run_in(tmp.path(), &["archive.7z.001"]);
    let out = stdout(&output);
    assert!(output.status.success(), "{out}");
    assert!(
        !out.contains("Delete"),
        "nobody to ask, so no question:\n{out}"
    );
    assert_eq!(exists(tmp.path(), SEVENZ_SPLIT), [true; 2]);
}

// ---------------------------------------------------------------------------
// Honest output and argument checking
// ---------------------------------------------------------------------------

#[test]
fn nothing_extracted_is_not_reported_as_extracted() {
    // A single-file archive whose file already sits next to it (here: the
    // user's own, different file).
    let tmp = copy_fixtures(&["7z/umlaut-non_solid.7z"]);
    fs::write(tmp.path().join("täst.txt"), "MINE").unwrap();

    let output = run_in(tmp.path(), &["--delete", "umlaut-non_solid.7z"]);
    let out = stdout(&output);
    assert!(output.status.success(), "{out}");
    assert!(!out.contains("✓ Extracted"), "claims success:\n{out}");
    assert!(out.contains("Nothing extracted: 1 file skipped"), "{out}");
    assert_eq!(read(tmp.path().join("täst.txt")), "MINE");
    assert!(
        tmp.path().join("umlaut-non_solid.7z").exists(),
        "archive kept"
    );
}

#[test]
fn unknown_option_stops_before_doing_anything() {
    let tmp = copy_fixtures(RAR_VOLS);
    // A typo for --delete: must not run as if it weren't there.
    let output = run_in(tmp.path(), &["rar5-vols.part1.rar", "--delet"]);
    let err = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        err.contains("unknown option --delet") && err.contains("Usage:"),
        "{err}"
    );
    assert!(!tmp.path().join("rar5-vols").exists(), "nothing unpacked");
}

#[test]
fn help_and_version() {
    let tmp = tempfile::TempDir::new().unwrap();
    let help = run_in(tmp.path(), &["--help"]);
    assert!(help.status.success());
    assert!(stdout(&help).contains("--delete"));
    let version = run_in(tmp.path(), &["--version"]);
    assert!(stdout(&version).starts_with("unpacker "));
}

#[test]
fn missing_file_is_named_without_pretending_to_start() {
    let tmp = tempfile::TempDir::new().unwrap();
    let output = run_in(tmp.path(), &["nope.rar"]);
    let err = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1));
    assert!(err.contains("nope.rar: file not found"), "{err}");
    assert!(!stdout(&output).contains("→"), "{}", stdout(&output));
}
