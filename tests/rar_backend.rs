//! End-to-end tests for the RAR backend, on real archives made by WinRAR /
//! `rar` (see tests/fixtures/README.md). Expected CRCs come from `rarfile`.

#![allow(clippy::unwrap_used, clippy::panic)]

mod common;

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use common::*;
use unpacker::{Error, Scheme, backend, discovery};

const VOLS: &[&str] = &["vols/bigfile.txt", "vols/smallfile.txt"];
const VOLS_CRC: &[u32] = &[0x509a_d74c, 0xd08a_1f86];

fn assert_files(dest: &Path, names: &[&str], crcs: &[u32]) {
    assert_eq!(tree(dest), names);
    for (name, crc) in names.iter().zip(crcs) {
        let data = fs::read(dest.join(name)).unwrap();
        assert_eq!(crc32(&data), *crc, "CRC mismatch for {name}");
    }
}

/// No `.unpacker-tmp` leftovers anywhere under `dir`.
fn clean(dir: &Path) -> bool {
    !tree(dir).iter().any(|f| f.contains(".unpacker-tmp"))
}

fn unpack(tmp: &Path, dropped: &str, password: Option<&str>) -> (unpacker::Result<()>, Recorder) {
    extract(&tmp.join(dropped), &tmp.join("out"), password)
}

// ---------------------------------------------------------------------------
// Volumes
// ---------------------------------------------------------------------------

#[test]
fn rar3_old_style_volumes_from_a_middle_part() {
    let tmp = copy_fixtures(&["rar/rar3-old.rar", "rar/rar3-old.r00", "rar/rar3-old.r01"]);
    let set = discovery::discover_on_disk(&tmp.path().join("rar3-old.r00")).unwrap();
    assert_eq!((set.scheme, set.parts.len()), (Scheme::RarOld, 3));

    let (result, _) = unpack(tmp.path(), "rar3-old.r00", None);
    result.unwrap();
    assert_files(&tmp.path().join("out"), VOLS, VOLS_CRC);
}

#[test]
fn rar3_and_rar5_new_style_volumes_from_last_part() {
    for prefix in ["rar3-vols", "rar5-vols"] {
        let names: Vec<String> = (1..=3)
            .map(|n| format!("rar/{prefix}.part{n}.rar"))
            .collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let tmp = copy_fixtures(&names);

        let (result, _) = unpack(tmp.path(), &format!("{prefix}.part3.rar"), None);
        result.unwrap_or_else(|e| panic!("{prefix}: {e}"));
        assert_files(&tmp.path().join("out"), VOLS, VOLS_CRC);
    }
}

#[test]
fn missing_middle_volume_is_named_before_anything_runs() {
    let tmp = copy_fixtures(&["rar/rar5-vols.part1.rar", "rar/rar5-vols.part3.rar"]);
    let (result, _) = unpack(tmp.path(), "rar5-vols.part1.rar", None);
    match result {
        Err(Error::MissingVolume(name)) => assert_eq!(name, "rar5-vols.part2.rar"),
        other => panic!("expected MissingVolume, got {other:?}"),
    }
}

#[test]
fn missing_last_volume_is_reported_and_leaves_no_partial_file() {
    let tmp = copy_fixtures(&["rar/rar5-vols.part1.rar", "rar/rar5-vols.part2.rar"]);

    let (result, _) = unpack(tmp.path(), "rar5-vols.part1.rar", None);

    match result {
        Err(Error::MissingVolume(name)) => assert_eq!(name, "rar5-vols.part3.rar"),
        other => panic!("expected MissingVolume, got {other:?}"),
    }
    let out = tmp.path().join("out");
    assert!(!out.join("vols/bigfile.txt").exists());
    assert!(clean(tmp.path()), "{:?}", tree(tmp.path()));
}

#[test]
fn byte_split_rar_is_joined_and_cleaned_up() {
    let tmp = tempfile::TempDir::new().unwrap();
    let bytes = fs::read(fixture("rar/rar5-subdirs.rar")).unwrap();
    let parts = write_split(tmp.path(), "subdirs.rar", &bytes, 3);

    let (result, _) = extract(&parts[2], &tmp.path().join("out"), None);
    result.unwrap();

    assert_eq!(read(tmp.path().join("out/sub/dir1/file1.txt")).len(), 6);
    // Only the parts and the output: the joined temp copy is gone.
    let top: Vec<String> = fs::read_dir(tmp.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(top.len(), 4, "{top:?}");
}

// ---------------------------------------------------------------------------
// Content
// ---------------------------------------------------------------------------

#[test]
fn nested_unicode_and_empty_directories() {
    let tmp = copy_fixtures(&["rar/rar5-subdirs.rar"]);
    let (result, rec) = unpack(tmp.path(), "rar5-subdirs.rar", None);
    result.unwrap();

    let out = tmp.path().join("out");
    assert_files(
        &out,
        &[
            "sub/dir1/file1.txt",
            "sub/dir2/file2.txt",
            "sub/with space/long fn.txt",
            "sub/üȵĩöḋè/file.txt",
        ],
        &[0xe229_f704, 0xc904_a4c7, 0x71b7_247d, 0x2fec_89c1],
    );
    assert!(out.join("sub/empty").is_dir());
    assert!(rec.warnings.is_empty(), "{:?}", rec.warnings);
}

#[test]
fn solid_archives() {
    for name in ["rar3-solid.rar", "rar5-solid.rar"] {
        let tmp = copy_fixtures(&[&format!("rar/{name}")]);
        let (result, _) = unpack(tmp.path(), name, None);
        result.unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_files(
            &tmp.path().join("out"),
            &["stest1.txt", "stest2.txt"],
            &[0xc5b7_e6a2, 0xc5b7_e6a2],
        );
    }
}

#[test]
fn corrupt_data_fails_without_partial_files() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut bytes = fs::read(fixture("rar/rar5-vols.part1.rar")).unwrap();
    // Hit the compressed data of bigfile.txt, well past the headers.
    bytes[60_000] ^= 0xFF;
    let path = write(tmp.path(), "damaged.rar", &bytes);

    // A lone part1 with SPLIT_AFTER: damage or the missing part2 — either way
    // it must fail cleanly.
    let (result, _) = extract(&path, &tmp.path().join("out"), None);
    assert!(matches!(result, Err(Error::Corrupt(_))), "{result:?}");
    assert!(!tmp.path().join("out/vols/bigfile.txt").exists());
    assert!(clean(tmp.path()), "{:?}", tree(tmp.path()));
}

// ---------------------------------------------------------------------------
// Passwords
// ---------------------------------------------------------------------------

#[test]
fn rar5_encrypted_data_password_flow() {
    let tmp = copy_fixtures(&["rar/rar5-psw.rar"]);
    let out = tmp.path().join("out");

    let set = discovery::discover_on_disk(&tmp.path().join("rar5-psw.rar")).unwrap();
    let mut ex = backend::open(&set, None).unwrap();
    assert!(ex.needs_password().unwrap());
    assert!(ex.list().unwrap().iter().all(|e| e.encrypted));

    let (result, _) = unpack(tmp.path(), "rar5-psw.rar", None);
    assert!(matches!(result, Err(Error::PasswordRequired)), "{result:?}");
    assert!(tree(&out).is_empty());

    let (result, _) = unpack(tmp.path(), "rar5-psw.rar", Some("wrong"));
    assert!(matches!(result, Err(Error::WrongPassword)), "{result:?}");
    assert!(tree(&out).is_empty(), "left behind: {:?}", tree(&out));

    let (result, _) = unpack(tmp.path(), "rar5-psw.rar", Some("password"));
    result.unwrap();
    // RAR5 encryption stores a keyed ("tweaked") checksum, so the header CRC
    // isn't the plain CRC-32. Same content as the other `stest` fixtures.
    assert_files(
        &out,
        &["stest1.txt", "stest2.txt"],
        &[0xc5b7_e6a2, 0xc5b7_e6a2],
    );
}

#[test]
fn rar5_encrypted_headers_password_flow() {
    let tmp = copy_fixtures(&["rar/rar5-hpsw.rar"]);
    let set = discovery::discover_on_disk(&tmp.path().join("rar5-hpsw.rar")).unwrap();

    assert!(matches!(
        backend::open(&set, None),
        Err(Error::PasswordRequired)
    ));
    assert!(matches!(
        backend::open(&set, Some("wrong")),
        Err(Error::WrongPassword)
    ));

    let (result, _) = unpack(tmp.path(), "rar5-hpsw.rar", Some("password"));
    result.unwrap();
    assert_files(
        &tmp.path().join("out"),
        &["stest1.txt", "stest2.txt"],
        &[0xc5b7_e6a2, 0xc5b7_e6a2],
    );
}

#[test]
fn rar3_encrypted_archive_with_password() {
    // Holds two empty files, so a wrong password is undetectable here
    // (RAR3 has no password check; there's no data to fail a CRC).
    let tmp = copy_fixtures(&["rar/rar3-comment-psw.rar"]);
    let (result, _) = unpack(tmp.path(), "rar3-comment-psw.rar", None);
    assert!(matches!(result, Err(Error::PasswordRequired)), "{result:?}");

    let (result, _) = unpack(tmp.path(), "rar3-comment-psw.rar", Some("password"));
    result.unwrap();
    assert_eq!(tree(&tmp.path().join("out")), ["file1.txt", "file2.txt"]);
}

// ---------------------------------------------------------------------------
// Safety
// ---------------------------------------------------------------------------

#[test]
fn malicious_symlink_cannot_escape() {
    // `up` is a symlink to `..`, then `up/pwned.txt` tries to write through it.
    let tmp = copy_fixtures(&["rar/rar5-evil-symlink-traversal.rar"]);
    let (result, rec) = unpack(tmp.path(), "rar5-evil-symlink-traversal.rar", None);
    result.unwrap();

    let out = tmp.path().join("out");
    assert!(
        !tmp.path().join("pwned.txt").exists(),
        "escaped the output folder!"
    );
    assert!(
        !fs::symlink_metadata(out.join("up"))
            .unwrap()
            .file_type()
            .is_symlink(),
        "symlink was created"
    );
    assert_eq!(tree(&out), ["up/pwned.txt"]); // harmless: a real folder now
    assert!(
        rec.warnings.iter().any(|w| w.contains("symbolic link")),
        "{:?}",
        rec.warnings
    );
}

// ---------------------------------------------------------------------------
// The real binary
// ---------------------------------------------------------------------------

#[test]
fn binary_prompts_for_rar_password() {
    let tmp = copy_fixtures(&["rar/rar5-hpsw.rar"]);

    let mut child = Command::new(env!("CARGO_BIN_EXE_unpacker"))
        .arg(tmp.path().join("rar5-hpsw.rar"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"nope\npassword\n\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        output.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("Wrong password"), "{stdout}");
    assert_eq!(
        tree(&tmp.path().join("rar5-hpsw")),
        ["stest1.txt", "stest2.txt"]
    );
}
