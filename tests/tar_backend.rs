//! End-to-end tests for the tar backend, on archives made by GNU tar,
//! gzip, bzip2, xz and libzstd (see tests/fixtures/README.md).

#![allow(clippy::unwrap_used, clippy::panic)]

mod common;

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use common::*;
use tempfile::TempDir;
use unpacker::{Error, Scheme, backend, discovery};

/// The pseudo-random generator the fixtures' `episode.mkv` was made with.
fn lcg_bytes(n: usize) -> Vec<u8> {
    let mut x: u64 = 12345;
    (0..n)
        .map(|_| {
            x = (x * 1_103_515_245 + 12345) % (1 << 31);
            (x >> 16) as u8
        })
        .collect()
}

const LONG_NAME: &str = "Show/a-very-long-folder-name-a-very-long-folder-name-a-very-long-folder-name-/\
                         and-an-even-longer-file-name-to-pass-the-100-character-limit.txt";

/// What every `show.*` fixture holds (regular files only; the symlink and
/// the hard link are skipped).
fn assert_show(dest: &Path) {
    assert_eq!(
        tree(dest),
        [
            "Show/Season 1/episode.mkv",
            "Show/Season 1/episode.nl.srt",
            LONG_NAME,
            "Show/täst ünïcode.txt",
        ]
    );
    assert_eq!(
        fs::read(dest.join("Show/Season 1/episode.mkv")).unwrap(),
        lcg_bytes(60_000)
    );
    assert_eq!(read(dest.join(LONG_NAME)), "long\n");
    assert_eq!(read(dest.join("Show/täst ünïcode.txt")), "unicode\n");
    assert!(dest.join("Show/Extras").is_dir(), "empty folder");
    assert!(
        !dest.join("Show/link-out").exists(),
        "symlink must not be created"
    );
}

/// Where the header of `entry` starts in a plain tar.
fn header_position(tar_bytes: &[u8], entry: &str) -> usize {
    let mut archive = tar::Archive::new(tar_bytes);
    for e in archive.entries().unwrap() {
        let e = e.unwrap();
        if e.path_bytes().as_ref() == entry.as_bytes() {
            return e.raw_header_position() as usize;
        }
    }
    panic!("{entry} not in archive");
}

// ---------------------------------------------------------------------------
// Formats
// ---------------------------------------------------------------------------

#[test]
fn every_compression_gives_the_same_files() {
    for name in [
        "show.tar",
        "show.tar.gz",
        "show.tar.bz2",
        "show-pax.tar.xz",
        "show.tar.zst",
    ] {
        let tmp = copy_fixtures(&[&format!("tar/{name}")]);
        let dest = tmp.path().join("out");

        let (result, rec) = extract(&tmp.path().join(name), &dest, None);
        result.unwrap_or_else(|e| panic!("{name}: {e}"));

        assert_show(&dest);
        assert_eq!(rec.warnings.len(), 2, "{name}: {:?}", rec.warnings);
        assert!(
            rec.warnings
                .iter()
                .all(|w| w.contains("links are not extracted"))
        );
    }
}

#[test]
fn list_has_files_and_folders_but_not_links() {
    let tmp = copy_fixtures(&["tar/show.tar.gz"]);
    let set = discovery::discover_on_disk(&tmp.path().join("show.tar.gz")).unwrap();
    let mut ex = backend::open(&set, None).unwrap();
    assert!(!ex.needs_password().unwrap());

    let entries = ex.list().unwrap();
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"Show/Extras/"), "{names:?}");
    assert!(names.contains(&LONG_NAME), "{names:?}");
    assert!(!names.iter().any(|n| n.contains("link")), "{names:?}");
    let mkv = entries
        .iter()
        .find(|e| e.name.ends_with("episode.mkv"))
        .unwrap();
    assert_eq!((mkv.size, mkv.is_dir), (60_000, false));
}

#[test]
fn a_compressed_file_that_is_not_a_tar_is_refused() {
    let tmp = copy_fixtures(&["tar/notes.txt.gz"]);
    let (result, _) = extract(
        &tmp.path().join("notes.txt.gz"),
        &tmp.path().join("out"),
        None,
    );
    match result {
        Err(Error::Unsupported(msg)) => assert!(msg.contains("isn't a tar archive"), "{msg}"),
        other => panic!("expected Unsupported, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Splits and truncation
// ---------------------------------------------------------------------------

#[test]
fn split_plain_and_compressed_tars() {
    for name in ["show.tar", "show.tar.gz"] {
        let tmp = TempDir::new().unwrap();
        let bytes = fs::read(fixture(&format!("tar/{name}"))).unwrap();
        let parts = write_split(tmp.path(), name, &bytes, 3);
        let set = discovery::discover_on_disk(&parts[1]).unwrap();
        assert_eq!((set.scheme, set.output_name()), (Scheme::Split, "show"));

        let dest = tmp.path().join("out");
        let (result, _) = extract(&parts[1], &dest, None);
        result.unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_show(&dest);
    }
}

#[test]
fn split_tar_cut_at_a_header_boundary_is_not_mistaken_for_complete() {
    // The dangerous case: part 1 ends exactly where a header starts, as with
    // 512-aligned part sizes. The tar crate alone would call this a normal
    // end, silently dropping every file from part 2 on.
    let tmp = TempDir::new().unwrap();
    let bytes = fs::read(fixture("tar/show.tar")).unwrap();
    let cut = header_position(&bytes, "Show/täst ünïcode.txt");
    assert_eq!(cut % 512, 0);
    let part1 = write(tmp.path(), "show.tar.001", &bytes[..cut]);
    // show.tar.002 is "missing".

    let (result, _) = extract(&part1, &tmp.path().join("out"), None);

    match result {
        Err(Error::Corrupt(msg)) => {
            assert!(msg.contains("\"show.tar.002\" may be missing"), "{msg}")
        }
        other => panic!("expected Corrupt, got {other:?}"),
    }
}

#[test]
fn single_tar_cut_short_is_an_error() {
    let tmp = TempDir::new().unwrap();
    let bytes = fs::read(fixture("tar/show.tar")).unwrap();
    let cut = header_position(&bytes, "Show/täst ünïcode.txt");
    let archive = write(tmp.path(), "cut.tar", &bytes[..cut]);

    let (result, _) = extract(&archive, &tmp.path().join("out"), None);

    match result {
        Err(Error::Corrupt(msg)) => assert!(msg.contains("ends too early"), "{msg}"),
        other => panic!("expected Corrupt, got {other:?}"),
    }
}

#[test]
fn truncated_compressed_tar_is_an_error_and_leaves_no_partial_file() {
    let tmp = TempDir::new().unwrap();
    let bytes = fs::read(fixture("tar/show.tar.gz")).unwrap();
    let archive = write(tmp.path(), "show.tar.gz", &bytes[..bytes.len() / 2]);
    let dest = tmp.path().join("out");

    let (result, _) = extract(&archive, &dest, None);

    assert!(matches!(result, Err(Error::Corrupt(_))), "{result:?}");
    assert!(
        !tree(&dest).iter().any(|f| f.contains(".unpacker-tmp")),
        "{:?}",
        tree(&dest)
    );
}

// ---------------------------------------------------------------------------
// Safety
// ---------------------------------------------------------------------------

#[test]
fn hostile_tar_cannot_escape() {
    // ../escaped.txt, /abs.txt, symlink up -> .., then up/pwned.txt
    let tmp = copy_fixtures(&["tar/evil.tar"]);
    let dest = tmp.path().join("out");

    let (result, rec) = extract(&tmp.path().join("evil.tar"), &dest, None);
    result.unwrap();

    assert!(!tmp.path().join("escaped.txt").exists());
    assert!(!tmp.path().join("pwned.txt").exists());
    assert!(
        !fs::symlink_metadata(dest.join("up"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    // Absolute paths become relative; `up` is a plain folder now.
    assert_eq!(tree(&dest), ["abs.txt", "good.txt", "up/pwned.txt"]);
    assert_eq!(rec.warnings.len(), 2, "{:?}", rec.warnings);
}

// ---------------------------------------------------------------------------
// The real binary
// ---------------------------------------------------------------------------

#[test]
fn binary_unpacks_tar_gz_and_flattens_the_top_folder() {
    let tmp = copy_fixtures(&["tar/show.tar.gz"]);

    let mut child = Command::new(env!("CARGO_BIN_EXE_unpacker"))
        .args(["--delete", "show.tar.gz"])
        .current_dir(tmp.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"\n").unwrap();
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(output.status.success(), "{stdout}");
    // One top-level folder ("Show") → its contents go straight into "show".
    let mkv = fs::read(tmp.path().join("show/Season 1/episode.mkv")).unwrap();
    assert_eq!(mkv, lcg_bytes(60_000));
    // The two links were skipped, so the archive is kept despite --delete.
    assert!(stdout.contains("2 files skipped"), "{stdout}");
    assert!(tmp.path().join("show.tar.gz").exists());
}
