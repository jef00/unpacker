//! Volume discovery: given the one file a user dropped, work out which files
//! make up the archive and in which order they must be read.
//!
//! Everything here is pure string logic except [`discover_on_disk`], which
//! only lists a directory and hands off to [`discover`]. That keeps the
//! naming rules — the most bug-prone part of any unpacker — cheap to test.
//!
//! Supported naming schemes:
//!
//! | Scheme        | Files                                          | Read order          |
//! |---------------|------------------------------------------------|---------------------|
//! | `RarNew`      | `x.part1.rar`, `x.part2.rar`, …                | by part number      |
//! | `RarOld`      | `x.rar`, `x.r00` … `x.r99`, `x.s00`, …         | `.rar` first        |
//! | `Split`       | `x.7z.001`, `x.zip.001`, `x.rar.001`, …        | by number           |
//! | `ZipSpanned`  | `x.z01`, `x.z02`, …, `x.zip`                   | `.zip` **last**     |
//!
//! Limitation: from filenames alone we can't tell whether *trailing* parts
//! are missing (part 5 of 5 vs. part 5 of 7). The backend detects that when
//! it runs off the end of the last volume.

use std::collections::{BTreeMap, HashSet};
use std::iter;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    /// One self-contained file.
    Single,
    /// `x.part1.rar`, `x.part2.rar`, … (WinRAR default since 3.0).
    RarNew,
    /// `x.rar`, `x.r00`, `x.r01`, … (classic scene style).
    RarOld,
    /// `x.7z.001`, `x.tar.gz.001`, … — byte splits of a 7z, ZIP, RAR or tar archive.
    /// Splits of anything else (`movie.mkv.001`) are not volume sets.
    Split,
    /// `x.z01`, `x.z02`, …, `x.zip` — PKZIP spanned archive.
    ZipSpanned,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeSet {
    pub scheme: Scheme,
    /// Name without the volume suffix: `movie` for `movie.part1.rar`,
    /// `movie.7z` for `movie.7z.001`.
    pub base: String,
    /// Every part in read order. Never empty; `parts[0]` is the one to open.
    pub parts: Vec<PathBuf>,
}

impl VolumeSet {
    /// The file a backend should open first.
    pub fn first(&self) -> &Path {
        &self.parts[0]
    }

    /// The name the part *after* the last one we have would get, for
    /// "part X is missing" messages when an archive turns out to continue.
    /// `None` when the set can't continue (single files, spanned ZIPs).
    pub fn next_part_name(&self) -> Option<String> {
        let last = self.parts.last().and_then(|p| file_name(p))?;
        let piece = classify(last)?;
        let base = piece.base;
        let width = piece.digits;
        match (self.scheme, piece.role) {
            (Scheme::RarNew, Role::Numbered(n)) => {
                Some(format!("{base}.part{:0width$}.rar", n + 1))
            }
            (Scheme::RarOld, Role::Head) => Some(rar_old_name(base, 0)),
            (Scheme::RarOld, Role::Numbered(i)) => Some(rar_old_name(base, i + 1)),
            (Scheme::Split, Role::Numbered(n)) => Some(format!("{base}.{:0width$}", n + 1)),
            _ => None,
        }
    }

    /// A sensible name for the output folder.
    ///
    /// Only strips archive extensions we know about, because release names
    /// are full of dots (`Some.Show.S01E01`) that are *not* extensions.
    pub fn output_name(&self) -> &str {
        match self.scheme {
            // `movie.7z` (from `movie.7z.001`), `movie.tar.gz` → `movie`.
            Scheme::Split => without_archive_suffix(&self.base),
            // `base` already lost one extension: `movie.tar.gz` → `movie.tar`.
            // Telegram may have cut that `.tar` short: `movie.ta.gz`.
            Scheme::Single => {
                let compressed = self
                    .parts
                    .first()
                    .and_then(|p| file_name(p))
                    .is_some_and(has_tar_compression);
                let tar: &[&str] = if compressed {
                    &[".tar", ".ta", ".t"]
                } else {
                    &[".tar"]
                };
                tar.iter()
                    .find_map(|suffix| strip_suffix_ci(&self.base, suffix))
                    .filter(|stem| !stem.is_empty())
                    .unwrap_or(&self.base)
            }
            _ => &self.base,
        }
    }
}

/// Lists the dropped file's directory and runs [`discover`] on it.
pub fn discover_on_disk(dropped: &Path) -> Result<VolumeSet> {
    let dir = match dropped.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let mut siblings = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_file() {
            siblings.push(path);
        }
    }
    let set = discover(dropped, &siblings)?;
    // `Title.001`: Telegram may have cut the `.7z` out of `Title.7z.001`.
    // If the first part's content is an archive, it's a split after all.
    if set.scheme == Scheme::Single
        && let Some(stem) = split_stem_by_content(dropped, &siblings)
    {
        return discover_with(dropped, &siblings, Some(&stem));
    }
    Ok(set)
}

/// For `x.NNN` whose `x.001` sibling (or itself) starts with an archive
/// signature: the stem `x`. A raw split video (`movie.mkv.001`) isn't an
/// archive inside, so it stays a single, unsupported file.
fn split_stem_by_content(dropped: &Path, siblings: &[PathBuf]) -> Option<String> {
    let (stem, ext) = file_name(dropped)?.rsplit_once('.')?;
    parse_number(ext).filter(|&n| n >= 1 && ext.len() >= 3)?;
    let first = siblings
        .iter()
        .map(PathBuf::as_path)
        .chain(iter::once(dropped))
        .find(|p| {
            file_name(p)
                .and_then(|n| n.rsplit_once('.'))
                .is_some_and(|(s, e)| {
                    s.eq_ignore_ascii_case(stem) && e.len() >= 3 && parse_number(e) == Some(1)
                })
        })?;
    crate::detect::from_file(first)
        .ok()
        .map(|_| stem.to_string())
}

/// [`archives_among`], plus archives Telegram's name cutting hid, found by
/// content. Reads the start of those files:
///
/// - `.001` splits whose `.7z`/`.zip`/… was cut (see [`discover_on_disk`]);
/// - compressed tars whose `.tar` was cut: `Title.tar.gz` → `Title.ta.gz`
///   or `Title.gz`. A `.gz` of anything else (`notes.txt.gz`) isn't one.
pub fn archives_among_on_disk(files: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = archives_among(files);
    for path in files {
        let Some(name) = file_name(path) else {
            continue;
        };
        if out.contains(path) {
            continue;
        }
        let is_first_split = name.rsplit_once('.').is_some_and(|(stem, ext)| {
            !stem.is_empty() && ext.len() >= 3 && parse_number(ext) == Some(1)
        });
        let found = if is_first_split {
            classify(name).is_none() && crate::detect::from_file(path).is_ok()
        } else {
            has_tar_compression(name) && crate::backend::holds_tar(path)
        };
        if found {
            out.push(path.clone());
        }
    }
    out.sort();
    out
}

/// Works out the full volume set for `dropped`.
///
/// `siblings` is every file in the same directory; it may or may not contain
/// `dropped` itself. No filesystem access happens here.
pub fn discover(dropped: &Path, siblings: &[PathBuf]) -> Result<VolumeSet> {
    discover_with(dropped, siblings, None)
}

/// [`discover`], also treating `<split_stem>.NNN` as a split even without an
/// archive extension inside (the content was checked by the caller).
fn discover_with(
    dropped: &Path,
    siblings: &[PathBuf],
    split_stem: Option<&str>,
) -> Result<VolumeSet> {
    let Some(me) = file_name(dropped).and_then(|n| classify_with(n, split_stem)) else {
        return Ok(single(dropped));
    };

    let mut head: Option<&Path> = None;
    let mut numbered: BTreeMap<u32, &Path> = BTreeMap::new();
    // Zero-padding width for naming a missing part, learned from the set
    // itself: `x.part02.rar` tells us part 1 is `x.part01.rar`.
    let mut width = match me.kind {
        Kind::Split => 3, // 7-Zip default
        _ => 1,
    };

    // `dropped` goes last so a sibling's path (from read_dir) wins on duplicates.
    let candidates = siblings
        .iter()
        .map(PathBuf::as_path)
        .chain(iter::once(dropped));
    for path in candidates {
        let Some(piece) = file_name(path).and_then(|n| classify_with(n, split_stem)) else {
            continue;
        };
        if piece.kind != me.kind || !same_name(piece.base, me.base) {
            continue;
        }
        if piece.zero_padded {
            width = width.max(piece.digits);
        }
        let existing = match piece.role {
            Role::Head => *head.get_or_insert(path),
            Role::Numbered(n) => *numbered.entry(n).or_insert(path),
        };
        ensure_same_file(existing, path)?;
    }

    let base = me.base;
    let parts: Vec<PathBuf> = match me.kind {
        Kind::RarNew => in_order(&numbered, 1, |n| format!("{base}.part{n:0width$}.rar"))?,

        Kind::Split => in_order(&numbered, 1, |n| format!("{base}.{n:0width$}"))?,

        Kind::RarOld => {
            if numbered.is_empty() {
                return Ok(single(dropped)); // just a normal .rar
            }
            let head = head.ok_or_else(|| Error::MissingVolume(format!("{base}.rar")))?;
            let tail = in_order(&numbered, 0, |i| rar_old_name(base, i))?;
            iter::once(head.to_path_buf()).chain(tail).collect()
        }

        Kind::ZipSpanned => {
            if numbered.is_empty() {
                return Ok(single(dropped)); // just a normal .zip
            }
            let head = head.ok_or_else(|| Error::MissingVolume(format!("{base}.zip")))?;
            let body = in_order(&numbered, 1, |n| format!("{base}.z{n:02}"))?;
            body.into_iter()
                .chain(iter::once(head.to_path_buf()))
                .collect()
        }
    };

    Ok(VolumeSet {
        scheme: me.kind.into(),
        base: base.to_string(),
        parts,
    })
}

/// Picks one file per archive among `files` (a directory listing), so a
/// folder can be unpacked without dropping anything onto the program.
///
/// Anything that isn't named like an archive or an archive part is left out
/// (videos, our own `.unpacker-tmp` files, the program itself). Pass each
/// result to [`discover_on_disk`] to get the full set, or the error saying
/// which part is missing. The result is sorted, so archives are unpacked in
/// a predictable order.
pub fn archives_among(files: &[PathBuf]) -> Vec<PathBuf> {
    let mut sorted: Vec<&PathBuf> = files.iter().collect();
    sorted.sort();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for path in sorted {
        let Some(name) = file_name(path) else {
            continue;
        };
        let key = match classify(name) {
            Some(piece) => format!("{:?}/{}", piece.kind, piece.base.to_lowercase()),
            None if is_archive_name(name) => name.to_lowercase(),
            None => continue,
        };
        if seen.insert(key) {
            out.push(path.clone());
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Filename classification
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    RarNew,
    RarOld,
    Split,
    ZipSpanned,
}

impl From<Kind> for Scheme {
    fn from(kind: Kind) -> Self {
        match kind {
            Kind::RarNew => Scheme::RarNew,
            Kind::RarOld => Scheme::RarOld,
            Kind::Split => Scheme::Split,
            Kind::ZipSpanned => Scheme::ZipSpanned,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// The un-numbered file of the set: `x.rar` (old RAR) or `x.zip` (spanned).
    Head,
    /// A numbered part. For `RarOld`, `.r00` is 0, `.r99` is 99, `.s00` is 100.
    Numbered(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Piece<'a> {
    kind: Kind,
    base: &'a str,
    role: Role,
    /// How many digits the number was written with (`part01` → 2).
    digits: usize,
    /// Whether it had leading zeros, i.e. `digits` reflects deliberate padding.
    zero_padded: bool,
}

fn classify(name: &str) -> Option<Piece<'_>> {
    classify_with(name, None)
}

/// `split_stem`: a stem known (from content) to be a split archive.
fn classify_with<'a>(name: &'a str, split_stem: Option<&str>) -> Option<Piece<'a>> {
    let (stem, ext) = name.rsplit_once('.')?;
    if stem.is_empty() {
        return None;
    }
    let head = |kind, base| {
        Some(Piece {
            kind,
            base,
            role: Role::Head,
            digits: 0,
            zero_padded: false,
        })
    };
    let numbered = |kind, base, n, digits: &str| {
        Some(Piece {
            kind,
            base,
            role: Role::Numbered(n),
            digits: digits.len(),
            zero_padded: digits.len() > 1 && digits.starts_with('0'),
        })
    };

    if ext.eq_ignore_ascii_case("rar") {
        // `x.part3.rar` → new style; anything else ending in .rar → old-style head.
        if let Some((base, part)) = stem.rsplit_once('.')
            && let Some(digits) = strip_prefix_ci(part, "part")
            && let Some(n) = parse_number(digits).filter(|&n| n >= 1)
            && !base.is_empty()
        {
            return numbered(Kind::RarNew, base, n, digits);
        }
        return head(Kind::RarOld, stem);
    }

    if ext.eq_ignore_ascii_case("zip") {
        return head(Kind::ZipSpanned, stem);
    }

    let mut chars = ext.chars();
    let letter = chars.next()?.to_ascii_lowercase();
    let rest = chars.as_str();

    // `.z01`, `.z02`, … `.z100` — spanned ZIP body.
    if letter == 'z'
        && rest.len() >= 2
        && let Some(n) = parse_number(rest).filter(|&n| n >= 1)
    {
        return numbered(Kind::ZipSpanned, stem, n, rest);
    }

    // `.r00` … `.r99`, then `.s00` … — old-style RAR body.
    if ('r'..='y').contains(&letter)
        && rest.len() == 2
        && let Some(nn) = parse_number(rest)
    {
        let index = (letter as u32 - 'r' as u32) * 100 + nn;
        return numbered(Kind::RarOld, stem, index, rest);
    }

    // `.001`, `.002`, … — byte split of an archive (`x.7z.001`). Splits of
    // anything else, like a raw `movie.mkv.001`, are deliberately not sets.
    if ext.len() >= 3
        && (is_archive_name(stem) || split_stem.is_some_and(|s| s.eq_ignore_ascii_case(stem)))
        && let Some(n) = parse_number(ext).filter(|&n| n >= 1)
    {
        return numbered(Kind::Split, stem, n, ext);
    }

    None
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Collects numbered parts, insisting they run `start, start+1, …` without gaps.
fn in_order(
    numbered: &BTreeMap<u32, &Path>,
    start: u32,
    name_of: impl Fn(u32) -> String,
) -> Result<Vec<PathBuf>> {
    (start..)
        .zip(numbered)
        .map(|(expected, (&n, path))| {
            if n == expected {
                Ok(path.to_path_buf())
            } else {
                Err(Error::MissingVolume(name_of(expected)))
            }
        })
        .collect()
}

/// Errors if two *different* files claim the same slot in the set
/// (e.g. both `x.part1.rar` and `x.part01.rar` exist).
fn ensure_same_file(existing: &Path, candidate: &Path) -> Result<()> {
    if file_name(existing) == file_name(candidate) {
        return Ok(());
    }
    Err(Error::ConflictingVolumes(
        display_name(existing),
        display_name(candidate),
    ))
}

pub(crate) fn rar_old_name(base: &str, index: u32) -> String {
    let letter = char::from_u32('r' as u32 + index / 100).unwrap_or('?');
    format!("{base}.{letter}{:02}", index % 100)
}

pub(crate) fn single(path: &Path) -> VolumeSet {
    let name = file_name(path).unwrap_or_default();
    let base = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    VolumeSet {
        scheme: Scheme::Single,
        base: base.to_string(),
        parts: vec![path.to_path_buf()],
    }
}

fn file_name(path: &Path) -> Option<&str> {
    path.file_name()?.to_str()
}

fn display_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// Windows and macOS filesystems are case-insensitive by default.
fn same_name(a: &str, b: &str) -> bool {
    a == b || a.to_lowercase() == b.to_lowercase()
}

/// Archive types (also when byte-split into `.001`, `.002`, … parts).
const ARCHIVE_EXTENSIONS: [&str; 9] = [
    "7z", "zip", "rar", "tar", "tgz", "tbz", "tbz2", "txz", "tzst",
];

/// Compressions that make an archive only around a tar: `x.tar.gz` is one,
/// a lone `notes.txt.gz` or `syslog.xz` isn't.
const TAR_COMPRESSIONS: [&str; 4] = ["gz", "bz2", "xz", "zst"];

/// `x.gz`, `x.tar.xz`, `x.zst`, …: a compression tar archives use.
fn has_tar_compression(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(stem, ext)| {
        !stem.is_empty() && TAR_COMPRESSIONS.iter().any(|e| ext.eq_ignore_ascii_case(e))
    })
}

/// `movie.7z`, `movie.tar.gz` → true; `movie.mkv`, `log.gz`, `movie` → false.
fn is_archive_name(name: &str) -> bool {
    let Some((stem, ext)) = name.rsplit_once('.') else {
        return false;
    };
    let is = |list: &[&str]| list.iter().any(|e| ext.eq_ignore_ascii_case(e));
    !stem.is_empty()
        && (is(&ARCHIVE_EXTENSIONS)
            || (is(&TAR_COMPRESSIONS)
                && strip_suffix_ci(stem, ".tar").is_some_and(|s| !s.is_empty())))
}

/// `movie.7z` → `movie`, `movie.tar.gz` → `movie`; anything else unchanged.
fn without_archive_suffix(name: &str) -> &str {
    if !is_archive_name(name) {
        return name;
    }
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    strip_suffix_ci(stem, ".tar")
        .filter(|s| !s.is_empty())
        .unwrap_or(stem)
}

fn parse_number(s: &str) -> Option<u32> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &s[prefix.len()..])
}

pub(crate) fn strip_suffix_ci<'a>(s: &'a str, suffix: &str) -> Option<&'a str> {
    let split = s.len().checked_sub(suffix.len())?;
    let tail = s.get(split..)?;
    tail.eq_ignore_ascii_case(suffix).then(|| &s[..split])
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn files(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(|n| PathBuf::from("dl").join(n)).collect()
    }

    fn names(set: &VolumeSet) -> Vec<String> {
        set.parts.iter().map(|p| display_name(p)).collect()
    }

    fn run(dropped: &str, dir: &[&str]) -> Result<VolumeSet> {
        discover(&PathBuf::from("dl").join(dropped), &files(dir))
    }

    fn missing(result: Result<VolumeSet>) -> String {
        match result {
            Err(Error::MissingVolume(name)) => name,
            other => panic!("expected MissingVolume, got {other:?}"),
        }
    }

    // --- finding archives in a folder ----------------------------------------

    #[test]
    fn archives_among_picks_one_file_per_archive() {
        let found = archives_among(&files(&[
            "b.part2.rar",
            "b.part1.rar",
            "movie.mkv",
            "c.7z",
            "a.zip.002",
            "a.zip.001",
            "old.r00",
            "old.rar",
            "span.z01",
            "span.zip",
            "unpacker.exe",
            "x.mkv.unpacker-tmp",
            "video.mkv.001",
        ]));
        let found: Vec<String> = found.iter().map(|p| display_name(p)).collect();
        assert_eq!(
            found,
            ["a.zip.001", "b.part1.rar", "c.7z", "old.r00", "span.z01"]
        );
    }

    #[test]
    fn archives_among_each_finds_the_whole_set() {
        let dir = [
            "b.part1.rar",
            "b.part2.rar",
            "old.rar",
            "old.r00",
            "span.zip",
            "span.z01",
        ];
        for picked in archives_among(&files(&dir)) {
            let set = discover(&picked, &files(&dir)).unwrap();
            assert_eq!(set.parts.len(), 2, "{set:?}");
        }
    }

    // --- single files ------------------------------------------------------

    #[test]
    fn non_archive_is_single() {
        let set = run("movie.mkv", &["movie.mkv"]).unwrap();
        assert_eq!(set.scheme, Scheme::Single);
        assert_eq!(set.base, "movie");
    }

    #[test]
    fn file_without_extension_is_single() {
        let set = run("README", &["README"]).unwrap();
        assert_eq!(set.scheme, Scheme::Single);
        assert_eq!(set.base, "README");
    }

    #[test]
    fn plain_rar_is_single() {
        let set = run("movie.rar", &["movie.rar", "other.r00"]).unwrap();
        assert_eq!(set.scheme, Scheme::Single);
        assert_eq!(names(&set), ["movie.rar"]);
    }

    #[test]
    fn plain_zip_is_single() {
        let set = run("movie.zip", &["movie.zip"]).unwrap();
        assert_eq!(set.scheme, Scheme::Single);
    }

    #[test]
    fn dropped_file_missing_from_listing_still_counts() {
        let set = run("movie.mkv", &[]).unwrap();
        assert_eq!(names(&set), ["movie.mkv"]);
    }

    // --- RAR new style -----------------------------------------------------

    #[test]
    fn rar_new_from_middle_part() {
        let set = run(
            "movie.part2.rar",
            &[
                "movie.part3.rar",
                "movie.part1.rar",
                "movie.part2.rar",
                "notes.txt",
            ],
        )
        .unwrap();
        assert_eq!(set.scheme, Scheme::RarNew);
        assert_eq!(set.base, "movie");
        assert_eq!(
            names(&set),
            ["movie.part1.rar", "movie.part2.rar", "movie.part3.rar"]
        );
    }

    #[test]
    fn rar_new_sorts_numerically_not_alphabetically() {
        let dir: Vec<String> = (1..=12).map(|n| format!("m.part{n}.rar")).collect();
        let dir: Vec<&str> = dir.iter().map(String::as_str).collect();
        let set = run("m.part1.rar", &dir).unwrap();
        assert_eq!(set.parts.len(), 12);
        assert_eq!(display_name(&set.parts[9]), "m.part10.rar");
        assert_eq!(display_name(&set.parts[11]), "m.part12.rar");
    }

    #[test]
    fn rar_new_missing_first_part() {
        let err = run("movie.part3.rar", &["movie.part2.rar", "movie.part3.rar"]);
        assert_eq!(missing(err), "movie.part1.rar");
    }

    #[test]
    fn rar_new_gap_reports_the_gap() {
        let err = run("movie.part1.rar", &["movie.part1.rar", "movie.part3.rar"]);
        assert_eq!(missing(err), "movie.part2.rar");
    }

    #[test]
    fn rar_new_missing_name_keeps_zero_padding() {
        let err = run(
            "movie.part02.rar",
            &["movie.part02.rar", "movie.part03.rar"],
        );
        assert_eq!(missing(err), "movie.part01.rar");
    }

    #[test]
    fn rar_new_with_dotted_release_name() {
        let set = run(
            "Some.Show.S01E01.1080p.part1.rar",
            &[
                "Some.Show.S01E01.1080p.part1.rar",
                "Some.Show.S01E01.1080p.part2.rar",
            ],
        )
        .unwrap();
        assert_eq!(set.base, "Some.Show.S01E01.1080p");
        assert_eq!(set.output_name(), "Some.Show.S01E01.1080p");
    }

    #[test]
    fn rar_new_ignores_other_sets_in_same_folder() {
        let set = run(
            "a.part1.rar",
            &[
                "a.part1.rar",
                "a.part2.rar",
                "b.part1.rar",
                "b.part2.rar",
                "b.part3.rar",
            ],
        )
        .unwrap();
        assert_eq!(names(&set), ["a.part1.rar", "a.part2.rar"]);
    }

    #[test]
    fn rar_new_is_case_insensitive() {
        let set = run("MOVIE.PART1.RAR", &["MOVIE.PART1.RAR", "movie.part2.rar"]).unwrap();
        assert_eq!(set.parts.len(), 2);
    }

    #[test]
    fn rar_new_duplicate_part_numbers_conflict() {
        let err = run(
            "m.part1.rar",
            &["m.part1.rar", "m.part01.rar", "m.part2.rar"],
        );
        assert!(matches!(err, Err(Error::ConflictingVolumes(..))), "{err:?}");
    }

    // --- RAR old style -----------------------------------------------------

    #[test]
    fn rar_old_head_comes_first_when_tail_dropped() {
        let set = run(
            "movie.r01",
            &["movie.r01", "movie.r00", "movie.rar", "movie.r02"],
        )
        .unwrap();
        assert_eq!(set.scheme, Scheme::RarOld);
        assert_eq!(
            names(&set),
            ["movie.rar", "movie.r00", "movie.r01", "movie.r02"]
        );
    }

    #[test]
    fn rar_old_rolls_over_from_r99_to_s00() {
        let mut dir = vec!["m.rar".to_string()];
        dir.extend((0..100).map(|n| format!("m.r{n:02}")));
        dir.extend(["m.s00".to_string(), "m.s01".to_string()]);
        let dir: Vec<&str> = dir.iter().map(String::as_str).collect();

        let set = run("m.s01", &dir).unwrap();
        assert_eq!(set.parts.len(), 103);
        assert_eq!(display_name(&set.parts[100]), "m.r99");
        assert_eq!(display_name(&set.parts[102]), "m.s01");
    }

    #[test]
    fn rar_old_missing_head() {
        let err = run("movie.r00", &["movie.r00", "movie.r01"]);
        assert_eq!(missing(err), "movie.rar");
    }

    #[test]
    fn rar_old_gap_reports_the_gap() {
        let err = run("movie.rar", &["movie.rar", "movie.r00", "movie.r02"]);
        assert_eq!(missing(err), "movie.r01");
    }

    #[test]
    fn rar_old_gap_across_rollover() {
        let mut dir = vec!["m.rar".to_string()];
        dir.extend((0..100).map(|n| format!("m.r{n:02}")));
        dir.push("m.s01".to_string());
        let dir: Vec<&str> = dir.iter().map(String::as_str).collect();
        assert_eq!(missing(run("m.rar", &dir)), "m.s00");
    }

    #[test]
    fn rar_old_and_new_in_same_folder_stay_apart() {
        let set = run("x.rar", &["x.rar", "x.r00", "x.part1.rar", "x.part2.rar"]).unwrap();
        assert_eq!(names(&set), ["x.rar", "x.r00"]);
    }

    // --- Split (.001) ------------------------------------------------------

    #[test]
    fn split_7z_from_last_part() {
        let set = run(
            "movie.7z.003",
            &["movie.7z.001", "movie.7z.002", "movie.7z.003"],
        )
        .unwrap();
        assert_eq!(set.scheme, Scheme::Split);
        assert_eq!(set.base, "movie.7z");
        assert_eq!(set.output_name(), "movie");
        assert_eq!(
            names(&set),
            ["movie.7z.001", "movie.7z.002", "movie.7z.003"]
        );
    }

    #[test]
    fn split_missing_first_part() {
        let err = run("movie.zip.002", &["movie.zip.002"]);
        assert_eq!(missing(err), "movie.zip.001");
    }

    #[test]
    fn split_continues_past_999() {
        let dir: Vec<String> = (1..=1000).map(|n| format!("m.7z.{n:03}")).collect();
        let dir: Vec<&str> = dir.iter().map(String::as_str).collect();
        let set = run("m.7z.1000", &dir).unwrap();
        assert_eq!(set.parts.len(), 1000);
        assert_eq!(display_name(&set.parts[999]), "m.7z.1000");
    }

    #[test]
    fn split_missing_name_uses_7zip_padding() {
        let err = run("m.7z.1000", &["m.7z.999", "m.7z.1000"]);
        assert_eq!(missing(err), "m.7z.001");
    }

    #[test]
    fn rar_new_padding_learned_from_siblings() {
        // Dropping part10 (no leading zero) still names the gap `part01`.
        let dir: Vec<String> = (2..=10).map(|n| format!("m.part{n:02}.rar")).collect();
        let dir: Vec<&str> = dir.iter().map(String::as_str).collect();
        assert_eq!(missing(run("m.part10.rar", &dir)), "m.part01.rar");
    }

    #[test]
    fn rar_new_unpadded_set_names_gap_unpadded() {
        let err = run("m.part3.rar", &["m.part2.rar", "m.part3.rar"]);
        assert_eq!(missing(err), "m.part1.rar");
    }

    #[test]
    fn split_of_a_non_archive_is_not_a_volume_set() {
        for name in ["movie.mkv.001", "data.001", "movie.mkv.002"] {
            let set = run(name, &["movie.mkv.001", "movie.mkv.002", "data.001"]).unwrap();
            assert_eq!(set.scheme, Scheme::Single, "{name}");
            assert_eq!(set.parts.len(), 1, "{name}");
        }
    }

    #[test]
    fn split_inner_extension_is_case_insensitive() {
        let set = run("Movie.7Z.002", &["Movie.7Z.001", "Movie.7Z.002"]).unwrap();
        assert_eq!((set.scheme, set.output_name()), (Scheme::Split, "Movie"));
    }

    #[test]
    fn split_000_is_not_a_volume() {
        let set = run("data.000", &["data.000"]).unwrap();
        assert_eq!(set.scheme, Scheme::Single);
    }

    // --- ZIP spanned -------------------------------------------------------

    #[test]
    fn zip_spanned_head_goes_last() {
        let set = run("movie.zip", &["movie.zip", "movie.z02", "movie.z01"]).unwrap();
        assert_eq!(set.scheme, Scheme::ZipSpanned);
        assert_eq!(names(&set), ["movie.z01", "movie.z02", "movie.zip"]);
    }

    #[test]
    fn zip_spanned_missing_head() {
        let err = run("movie.z01", &["movie.z01", "movie.z02"]);
        assert_eq!(missing(err), "movie.zip");
    }

    #[test]
    fn zip_spanned_gap() {
        let err = run("movie.z01", &["movie.z01", "movie.z03", "movie.zip"]);
        assert_eq!(missing(err), "movie.z02");
    }

    // --- classify edge cases ----------------------------------------------

    #[test]
    fn classify_rejects_weird_names() {
        assert_eq!(classify(".rar"), None);
        assert_eq!(classify("noext"), None);
        assert_eq!(classify("x.r0"), None); // too short
        assert_eq!(classify("x.r000"), None); // too long for old RAR
        assert_eq!(classify("x.z00"), None);
        assert_eq!(classify("x.mkv"), None);
    }

    #[test]
    fn part0_falls_back_to_old_style_head() {
        let p = classify("x.part0.rar").unwrap();
        assert_eq!((p.kind, p.base), (Kind::RarOld, "x.part0"));
    }

    #[test]
    fn next_part_name_follows_each_scheme() {
        let next = |dropped: &str, dir: &[&str]| run(dropped, dir).unwrap().next_part_name();
        assert_eq!(
            next("m.part1.rar", &["m.part1.rar", "m.part2.rar"]),
            Some("m.part3.rar".into())
        );
        assert_eq!(next("m.rar", &["m.rar", "m.r00"]), Some("m.r01".into()));
        assert_eq!(
            next("m.7z.002", &["m.7z.001", "m.7z.002"]),
            Some("m.7z.003".into())
        );
        assert_eq!(next("m.zip", &["m.z01", "m.zip"]), None);
        assert_eq!(next("m.mkv", &["m.mkv"]), None);
    }

    #[test]
    fn next_part_name_keeps_padding_and_rolls_over() {
        let padded: Vec<String> = (1..=9).map(|n| format!("m.part{n:02}.rar")).collect();
        let padded: Vec<&str> = padded.iter().map(String::as_str).collect();
        let set = run("m.part01.rar", &padded).unwrap();
        assert_eq!(set.next_part_name(), Some("m.part10.rar".into()));

        let mut old = vec!["m.rar".to_string()];
        old.extend((0..100).map(|n| format!("m.r{n:02}")));
        let old: Vec<&str> = old.iter().map(String::as_str).collect();
        let set = run("m.rar", &old).unwrap();
        assert_eq!(set.next_part_name(), Some("m.s00".into()));
    }

    #[test]
    fn tar_names() {
        for name in [
            "a.tar",
            "a.tgz",
            "a.tar.gz",
            "a.TAR.XZ",
            "a.tar.bz2",
            "a.tbz2",
            "a.tar.zst",
        ] {
            assert!(is_archive_name(name), "{name}");
        }
        for name in ["log.gz", "syslog.xz", ".tar.gz", "a.mkv.zst", ".tar"] {
            assert!(!is_archive_name(name), "{name}");
        }
    }

    #[test]
    fn tar_output_names() {
        let single = |name: &str| run(name, &[name]).unwrap();
        assert_eq!(single("Show.S01.tar.gz").output_name(), "Show.S01");
        assert_eq!(single("Show.S01.tar").output_name(), "Show.S01");
        assert_eq!(single("Show.S01.tgz").output_name(), "Show.S01");
        assert_eq!(single("Show.S01.mkv").output_name(), "Show.S01");

        let split = run("m.tar.gz.002", &["m.tar.gz.001", "m.tar.gz.002"]).unwrap();
        assert_eq!((split.scheme, split.output_name()), (Scheme::Split, "m"));
        let split = run("m.tar.001", &["m.tar.001", "m.tar.002"]).unwrap();
        assert_eq!((split.scheme, split.output_name()), (Scheme::Split, "m"));
        // Not tar inside: still not a split.
        assert_eq!(
            run("log.gz.001", &["log.gz.001"]).unwrap().scheme,
            Scheme::Single
        );
    }

    #[test]
    fn double_click_picks_tars_but_not_lone_compressed_files() {
        let files = files(&[
            "a.tar.gz",
            "b.tar",
            "c.tar.xz.001",
            "c.tar.xz.002",
            "log.gz",
            "x.mkv",
        ]);
        let names: Vec<String> = archives_among(&files)
            .iter()
            .map(|p| display_name(p))
            .collect();
        assert_eq!(names, ["a.tar.gz", "b.tar", "c.tar.xz.001"]);
    }

    #[test]
    fn output_name_for_split_without_known_ext() {
        let set = VolumeSet {
            scheme: Scheme::Split,
            base: ".7z".into(),
            parts: vec!["dl/.7z.001".into()],
        };
        assert_eq!(set.output_name(), ".7z");
    }
}
