//! Restoring RAR volume names that Telegram shortened, on real WinRAR
//! volumes renamed the way downloads actually arrive:
//! `Title.rar`, `Title (1).rar`, `Title (2).rar`, in download order.

#![allow(clippy::unwrap_used, clippy::panic)]

mod common;

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use common::*;
use tempfile::TempDir;
use unpacker::Error;
use unpacker::repair::fix_volume_names;

/// The real-world title that lost its `.part01` on Telegram.
const T: &str = "Sjakie_En_De_Chocolade_Fabriek_2005_Vlaams_,_Nederlands_Gesproken";

const VOLS: &[&str] = &["vols/bigfile.txt", "vols/smallfile.txt"];
const VOLS_CRC: &[u32] = &[0x509a_d74c, 0xd08a_1f86];

/// Copies fixture volumes into a temp dir under new names.
fn arrive(files: &[(&str, &str)]) -> TempDir {
    let tmp = TempDir::new().unwrap();
    for (fixture_name, name) in files {
        fs::copy(fixture(fixture_name), tmp.path().join(name)).unwrap();
    }
    tmp
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn assert_vols(dest: &Path) {
    assert_eq!(tree(dest), VOLS);
    for (name, crc) in VOLS.iter().zip(VOLS_CRC) {
        assert_eq!(crc32(&fs::read(dest.join(name)).unwrap()), *crc, "{name}");
    }
}

fn notes(outcome: &unpacker::repair::Outcome) -> String {
    outcome.notes.join("\n")
}

// ---------------------------------------------------------------------------
// The Telegram case
// ---------------------------------------------------------------------------

#[test]
fn identical_names_with_counters_in_download_order() {
    // Downloaded as part 2, part 3, part 1: the counters follow download
    // order, not part order.
    let tmp = arrive(&[
        ("rar/rar5-vols.part2.rar", &format!("{T}.rar")),
        ("rar/rar5-vols.part3.rar", &format!("{T} (1).rar")),
        ("rar/rar5-vols.part1.rar", &format!("{T} (2).rar")),
    ]);

    let outcome = fix_volume_names(tmp.path());

    assert_eq!(outcome.renamed.len(), 3, "{}", notes(&outcome));
    assert_eq!(
        names_in(tmp.path()),
        [
            format!("{T}.part01.rar"),
            format!("{T}.part02.rar"),
            format!("{T}.part03.rar")
        ]
    );
    assert!(
        notes(&outcome).contains("Restored the part numbers"),
        "{}",
        notes(&outcome)
    );

    let (result, _) = extract(
        &tmp.path().join(format!("{T}.part02.rar")),
        &tmp.path().join("out"),
        None,
    );
    result.unwrap();
    assert_vols(&tmp.path().join("out"));
}

#[test]
fn counters_without_a_space_as_named_by_hand() {
    // Jeffrey's real test: parts saved by hand as `T(1).rar`, `T(2).rar`, …
    // in chat order, which isn't part order.
    let tmp = arrive(&[
        ("rar/rar5-vols.part2.rar", &format!("{T}(1).rar")),
        ("rar/rar5-vols.part3.rar", &format!("{T}(2).rar")),
        ("rar/rar5-vols.part1.rar", &format!("{T}(3).rar")),
    ]);
    fix_volume_names(tmp.path());
    assert_eq!(
        names_in(tmp.path()),
        [
            format!("{T}.part01.rar"),
            format!("{T}.part02.rar"),
            format!("{T}.part03.rar")
        ]
    );
    let (result, _) = extract(
        &tmp.path().join(format!("{T}.part01.rar")),
        &tmp.path().join("out"),
        None,
    );
    result.unwrap();
    assert_vols(&tmp.path().join("out"));
}

#[test]
fn half_renamed_folder_from_the_first_test_build_is_repaired() {
    // What the first test build left behind: each file renamed on its own,
    // counters kept, and part 1 not renamed at all.
    let tmp = arrive(&[
        ("rar/rar5-vols.part2.rar", &format!("{T}(1).part02.rar")),
        ("rar/rar5-vols.part3.rar", &format!("{T}(2).part03.rar")),
        ("rar/rar5-vols.part1.rar", &format!("{T}(3).rar")),
    ]);
    fix_volume_names(tmp.path());
    assert_eq!(
        names_in(tmp.path()),
        [
            format!("{T}.part01.rar"),
            format!("{T}.part02.rar"),
            format!("{T}.part03.rar")
        ]
    );
}

#[test]
fn a_lone_first_part_gets_its_number_before_the_rest_arrives() {
    // Looks like a normal single archive by name, but its header says
    // "volume 1 of a .partNN set".
    let tmp = arrive(&[("rar/rar5-vols.part1.rar", &format!("{T}.rar"))]);
    fix_volume_names(tmp.path());
    assert_eq!(names_in(tmp.path()), [format!("{T}.part01.rar")]);
}

#[test]
fn rar4_volumes_too() {
    let tmp = arrive(&[
        ("rar/rar3-vols.part3.rar", &format!("{T}.rar")),
        ("rar/rar3-vols.part1.rar", &format!("{T} (1).rar")),
        ("rar/rar3-vols.part2.rar", &format!("{T} (2).rar")),
    ]);
    fix_volume_names(tmp.path());
    let (result, _) = extract(
        &tmp.path().join(format!("{T}.part01.rar")),
        &tmp.path().join("out"),
        None,
    );
    result.unwrap();
    assert_vols(&tmp.path().join("out"));
}

#[test]
fn names_that_lie_are_corrected_from_the_headers() {
    // With a length limit, "T.part10" can be cut to "T.part1", clashing
    // with the real part 1. Here every name states the wrong part.
    let tmp = arrive(&[
        ("rar/rar5-vols.part3.rar", "T.part1.rar"),
        ("rar/rar5-vols.part1.rar", "T.part1 (1).rar"),
        ("rar/rar5-vols.part2.rar", "T.pa.rar"),
    ]);
    fix_volume_names(tmp.path());
    assert_eq!(
        names_in(tmp.path()),
        ["T.part01.rar", "T.part02.rar", "T.part03.rar"]
    );
    let (result, _) = extract(
        &tmp.path().join("T.part01.rar"),
        &tmp.path().join("out"),
        None,
    );
    result.unwrap();
    assert_vols(&tmp.path().join("out"));
}

#[test]
fn old_style_sets_keep_old_style_names() {
    let tmp = arrive(&[
        ("rar/rar3-old.r01", "Old.rar"),
        ("rar/rar3-old.rar", "Old (1).rar"),
        ("rar/rar3-old.r00", "Old (2).rar"),
    ]);
    fix_volume_names(tmp.path());
    assert_eq!(names_in(tmp.path()), ["Old.r00", "Old.r01", "Old.rar"]);
    let (result, _) = extract(&tmp.path().join("Old.rar"), &tmp.path().join("out"), None);
    result.unwrap();
    assert_vols(&tmp.path().join("out"));
}

// ---------------------------------------------------------------------------
// Duplicates, missing parts, conflicts
// ---------------------------------------------------------------------------

#[test]
fn a_redownloaded_copy_is_recognised_and_left_alone() {
    let tmp = arrive(&[
        ("rar/rar5-vols.part1.rar", &format!("{T}.rar")),
        ("rar/rar5-vols.part2.rar", &format!("{T} (1).rar")),
        ("rar/rar5-vols.part2.rar", &format!("{T} (2).rar")), // "redownload?" → yes
        ("rar/rar5-vols.part3.rar", &format!("{T} (3).rar")),
    ]);

    let outcome = fix_volume_names(tmp.path());

    assert!(
        notes(&outcome).contains("second copy of part 2"),
        "{}",
        notes(&outcome)
    );
    assert_eq!(outcome.renamed.len(), 3);
    assert!(
        tmp.path().join(format!("{T} (2).rar")).exists(),
        "the copy stays as it is"
    );
    let (result, _) = extract(
        &tmp.path().join(format!("{T}.part01.rar")),
        &tmp.path().join("out"),
        None,
    );
    result.unwrap();
    assert_vols(&tmp.path().join("out"));
}

#[test]
fn a_missing_middle_part_is_named_in_chat_terms() {
    let tmp = arrive(&[
        ("rar/rar5-vols.part1.rar", &format!("{T}.rar")),
        ("rar/rar5-vols.part3.rar", &format!("{T} (1).rar")),
    ]);

    let outcome = fix_volume_names(tmp.path());

    assert!(
        notes(&outcome).contains("Part 2 of 3 is missing"),
        "{}",
        notes(&outcome)
    );
    assert_eq!(
        names_in(tmp.path()),
        [format!("{T}.part01.rar"), format!("{T}.part03.rar")]
    );
    let (result, _) = extract(
        &tmp.path().join(format!("{T}.part01.rar")),
        &tmp.path().join("out"),
        None,
    );
    match result {
        Err(Error::MissingVolume(name)) => assert_eq!(name, format!("{T}.part02.rar")),
        other => panic!("expected MissingVolume, got {other:?}"),
    }
}

#[test]
fn an_overwritten_first_part_is_asked_for() {
    // Telegram replaced part 1 with part 2; part 3 came later.
    let tmp = arrive(&[
        ("rar/rar5-vols.part2.rar", &format!("{T}.rar")),
        ("rar/rar5-vols.part3.rar", &format!("{T} (1).rar")),
    ]);
    let outcome = fix_volume_names(tmp.path());
    assert!(
        notes(&outcome).contains("Part 1 of 3 is missing"),
        "{}",
        notes(&outcome)
    );
    assert_eq!(
        names_in(tmp.path()),
        [format!("{T}.part02.rar"), format!("{T}.part03.rar")]
    );
}

/// Whether `path` holds exactly the fixture `name`.
fn is(path: impl AsRef<Path>, name: &str) -> bool {
    fs::read(path.as_ref()).unwrap() == fs::read(fixture(name)).unwrap()
}

const RAR5: [&str; 3] = [
    "rar/rar5-vols.part1.rar",
    "rar/rar5-vols.part2.rar",
    "rar/rar5-vols.part3.rar",
];
const RAR4: [&str; 3] = [
    "rar/rar3-vols.part1.rar",
    "rar/rar3-vols.part2.rar",
    "rar/rar3-vols.part3.rar",
];

#[test]
fn two_archives_with_the_same_title_are_told_apart_by_content() {
    // E.g. a 1080p and a 720p release, both shortened to the same title and
    // downloaded mixed up. Inside, both hold a file with the same name and
    // size; only the date in the headers differs.
    let tmp = arrive(&[
        (RAR5[1], "T.rar"),
        (RAR4[0], "T (1).rar"),
        (RAR5[2], "T (2).rar"),
        (RAR4[2], "T (3).rar"),
        (RAR5[0], "T (4).rar"),
        (RAR4[1], "T (5).rar"),
    ]);

    let outcome = fix_volume_names(tmp.path());

    assert_eq!(outcome.renamed.len(), 6, "{}", notes(&outcome));
    let d = tmp.path();
    let first = if is(d.join("T.part01.rar"), RAR5[0]) {
        RAR5
    } else {
        RAR4
    };
    let second = if first == RAR5 { RAR4 } else { RAR5 };
    for n in 0..3 {
        assert!(is(d.join(format!("T.part0{}.rar", n + 1)), first[n]));
        assert!(is(d.join(format!("T (2).part0{}.rar", n + 1)), second[n]));
    }
    // Both unpack, as two separate archives.
    for name in ["T.part01.rar", "T (2).part01.rar"] {
        let out = TempDir::new().unwrap();
        let (result, _) = extract(&d.join(name), out.path(), None);
        result.unwrap();
        assert_vols(out.path());
    }
    // And a second run leaves everything as it is.
    let again = fix_volume_names(d);
    assert!(again.renamed.is_empty(), "{}", notes(&again));
}

#[test]
fn any_download_order_of_two_mixed_archives_comes_out_right() {
    let all: Vec<&str> = RAR5.iter().chain(&RAR4).copied().collect();
    let mut seed = 0x2545_f491_u32;
    for round in 0..40 {
        // A shuffled download order, named the way Telegram's counters do.
        let mut order = all.clone();
        for i in (1..order.len()).rev() {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            order.swap(i, (seed >> 16) as usize % (i + 1));
        }
        let names: Vec<String> = (0..order.len())
            .map(|i| {
                if i == 0 {
                    "T.rar".into()
                } else {
                    format!("T ({i}).rar")
                }
            })
            .collect();
        let files: Vec<(&str, &str)> = order
            .iter()
            .copied()
            .zip(names.iter().map(String::as_str))
            .collect();
        let tmp = arrive(&files);

        let outcome = fix_volume_names(tmp.path());

        let d = tmp.path();
        let first = if is(d.join("T.part01.rar"), RAR5[0]) {
            RAR5
        } else {
            RAR4
        };
        let second = if first == RAR5 { RAR4 } else { RAR5 };
        for n in 0..3 {
            let ok = is(d.join(format!("T.part0{}.rar", n + 1)), first[n])
                && is(d.join(format!("T (2).part0{}.rar", n + 1)), second[n]);
            assert!(ok, "round {round}, order {order:?}\n{}", notes(&outcome));
        }
    }
}

#[test]
fn a_part_from_another_archive_is_kept_apart() {
    let tmp = arrive(&[
        (RAR5[0], "T.rar"),
        (RAR4[1], "T (1).rar"), // part 2, but of the other archive
        (RAR5[2], "T (2).rar"),
    ]);

    let outcome = fix_volume_names(tmp.path());
    let n = notes(&outcome);

    let d = tmp.path();
    assert!(is(d.join("T.part01.rar"), RAR5[0]), "{n}");
    assert!(is(d.join("T.part03.rar"), RAR5[2]), "{n}");
    assert!(is(d.join("T (2).part02.rar"), RAR4[1]), "{n}");
    assert!(n.contains("Part 2 of 3 is missing from \"T\""), "{n}");
    assert!(n.contains("from \"T (2)\""), "{n}");
}

#[test]
fn an_archive_arriving_later_does_not_take_the_name_of_one_already_there() {
    let tmp = arrive(&[
        (RAR4[0], "T.part1.rar"),
        (RAR4[1], "T.part2.rar"),
        (RAR4[2], "T.part3.rar"),
        (RAR5[0], "T.rar"),
        (RAR5[1], "T (1).rar"),
        (RAR5[2], "T (2).rar"),
    ]);

    let outcome = fix_volume_names(tmp.path());

    let d = tmp.path();
    for n in 0..3 {
        assert!(
            is(d.join(format!("T.part{}.rar", n + 1)), RAR4[n]),
            "{}",
            notes(&outcome)
        );
        assert!(
            is(d.join(format!("T (2).part0{}.rar", n + 1)), RAR5[n]),
            "{}",
            notes(&outcome)
        );
    }
}

#[test]
fn a_part_that_is_still_downloading_keeps_its_name() {
    let tmp = arrive(&[(RAR5[0], "T.rar"), (RAR5[2], "T (2).rar")]);
    // Part 2 has only arrived halfway.
    let bytes = fs::read(fixture(RAR5[1])).unwrap();
    fs::write(tmp.path().join("T (1).rar"), &bytes[..bytes.len() / 2]).unwrap();

    let outcome = fix_volume_names(tmp.path());
    let n = notes(&outcome);

    assert_eq!(
        names_in(tmp.path()),
        ["T (1).rar", "T.part01.rar", "T.part03.rar"],
        "{n}"
    );
    assert!(
        n.contains("Part 2 of \"T\" (\"T (1).rar\") isn't complete"),
        "{n}"
    );

    // Once it's complete, the next run finishes the job.
    fs::write(tmp.path().join("T (1).rar"), &bytes).unwrap();
    let outcome = fix_volume_names(tmp.path());
    assert_eq!(
        names_in(tmp.path()),
        ["T.part01.rar", "T.part02.rar", "T.part03.rar"],
        "{}",
        notes(&outcome)
    );
}

#[test]
fn a_rename_cut_short_by_a_crash_is_finished() {
    // A crash between the two rename steps leaves a parked file behind.
    let tmp = arrive(&[
        (RAR5[0], "T.rar"),
        (RAR5[1], "T (1).rar.unpacker-renaming"),
        (RAR5[2], "T (2).rar.unpacker-renaming3"),
    ]);

    let outcome = fix_volume_names(tmp.path());

    assert_eq!(
        names_in(tmp.path()),
        ["T.part01.rar", "T.part02.rar", "T.part03.rar"],
        "{}",
        notes(&outcome)
    );
    assert!(
        notes(&outcome).contains("T (1).rar → T.part02.rar"),
        "{}",
        notes(&outcome)
    );
}

#[test]
fn unprovable_parts_rename_nothing() {
    // Two archives' first parts, and a part 2 cut off so early that its
    // headers can't say which file it continues: it could be either's.
    let bytes = fs::read(fixture(RAR5[1])).unwrap();
    let cut = (8..bytes.len())
        .find(|&n| {
            matches!(
                unpacker::rar_volume::identify_reader(&mut std::io::Cursor::new(&bytes[..n])),
                unpacker::rar_volume::Identity::Volume(v)
                    if v.head == unpacker::rar_volume::Edge::Unknown
            )
        })
        .unwrap();
    let tmp = arrive(&[(RAR5[0], "T.rar"), (RAR4[0], "T (1).rar")]);
    fs::write(tmp.path().join("T (2).rar"), &bytes[..cut]).unwrap();
    let before = names_in(tmp.path());

    let outcome = fix_volume_names(tmp.path());

    assert!(outcome.renamed.is_empty());
    assert!(
        notes(&outcome).contains("can't tell which archive part 2 belongs to"),
        "{}",
        notes(&outcome)
    );
    assert_eq!(names_in(tmp.path()), before);
}

#[test]
fn renaming_is_all_or_nothing() {
    let tmp = arrive(&[
        (RAR5[0], "T.rar"),
        (RAR5[1], "T (1).rar"),
        (RAR5[2], "T (2).rar"),
    ]);
    // Something else already sits where part 3 should go.
    fs::write(tmp.path().join("T.part03.rar"), "not ours").unwrap();
    let before = names_in(tmp.path());

    let outcome = fix_volume_names(tmp.path());

    assert!(outcome.renamed.is_empty());
    assert!(
        notes(&outcome).contains("Nothing was renamed"),
        "{}",
        notes(&outcome)
    );
    // Parts 1 and 2 were renamed and then put back.
    assert_eq!(names_in(tmp.path()), before);
    assert_eq!(read(tmp.path().join("T.part03.rar")), "not ours");
}

#[test]
fn never_overwrites_a_file_that_is_in_the_way() {
    let tmp = arrive(&[
        ("rar/rar5-vols.part2.rar", "T.rar"),
        ("rar/rar5-vols.part1.rar", "T (1).rar"),
    ]);
    fs::write(tmp.path().join("T.part01.rar"), "not ours").unwrap();

    let outcome = fix_volume_names(tmp.path());

    assert!(outcome.renamed.is_empty());
    assert!(
        notes(&outcome).contains("already exists"),
        "{}",
        notes(&outcome)
    );
    assert_eq!(read(tmp.path().join("T.part01.rar")), "not ours");
}

#[test]
fn correctly_named_sets_are_never_touched() {
    let tmp = copy_fixtures(&[
        "rar/rar5-vols.part1.rar",
        "rar/rar5-vols.part2.rar",
        "rar/rar5-vols.part3.rar",
        "rar/rar3-old.rar",
        "rar/rar3-old.r00",
        "rar/rar3-old.r01",
        "rar/rar5-solid.rar",
    ]);
    // Even with a part missing, correct names stay as they are.
    fs::remove_file(tmp.path().join("rar5-vols.part2.rar")).unwrap();
    let before = names_in(tmp.path());

    let outcome = fix_volume_names(tmp.path());

    assert!(outcome.renamed.is_empty(), "{:?}", outcome.renamed);
    assert_eq!(names_in(tmp.path()), before);
    assert!(
        notes(&outcome).contains("Part 2 of 3 is missing"),
        "{}",
        notes(&outcome)
    );
}

// ---------------------------------------------------------------------------
// .001 splits that lost their `.7z`
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// The real binary
// ---------------------------------------------------------------------------

fn run(dir: &Path, args: &[&str]) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_unpacker"))
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"\n").unwrap();
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(output.status.success(), "{stdout}");
    stdout
}

#[test]
fn binary_drag_and_drop_of_any_misnamed_part() {
    let tmp = arrive(&[
        ("rar/rar5-vols.part2.rar", &format!("{T}.rar")),
        ("rar/rar5-vols.part3.rar", &format!("{T} (1).rar")),
        ("rar/rar5-vols.part1.rar", &format!("{T} (2).rar")),
    ]);

    // The user drops whatever file they see; it gets renamed under them.
    let out = run(tmp.path(), &["--keep", &format!("{T} (1).rar")]);

    assert!(out.contains("Restored the part numbers"), "{out}");
    assert!(out.contains("(3 parts)"), "{out}");
    // One top-level folder ("vols") → its contents go straight into "T".
    assert!(tmp.path().join(T).join("bigfile.txt").exists(), "{out}");
}

#[test]
fn binary_double_click_counts_one_archive_not_three() {
    let tmp = arrive(&[
        ("rar/rar5-vols.part1.rar", &format!("{T}.rar")),
        ("rar/rar5-vols.part2.rar", &format!("{T} (1).rar")),
        ("rar/rar5-vols.part3.rar", &format!("{T} (2).rar")),
    ]);

    let out = run(tmp.path(), &["--keep"]);

    assert!(out.contains("Found 1 archive(s)"), "{out}");
    assert!(tmp.path().join(T).join("bigfile.txt").exists(), "{out}");
}
