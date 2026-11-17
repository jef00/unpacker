//! End-to-end tests for the 7z backend. Most inputs are real archives made
//! by 7-Zip (see tests/fixtures/README.md); expected CRCs come from py7zr.

#![allow(clippy::unwrap_used, clippy::panic)]

mod common;

use std::fs;
use std::io::{Cursor, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use common::*;
use sevenz_rust2::encoder_options::AesEncoderOptions;
use sevenz_rust2::{ArchiveEntry, ArchiveWriter, EncoderMethod, Password};
use tempfile::TempDir;
use unpacker::{Error, Scheme, backend, discovery};

/// Contents of `archive.7z.001/.002` and `lzma2bcj.7z` (same files).
const SZIP_FILES: &[(&str, u32)] = &[
    ("mingw64/bin/libszip-0.dll", 0xbd53_59f4),
    ("mingw64/include/SZconfig.h", 0x34c7_9d01),
    ("mingw64/include/ricehdf.h", 0x461f_2456),
    ("mingw64/include/szip_adpt.h", 0xd214_7492),
    ("mingw64/include/szlib.h", 0x8512_30b8),
    ("mingw64/lib/libszip.a", 0x129a_341e),
    ("mingw64/lib/libszip.dll.a", 0x98cb_0094),
    ("mingw64/share/doc/szip/COPYING", 0xfb58_f485),
    ("mingw64/share/doc/szip/HISTORY.txt", 0xf54c_f050),
    ("mingw64/share/doc/szip/INSTALL", 0x8d47_7be9),
    ("mingw64/share/doc/szip/README", 0x916d_437a),
    ("mingw64/share/doc/szip/RELEASE.txt", 0x8a07_4911),
];

/// Contents of `solid.7z`, `copy.7z` and `encrypted_1.7z`.
const TEST_FILES: &[(&str, u32)] = &[("test/test2.txt", 0x88b7_9ace), ("test1.txt", 0x0862_6a3e)];

fn assert_files(dest: &Path, expected: &[(&str, u32)]) {
    let names: Vec<&str> = expected.iter().map(|(n, _)| *n).collect();
    assert_eq!(tree(dest), names);
    for (name, crc) in expected {
        let data = fs::read(dest.join(name)).unwrap();
        assert_eq!(crc32(&data), *crc, "CRC mismatch for {name}");
    }
}

fn no_temp_files(dest: &Path) -> bool {
    !tree(dest).iter().any(|f| f.ends_with(".unpacker-tmp"))
}

/// Builds a non-solid 7z in memory with our own writer.
fn build_7z(entries: &[(&str, &[u8])], encrypt: Option<(&str, EncoderMethod)>) -> Vec<u8> {
    let mut w = ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
    w.set_encrypt_header(false);
    if let Some((password, method)) = encrypt {
        w.set_content_methods(vec![
            AesEncoderOptions::new(Password::from(password)).into(),
            method.into(),
        ]);
    }
    for (name, data) in entries {
        w.push_archive_entry(ArchiveEntry::new_file(name), Some(Cursor::new(*data)))
            .unwrap();
    }
    w.finish().unwrap().into_inner()
}

// ---------------------------------------------------------------------------
// Real 7-Zip archives
// ---------------------------------------------------------------------------

#[test]
fn real_multivolume_split_from_second_part() {
    let tmp = copy_fixtures(&["7z/archive.7z.001", "7z/archive.7z.002"]);
    let dest = tmp.path().join("out");

    let set = discovery::discover_on_disk(&tmp.path().join("archive.7z.002")).unwrap();
    assert_eq!((set.scheme, set.output_name()), (Scheme::Split, "archive"));

    let (result, rec) = extract(&tmp.path().join("archive.7z.002"), &dest, None);
    result.unwrap();
    assert_files(&dest, SZIP_FILES);
    assert!(dest.join("mingw64/share/doc/szip").is_dir());
    assert!(rec.warnings.is_empty(), "{:?}", rec.warnings);
}

#[test]
fn multivolume_missing_last_part_says_so() {
    let tmp = copy_fixtures(&["7z/archive.7z.001"]);

    let (result, _) = extract(
        &tmp.path().join("archive.7z.001"),
        &tmp.path().join("out"),
        None,
    );

    match result {
        Err(Error::Corrupt(msg)) => {
            assert!(msg.contains("\"archive.7z.002\" may be missing"), "{msg}");
        }
        other => panic!("expected Corrupt, got {other:?}"),
    }
}

#[test]
fn lzma2_with_bcj_filter_and_nested_folders() {
    let tmp = copy_fixtures(&["7z/lzma2bcj.7z"]);
    let dest = tmp.path().join("out");
    let (result, _) = extract(&tmp.path().join("lzma2bcj.7z"), &dest, None);
    result.unwrap();
    assert_files(&dest, SZIP_FILES);
}

#[test]
fn solid_and_stored_archives() {
    for name in ["solid.7z", "copy.7z"] {
        let tmp = copy_fixtures(&[&format!("7z/{name}")]);
        let dest = tmp.path().join("out");
        let (result, _) = extract(&tmp.path().join(name), &dest, None);
        result.unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_files(&dest, TEST_FILES);
    }
}

#[test]
fn non_ascii_filename() {
    let tmp = copy_fixtures(&["7z/umlaut-non_solid.7z"]);
    let dest = tmp.path().join("out");
    let (result, _) = extract(&tmp.path().join("umlaut-non_solid.7z"), &dest, None);
    result.unwrap();
    assert_eq!(
        read(dest.join("täst.txt")),
        "This file contains a german umlaut in the filename."
    );
}

#[test]
fn crc_error_fails_without_leaving_the_bad_file() {
    let tmp = copy_fixtures(&["7z/crc_corrupted.7z"]);
    let dest = tmp.path().join("out");

    let (result, _) = extract(&tmp.path().join("crc_corrupted.7z"), &dest, None);

    assert!(matches!(result, Err(Error::Corrupt(_))), "{result:?}");
    assert!(!dest.join("src/scripts/py7zr").exists());
    assert!(no_temp_files(&dest), "{:?}", tree(&dest));
}

// ---------------------------------------------------------------------------
// Passwords
// ---------------------------------------------------------------------------

#[test]
fn encrypted_data_password_flow() {
    let tmp = copy_fixtures(&["7z/encrypted_1.7z"]);
    let archive = tmp.path().join("encrypted_1.7z");
    let dest = tmp.path().join("out");

    // Headers aren't encrypted: we can list and see that a password is needed.
    let set = discovery::discover_on_disk(&archive).unwrap();
    let mut ex = backend::open(&set, None).unwrap();
    assert!(ex.needs_password().unwrap());
    assert_eq!(ex.list().unwrap().iter().filter(|e| e.encrypted).count(), 2);

    let (result, _) = extract(&archive, &dest, None);
    assert!(matches!(result, Err(Error::PasswordRequired)), "{result:?}");
    assert!(tree(&dest).is_empty());

    let (result, _) = extract(&archive, &dest, Some("wrong"));
    assert!(matches!(result, Err(Error::WrongPassword)), "{result:?}");
    assert!(tree(&dest).is_empty(), "left behind: {:?}", tree(&dest));

    let (result, _) = extract(&archive, &dest, Some("secret"));
    result.unwrap();
    assert_files(&dest, TEST_FILES);
}

#[test]
fn encrypted_headers_password_flow() {
    let tmp = copy_fixtures(&["7z/filename_encryption.7z"]);
    let archive = tmp.path().join("filename_encryption.7z");
    let dest = tmp.path().join("out");
    let set = discovery::discover_on_disk(&archive).unwrap();

    // Can't even read the file list without the password.
    assert!(matches!(
        backend::open(&set, None),
        Err(Error::PasswordRequired)
    ));
    assert!(matches!(
        backend::open(&set, Some("nope")),
        Err(Error::WrongPassword)
    ));

    let (result, _) = extract(&archive, &dest, Some("hello"));
    result.unwrap();
    assert_eq!(tree(&dest), ["New Text Document.TXT"]);
}

#[test]
fn wrong_password_on_stored_aes_leaves_no_partial_file() {
    // AES without compression is the usual way videos get "protected":
    // there's no decompressor to choke, only the CRC at the very end.
    let tmp = TempDir::new().unwrap();
    let video: Vec<u8> = (0..700_000u32).map(|i| (i % 253) as u8).collect();
    let bytes = build_7z(
        &[("movie.mkv", &video)],
        Some(("right", EncoderMethod::COPY)),
    );
    let archive = write(tmp.path(), "movie.7z", &bytes);
    let dest = tmp.path().join("out");

    let (result, _) = extract(&archive, &dest, Some("wrong"));
    assert!(matches!(result, Err(Error::WrongPassword)), "{result:?}");
    assert!(tree(&dest).is_empty(), "left behind: {:?}", tree(&dest));

    let (result, _) = extract(&archive, &dest, Some("right"));
    result.unwrap();
    assert_eq!(fs::read(dest.join("movie.mkv")).unwrap(), video);
}

// ---------------------------------------------------------------------------
// Safety and splits of generated archives
// ---------------------------------------------------------------------------

#[test]
fn path_traversal_entries_are_skipped() {
    let tmp = TempDir::new().unwrap();
    let bytes = build_7z(&[("../escaped.txt", b"evil"), ("good.txt", b"fine")], None);
    let archive = write(tmp.path(), "x.7z", &bytes);
    let dest = tmp.path().join("out");

    let (result, rec) = extract(&archive, &dest, None);
    result.unwrap();

    assert_eq!(tree(&dest), ["good.txt"]);
    assert!(!tmp.path().join("escaped.txt").exists());
    assert_eq!(rec.warnings.len(), 1, "{:?}", rec.warnings);
}

#[test]
fn generated_split_into_many_parts() {
    let tmp = TempDir::new().unwrap();
    let data: Vec<u8> = (0..400_000u32).map(|i| (i * 31 % 256) as u8).collect();
    let bytes = build_7z(&[("episode.mkv", &data)], None);
    let parts = write_split(tmp.path(), "Show.S01E01.7z", &bytes, 5);
    let dest = tmp.path().join("out");

    let (result, _) = extract(&parts[3], &dest, None);
    result.unwrap();
    assert_eq!(fs::read(dest.join("episode.mkv")).unwrap(), data);
}

// ---------------------------------------------------------------------------
// The real binary
// ---------------------------------------------------------------------------

#[test]
fn binary_unpacks_real_split_next_to_it() {
    let tmp = copy_fixtures(&["7z/archive.7z.001", "7z/archive.7z.002"]);

    let mut child = Command::new(env!("CARGO_BIN_EXE_unpacker"))
        .arg(tmp.path().join("archive.7z.002"))
        .arg(tmp.path().join("archive.7z.001")) // same set dropped twice
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"\n").unwrap();
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(output.status.success(), "{stdout}");
    assert_eq!(
        stdout.matches("✓ Extracted").count(),
        1,
        "unpacked once:\n{stdout}"
    );
    // Everything sits in `mingw64/`, so that level is skipped.
    let unnested: Vec<(&str, u32)> = SZIP_FILES
        .iter()
        .map(|&(name, crc)| (name.strip_prefix("mingw64/").unwrap(), crc))
        .collect();
    assert_files(&tmp.path().join("archive"), &unnested);
}
