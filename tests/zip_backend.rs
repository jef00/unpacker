//! End-to-end tests for the ZIP backend: real archives on disk, going
//! through discovery → open → extract exactly like the binary does.

// Test helpers aren't `#[test]` fns, so clippy's in-test exemption misses them.
#![allow(clippy::unwrap_used, clippy::panic)]

use std::fs;
use std::io::{Cursor, Write};
use std::process::{Command, Stdio};

use tempfile::TempDir;
use unpacker::backend;
use unpacker::{Error, discovery};
use zip::write::SimpleFileOptions;
use zip::{AesMode, CompressionMethod, ZipWriter};

mod common;

use common::*;

enum Item<'a> {
    File(&'a str, &'a [u8]),
    Secret(&'a str, &'a [u8], &'a str),
    Stored(&'a str, &'a [u8]),
    Dir(&'a str),
}

fn build_zip(items: &[Item<'_>]) -> Vec<u8> {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let deflate = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    for item in items {
        match *item {
            Item::File(name, data) => {
                zip.start_file(name, deflate).unwrap();
                zip.write_all(data).unwrap();
            }
            Item::Secret(name, data, password) => {
                zip.start_file(name, deflate.with_aes_encryption(AesMode::Aes256, password))
                    .unwrap();
                zip.write_all(data).unwrap();
            }
            Item::Stored(name, data) => {
                let stored = deflate.compression_method(CompressionMethod::Stored);
                zip.start_file(name, stored).unwrap();
                zip.write_all(data).unwrap();
            }
            Item::Dir(name) => zip.add_directory(name, deflate).unwrap(),
        }
    }
    zip.finish().unwrap().into_inner()
}

// ---------------------------------------------------------------------------
// Plain archives
// ---------------------------------------------------------------------------

#[test]
fn extracts_files_and_directories() {
    let tmp = TempDir::new().unwrap();
    // Big enough to cross the 256 KiB copy buffer several times.
    let video: Vec<u8> = (0..1_000_000u32).map(|i| (i % 251) as u8).collect();
    let zip = build_zip(&[
        Item::Dir("Season 1/"),
        Item::File("Season 1/Episode 01.mkv", &video),
        Item::File(
            "Season 1/Episode 01.nl.srt",
            b"1\n00:00:01,000 --> 00:00:02,000\nHallo\n",
        ),
        Item::Dir("Extras/"),
        Item::File("readme.txt", b"enjoy"),
    ]);
    let archive = write(tmp.path(), "Show.S01.zip", &zip);
    let dest = tmp.path().join("out");

    let (result, rec) = extract(&archive, &dest, None);
    result.unwrap();

    assert_eq!(
        tree(&dest),
        [
            "Season 1/Episode 01.mkv",
            "Season 1/Episode 01.nl.srt",
            "readme.txt"
        ]
    );
    assert!(
        dest.join("Extras").is_dir(),
        "empty directories are created"
    );
    assert_eq!(
        fs::read(dest.join("Season 1/Episode 01.mkv")).unwrap(),
        video
    );
    assert_eq!(read(dest.join("readme.txt")), "enjoy");
    assert_eq!(rec.started.len(), 3);
    assert!(rec.warnings.is_empty(), "{:?}", rec.warnings);
}

#[test]
fn list_reports_entries() {
    let tmp = TempDir::new().unwrap();
    let zip = build_zip(&[
        Item::Dir("d/"),
        Item::File("d/a.txt", b"12345"),
        Item::Secret("b.txt", b"x", "pw"),
    ]);
    let archive = write(tmp.path(), "x.zip", &zip);

    let set = discovery::discover_on_disk(&archive).unwrap();
    let mut ex = backend::open(&set, None).unwrap();
    let entries = ex.list().unwrap();

    let summary: Vec<_> = entries
        .iter()
        .map(|e| (e.name.as_str(), e.size, e.is_dir, e.encrypted))
        .collect();
    assert_eq!(
        summary,
        [
            ("d/", 0, true, false),
            ("d/a.txt", 5, false, false),
            ("b.txt", 1, false, true)
        ]
    );
}

// ---------------------------------------------------------------------------
// Split archives (.zip.001, .zip.002, …)
// ---------------------------------------------------------------------------

#[test]
fn split_archive_extracts_from_any_part() {
    let tmp = TempDir::new().unwrap();
    let data: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 256) as u8).collect();
    let zip = build_zip(&[Item::Stored("movie.mkv", &data)]);
    let parts = write_split(tmp.path(), "movie.zip", &zip, 3);
    let dest = tmp.path().join("out");

    let (result, _) = extract(&parts[1], &dest, None); // drop the middle part
    result.unwrap();

    assert_eq!(fs::read(dest.join("movie.mkv")).unwrap(), data);
}

#[test]
fn split_archive_missing_last_part_says_so() {
    let tmp = TempDir::new().unwrap();
    let zip = build_zip(&[Item::Stored("movie.mkv", &[7u8; 100_000])]);
    let parts = write_split(tmp.path(), "movie.zip", &zip, 3);
    fs::remove_file(&parts[2]).unwrap();

    let (result, _) = extract(&parts[0], &tmp.path().join("out"), None);

    match result {
        Err(Error::Corrupt(msg)) => {
            assert!(msg.contains("\"movie.zip.003\" may be missing"), "{msg}");
        }
        other => panic!("expected Corrupt, got {other:?}"),
    }
}

#[test]
fn spanned_zip_is_rejected_with_a_clear_message() {
    let tmp = TempDir::new().unwrap();
    let first = write(tmp.path(), "movie.z01", b"PK\x07\x08PK\x03\x04junk");
    write(tmp.path(), "movie.zip", b"junk");

    let (result, _) = extract(&first, &tmp.path().join("out"), None);

    match result {
        Err(Error::Unsupported(msg)) => assert!(msg.contains("spanned"), "{msg}"),
        other => panic!("expected Unsupported, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Safety
// ---------------------------------------------------------------------------

#[test]
fn path_traversal_entries_are_skipped_not_written() {
    let tmp = TempDir::new().unwrap();
    let zip = build_zip(&[
        Item::File("../escaped.txt", b"evil"),
        Item::File("ok/../../also-escaped.txt", b"evil"),
        Item::File("good.txt", b"fine"),
    ]);
    let archive = write(tmp.path(), "x.zip", &zip);
    let dest = tmp.path().join("out");

    let (result, rec) = extract(&archive, &dest, None);
    result.unwrap();

    assert_eq!(tree(&dest), ["good.txt"]);
    assert!(!tmp.path().join("escaped.txt").exists());
    assert!(!tmp.path().join("also-escaped.txt").exists());
    assert_eq!(rec.warnings.len(), 2, "{:?}", rec.warnings);
}

#[test]
fn corrupt_entry_fails_and_leaves_no_partial_file() {
    let tmp = TempDir::new().unwrap();
    let payload = b"THIS-IS-THE-ORIGINAL-PAYLOAD".repeat(100);
    let mut zip = build_zip(&[Item::Stored("video.mkv", &payload)]);
    // Flip one byte inside the stored data: CRC check must catch it.
    let at = zip
        .windows(payload.len())
        .position(|w| w == payload.as_slice())
        .unwrap();
    zip[at + 500] ^= 0xFF;
    let archive = write(tmp.path(), "x.zip", &zip);
    let dest = tmp.path().join("out");

    let (result, _) = extract(&archive, &dest, None);

    assert!(matches!(result, Err(Error::Corrupt(_))), "{result:?}");
    assert!(tree(&dest).is_empty(), "left behind: {:?}", tree(&dest));
}

// ---------------------------------------------------------------------------
// Passwords
// ---------------------------------------------------------------------------

#[test]
fn aes_password_flow() {
    let tmp = TempDir::new().unwrap();
    let zip = build_zip(&[Item::Secret("secret.txt", b"top secret", "hunter2")]);
    let archive = write(tmp.path(), "x.zip", &zip);
    let dest = tmp.path().join("out");

    let set = discovery::discover_on_disk(&archive).unwrap();
    assert!(backend::open(&set, None).unwrap().needs_password().unwrap());

    let (result, _) = extract(&archive, &dest, None);
    assert!(matches!(result, Err(Error::PasswordRequired)), "{result:?}");
    assert!(!dest.exists(), "nothing may be written without a password");

    let (result, _) = extract(&archive, &dest, Some("wrong"));
    assert!(matches!(result, Err(Error::WrongPassword)), "{result:?}");
    assert!(
        !dest.exists(),
        "nothing may be written with a wrong password"
    );

    let (result, _) = extract(&archive, &dest, Some("hunter2"));
    result.unwrap();
    assert_eq!(read(dest.join("secret.txt")), "top secret");
}

#[test]
fn mixed_archive_asks_for_password_before_writing_anything() {
    let tmp = TempDir::new().unwrap();
    let zip = build_zip(&[
        Item::File("1-public.txt", b"hi"),
        Item::Secret("2-private.txt", b"secret", "pw"),
    ]);
    let archive = write(tmp.path(), "x.zip", &zip);
    let dest = tmp.path().join("out");

    let (result, _) = extract(&archive, &dest, None);
    assert!(matches!(result, Err(Error::PasswordRequired)), "{result:?}");
    assert!(!dest.exists());

    let (result, _) = extract(&archive, &dest, Some("pw"));
    result.unwrap();
    assert_eq!(tree(&dest), ["1-public.txt", "2-private.txt"]);
}

#[test]
fn zipcrypto_archive_made_by_info_zip() {
    // tests/fixtures/zip/zipcrypto.zip: `zip -P secret zipcrypto.zip hello.txt`
    let tmp = TempDir::new().unwrap();
    let archive = tmp.path().join("zipcrypto.zip");
    fs::copy(fixture("zip/zipcrypto.zip"), &archive).unwrap();
    let dest = tmp.path().join("out");

    let (result, _) = extract(&archive, &dest, Some("nope"));
    assert!(matches!(result, Err(Error::WrongPassword)), "{result:?}");

    let (result, _) = extract(&archive, &dest, Some("secret"));
    result.unwrap();
    assert_eq!(read(dest.join("hello.txt")), "hello from zipcrypto\n");
}

// ---------------------------------------------------------------------------
// The real binary
// ---------------------------------------------------------------------------

#[test]
fn binary_prompts_for_password_and_retries() {
    let tmp = TempDir::new().unwrap();
    let zip = build_zip(&[Item::Secret("episode.mkv", b"video bytes", "letmein")]);
    let archive = write(tmp.path(), "Show.S01E01.zip", &zip);

    let mut child = Command::new(env!("CARGO_BIN_EXE_unpacker"))
        .arg(&archive)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"wrong\nletmein\n\n") // last line answers the Windows pause
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        output.status.success(),
        "stdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("Wrong password"), "{stdout}");
    assert_eq!(
        read(tmp.path().join("Show.S01E01/episode.mkv")),
        "video bytes"
    );
}
