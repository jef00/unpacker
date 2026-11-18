//! Archives whose names Telegram shortened. Telegram keeps the extension and
//! cuts the name before it when it's long, so whatever sits there can be
//! lost: the `.7z` of `Title.7z.001`, or the `.tar` of `Title.tar.gz`.
//! These are recognised by content. (`.part01.rar` names are renamed back,
//! see `volume_names.rs`.)

#![allow(clippy::unwrap_used, clippy::panic)]

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use common::*;
use tempfile::TempDir;
use unpacker::{Scheme, discovery};

/// Copies fixtures into a temp dir under new names.
fn arrive(files: &[(&str, &str)]) -> TempDir {
    let tmp = TempDir::new().unwrap();
    for (fixture_name, name) in files {
        fs::copy(fixture(fixture_name), tmp.path().join(name)).unwrap();
    }
    tmp
}

fn files_in(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    files.sort();
    files
}

/// Runs the program in `dir` like a double-click, keeping the archives.
fn double_click(dir: &Path) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_unpacker"))
        .arg("--keep")
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(output.status.success(), "{out}");
    out
}

// ---------------------------------------------------------------------------
// `.001` splits that lost their archive extension
// ---------------------------------------------------------------------------

#[test]
fn split_7z_that_lost_its_extension_is_found_by_content() {
    let tmp = arrive(&[
        ("7z/archive.7z.001", "Some.Show.S01E01.1080p.001"),
        ("7z/archive.7z.002", "Some.Show.S01E01.1080p.002"),
    ]);
    let set = discovery::discover_on_disk(&tmp.path().join("Some.Show.S01E01.1080p.002")).unwrap();
    assert_eq!((set.scheme, set.parts.len()), (Scheme::Split, 2));

    let found = discovery::archives_among_on_disk(&files_in(tmp.path()));
    assert_eq!(found, [tmp.path().join("Some.Show.S01E01.1080p.001")]);

    let (result, _) = extract(
        &tmp.path().join("Some.Show.S01E01.1080p.002"),
        &tmp.path().join("out"),
        None,
    );
    result.unwrap();
    assert_eq!(tree(&tmp.path().join("out")).len(), 12);
}

#[test]
fn split_tar_gz_cut_mid_extension_is_found_by_content() {
    // `Title.tar.gz.001` with the name cut inside `.tar.gz`.
    let tmp = TempDir::new().unwrap();
    let bytes = fs::read(fixture("tar/show.tar.gz")).unwrap();
    let parts = write_split(tmp.path(), "Title.tar.g", &bytes, 3);

    let found = discovery::archives_among_on_disk(&files_in(tmp.path()));
    assert_eq!(found, [parts[0].clone()]);
    let (result, _) = extract(&parts[2], &tmp.path().join("out"), None);
    result.unwrap();
}

#[test]
fn raw_split_video_still_is_not_a_set() {
    let tmp = TempDir::new().unwrap();
    // Matroska magic: a video, not an archive.
    fs::write(tmp.path().join("movie.mkv.001"), b"\x1a\x45\xdf\xa3 video").unwrap();
    fs::write(tmp.path().join("movie.mkv.002"), b"more video").unwrap();
    let set = discovery::discover_on_disk(&tmp.path().join("movie.mkv.001")).unwrap();
    assert_eq!(set.scheme, Scheme::Single);
    assert!(discovery::archives_among_on_disk(&files_in(tmp.path())).is_empty());
}

// ---------------------------------------------------------------------------
// Compressed tars that lost their `.tar`
// ---------------------------------------------------------------------------

#[test]
fn compressed_tars_that_lost_their_tar_are_found_by_content() {
    let tmp = arrive(&[
        ("tar/show.tar.gz", "Some_Very_Long_Title.ta.gz"),
        ("tar/show.tar.bz2", "Another_Long_Title.bz2"),
        ("tar/show.tar.zst", "Third_Title.t.zst"),
        // A `.gz` of something else stays ignored, whatever it's called.
        ("tar/notes.txt.gz", "notes.gz"),
    ]);

    let found = discovery::archives_among_on_disk(&files_in(tmp.path()));
    let names: Vec<_> = found
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        [
            "Another_Long_Title.bz2",
            "Some_Very_Long_Title.ta.gz",
            "Third_Title.t.zst"
        ]
    );

    let out = double_click(tmp.path());
    assert!(out.contains("Found 3 archive(s)"), "{out}");
    // Named after the title, without the cut-off `.ta` / `.t`.
    for title in ["Some_Very_Long_Title", "Another_Long_Title", "Third_Title"] {
        assert!(tmp.path().join(title).is_dir(), "{title}\n{out}");
    }
}

#[test]
fn output_names_drop_a_cut_off_tar() {
    let name = |file: &str| {
        discovery::discover(Path::new(file), &[])
            .unwrap()
            .output_name()
            .to_string()
    };
    assert_eq!(name("Title.tar.gz"), "Title");
    assert_eq!(name("Title.ta.gz"), "Title");
    assert_eq!(name("Title.t.xz"), "Title");
    assert_eq!(name("Title.gz"), "Title");
    // Only after a tar compression: other dots are part of the name.
    assert_eq!(name("Some.Show.t.7z"), "Some.Show.t");
}
