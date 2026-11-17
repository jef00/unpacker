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

// ---------------------------------------------------------------------------
// Spanned archives (.z01, .z02, …, .zip), made by Info-ZIP `zip -s 64k`
// ---------------------------------------------------------------------------

/// The pseudo-random generator the fixtures' `video.bin` was made with.
fn lcg_bytes(n: usize) -> Vec<u8> {
    let mut x: u64 = 12345;
    (0..n)
        .map(|_| {
            x = (x * 1_103_515_245 + 12345) % (1 << 31);
            (x >> 16) as u8
        })
        .collect()
}

const SPANNED: &[&str] = &[
    "zip/spanned.z01",
    "zip/spanned.z02",
    "zip/spanned.z03",
    "zip/spanned.z04",
    "zip/spanned.z05",
    "zip/spanned.zip",
];

#[test]
fn spanned_zip_from_a_middle_part() {
    // 6 parts; file data crosses parts, and the central directory itself
    // starts on disk 3 and runs over three parts.
    let tmp = copy_fixtures(SPANNED);
    let set = discovery::discover_on_disk(&tmp.path().join("spanned.z03")).unwrap();
    assert_eq!(
        (set.scheme, set.parts.len()),
        (unpacker::Scheme::ZipSpanned, 6)
    );

    let dest = tmp.path().join("out");
    let (result, rec) = extract(&tmp.path().join("spanned.z03"), &dest, None);
    result.unwrap();

    assert_eq!(
        fs::read(dest.join("video.bin")).unwrap(),
        lcg_bytes(150_000)
    );
    for i in [0, 1, 599, 1199] {
        let expected = format!("file {i}\n").repeat(i % 7 + 1);
        assert_eq!(read(dest.join(format!("files/{i:04}.txt"))), expected);
    }
    assert_eq!(tree(&dest).len(), 1201);
    assert!(rec.warnings.is_empty(), "{:?}", rec.warnings);
}

#[test]
fn spanned_zip_with_zipcrypto_password() {
    let tmp = copy_fixtures(&["zip/spanned-pw.z01", "zip/spanned-pw.zip"]);
    let dropped = tmp.path().join("spanned-pw.zip");
    let dest = tmp.path().join("out");

    let (result, _) = extract(&dropped, &dest, None);
    assert!(matches!(result, Err(Error::PasswordRequired)), "{result:?}");
    let (result, _) = extract(&dropped, &dest, Some("wrong"));
    assert!(matches!(result, Err(Error::WrongPassword)), "{result:?}");
    assert!(tree(&dest).is_empty());

    let (result, _) = extract(&dropped, &dest, Some("secret"));
    result.unwrap();
    assert_eq!(
        fs::read(dest.join("secret.bin")).unwrap(),
        lcg_bytes(100_000)
    );
}

#[test]
fn spanned_zip64_made_with_zip_fz() {
    // `zip -fz` forces ZIP64 extra fields and ZIP64 end records.
    let tmp = copy_fixtures(&[
        "zip/spanned64.z01",
        "zip/spanned64.z02",
        "zip/spanned64.zip",
    ]);
    let dest = tmp.path().join("out");
    let (result, _) = extract(&tmp.path().join("spanned64.z01"), &dest, None);
    result.unwrap();
    assert_eq!(
        fs::read(dest.join("video.bin")).unwrap(),
        lcg_bytes(150_000)
    );
}

#[test]
fn spanned_zip_with_lying_sizes_fails_cleanly() {
    // A damaged/hostile .zip: the end record claims a ~4 GB central
    // directory, or one at an offset that overflows. Must be a clean error,
    // not a huge allocation or an arithmetic overflow.
    for (field_at, value) in [(12, 0xFFFF_FFF0u32), (16, 0xFFFF_FFF0)] {
        let tmp = copy_fixtures(&["zip/spanned-pw.z01", "zip/spanned-pw.zip"]);
        let last = tmp.path().join("spanned-pw.zip");
        let mut bytes = fs::read(&last).unwrap();
        let eocd = bytes.windows(4).rposition(|w| w == b"PK\x05\x06").unwrap();
        bytes[eocd + field_at..eocd + field_at + 4].copy_from_slice(&value.to_le_bytes());
        fs::write(&last, bytes).unwrap();

        let (result, _) = extract(&last, &tmp.path().join("out"), Some("secret"));
        assert!(matches!(result, Err(Error::Corrupt(_))), "{result:?}");
    }
}

#[test]
fn spanned_zip_missing_a_trailing_part_names_it() {
    // z01..z04 + .zip look complete to discovery; only the .zip knows it's
    // disk 6 of 6.
    let names: Vec<&str> = SPANNED
        .iter()
        .copied()
        .filter(|n| !n.ends_with("z05"))
        .collect();
    let tmp = copy_fixtures(&names);

    let (result, _) = extract(
        &tmp.path().join("spanned.zip"),
        &tmp.path().join("out"),
        None,
    );

    match result {
        Err(Error::MissingVolume(name)) => assert_eq!(name, "spanned.z05"),
        other => panic!("expected MissingVolume, got {other:?}"),
    }
}

#[test]
fn lone_last_part_of_spanned_set_asks_for_the_first() {
    let tmp = copy_fixtures(&["zip/spanned.zip"]);

    let (result, _) = extract(
        &tmp.path().join("spanned.zip"),
        &tmp.path().join("out"),
        None,
    );

    match result {
        Err(Error::MissingVolume(name)) => assert_eq!(name, "spanned.z01"),
        other => panic!("expected MissingVolume, got {other:?}"),
    }
}

#[test]
fn binary_unpacks_spanned_zip() {
    let tmp = copy_fixtures(SPANNED);

    let mut child = Command::new(env!("CARGO_BIN_EXE_unpacker"))
        .arg(tmp.path().join("spanned.zip"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"\n").unwrap();
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(output.status.success(), "{stdout}");
    assert!(stdout.contains("(6 parts)"), "{stdout}");
    let video = fs::read(tmp.path().join("spanned/video.bin")).unwrap();
    assert_eq!(video, lcg_bytes(150_000));
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
        .write_all(b"wrong\nletmein\n\n") // spare newline: harmless if no pause
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
