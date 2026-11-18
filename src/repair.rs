//! Puts volume numbers back into RAR volume names that lost them.
//!
//! Telegram keeps a file name's extension but cuts the name before it when
//! it's long. For `.part01.rar` volumes that removes the part number, so a
//! whole set arrives as `Title.rar`, and the downloader's counters make it
//! `Title.rar`, `Title (1).rar`, `Title (2).rar` in *download* order.
//!
//! This module reads each volume's real number from its headers
//! ([`crate::rar_volume`]) and renames the files to `Title.part01.rar`, … so
//! the folder makes sense again: to UnRAR, to WinRAR, and to people (a
//! missing `part03` is visible). Names are never trusted:
//!
//! - Which part a file is comes from its headers, and which *archive* it
//!   belongs to from its contents: a big file is cut across the parts, and
//!   each piece's header names it, so neighbouring parts must agree. Two
//!   different archives with the same shortened title (720p and 1080p, say)
//!   are told apart and become `Title` and `Title (2)`.
//! - A redownloaded copy of a part is recognised and left alone.
//! - A part that isn't complete yet (still downloading) keeps its name.
//! - A set whose names already work is left completely alone.
//! - Renaming is all or nothing per title, and can never overwrite a file,
//!   not even one created at the same moment. A rename cut short by a crash
//!   leaves `*.unpacker-renaming` files, which the next run finishes.
//! - When in doubt (parts that could belong to either archive), nothing is
//!   renamed and the note says why.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::discovery::{self, Scheme, rar_old_name, strip_suffix_ci};
use crate::rar_volume::{self, Edge, Identity, Link, Volume};

/// Suffix of a file parked mid-rename (plus an optional number).
const PARKED: &str = ".unpacker-renaming";

/// What happened in one folder.
#[derive(Debug, Default)]
pub struct Outcome {
    /// `(old path, new path)` for every file renamed.
    pub renamed: Vec<(PathBuf, PathBuf)>,
    /// Lines for the user: what was renamed, what's missing, what was skipped.
    pub notes: Vec<String>,
}

/// Restores volume names in `dir`. Never fails as a whole: problems with
/// one title become notes, and the other titles are still handled.
pub fn fix_volume_names(dir: &Path) -> Outcome {
    let mut outcome = Outcome::default();
    let Ok(entries) = fs::read_dir(dir) else {
        return outcome;
    };
    let files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file())
        .collect();

    // Group `.rar` files by the title they share once counters and part
    // numbers (whole or cut off) are taken away.
    let mut groups: BTreeMap<String, Vec<Member>> = BTreeMap::new();
    for path in &files {
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // A file parked by a rename that was cut short: judge it by the
        // name it had, and move it on to its proper name below.
        let (name, parked) = match unparked(file_name) {
            Some(original) => (original, true),
            None => (file_name, false),
        };
        let Some(title) = shared_title(name) else {
            continue;
        };
        let identity = rar_volume::identify(path);
        if identity == Identity::NotAVolume {
            continue;
        }
        groups
            .entry(title.to_lowercase())
            .or_default()
            .push(Member {
                path: path.clone(),
                name: name.to_string(),
                title: title.to_string(),
                identity,
                parked,
            });
    }

    for members in groups.into_values() {
        fix_title(&members, &files, &mut outcome);
    }
    outcome
}

/// `x.rar.unpacker-renaming` (or `…renaming2`) → `x.rar`.
fn unparked(name: &str) -> Option<&str> {
    let at = name.rfind(PARKED)?;
    name[at + PARKED.len()..]
        .bytes()
        .all(|b| b.is_ascii_digit())
        .then(|| &name[..at])
}

struct Member {
    path: PathBuf,
    /// The file's name (for a parked file, the name it had before).
    name: String,
    /// The shared title, in this file's own spelling.
    title: String,
    identity: Identity,
    /// Left behind by an interrupted rename.
    parked: bool,
}

/// One volume of one archive, from one file.
struct Part<'a> {
    member: &'a Member,
    volume: &'a Volume,
}

/// The parts of one archive, by volume number.
type Chain<'a> = BTreeMap<u32, Part<'a>>;

fn fix_title(members: &[Member], all_files: &[PathBuf], outcome: &mut Outcome) {
    let mut by_number: BTreeMap<u32, Vec<Part<'_>>> = BTreeMap::new();
    let mut unknown = Vec::new();
    for member in members {
        match &member.identity {
            Identity::Volume(volume) => by_number
                .entry(volume.number)
                .or_default()
                .push(Part { member, volume }),
            _ => unknown.push(member),
        }
    }
    if by_number.is_empty() {
        return;
    }
    let title = members
        .iter()
        .find(|m| matches!(&m.identity, Identity::Volume(v) if v.number == 0))
        .unwrap_or(&members[0])
        .title
        .clone();

    // Redownloaded copies are identical files: keep one of each.
    let mut parts = Vec::new();
    let mut duplicates = Vec::new();
    for (number, mut claimants) in by_number {
        // Prefer a file that isn't parked, then the shortest name.
        claimants.sort_by_key(|p| (p.member.parked, p.member.name.len(), p.member.name.clone()));
        let mut kept: Vec<Part<'_>> = Vec::new();
        for part in claimants {
            if kept
                .iter()
                .any(|k| same_contents(&k.member.path, &part.member.path))
            {
                duplicates.push((part.member, number));
            } else {
                kept.push(part);
            }
        }
        parts.extend(kept);
    }

    let mut chains = match chain(parts) {
        Ok(chains) => chains,
        Err(number) => {
            outcome.notes.push(format!(
                "! Found parts of more than one archive called \"{title}\", and can't tell \
                 which archive part {} belongs to. Renamed nothing for \"{title}\".",
                number + 1
            ));
            return;
        }
    };
    order_chains(&mut chains, &title);

    // Plan every rename for this title, then do them together.
    let mut plan = Vec::new();
    let mut chain_notes = Vec::new();
    let mut renamed_titles = Vec::new();
    for (i, chain) in chains.iter().enumerate() {
        let display = chain_title(&title, i);
        let new_numbering = chain
            .values()
            .next()
            .is_some_and(|p| p.volume.new_numbering);
        let total = chain
            .values()
            .find(|p| p.volume.last == Some(true))
            .map(|p| p.volume.number + 1);
        if !already_works(chain, &display, total, new_numbering, all_files) {
            let count = total.unwrap_or_else(|| chain.keys().last().map_or(1, |n| n + 1));
            let before = plan.len();
            for (&number, part) in chain {
                if !part.volume.complete() {
                    continue; // still downloading: renamed once it's done
                }
                let target = part.member.path.with_file_name(volume_name(
                    &display,
                    number,
                    count,
                    new_numbering,
                ));
                if target != part.member.path {
                    plan.push((part.member.path.clone(), target));
                }
            }
            if plan.len() > before {
                renamed_titles.push((display.clone(), before..plan.len()));
            }
        }
        for (&number, part) in chain {
            if !part.volume.complete() {
                chain_notes.push(format!(
                    "  Part {} of \"{display}\" (\"{}\") isn't complete: still downloading, or \
                     the download broke off. Left its name alone; run again once it's done.",
                    number + 1,
                    part.member.name
                ));
            }
        }
        if let Some(note) =
            missing_parts_note(&chain.keys().copied().collect::<Vec<_>>(), total, &display)
        {
            chain_notes.push(note);
        }
    }

    if !plan.is_empty() {
        match rename_all(&plan) {
            Ok(()) => {
                for (display, range) in renamed_titles {
                    outcome.notes.push(format!(
                        "✎ Restored the part numbers of \"{display}\" (Telegram had shortened \
                         the names):"
                    ));
                    for (from, to) in &plan[range] {
                        outcome
                            .notes
                            .push(format!("    {} → {}", shown(from), label(to)));
                    }
                }
                outcome.renamed.extend(plan);
            }
            Err(e) => outcome.notes.push(format!(
                "! Couldn't restore the part numbers of \"{title}\": {e}. Nothing was renamed."
            )),
        }
    }

    for (member, number) in duplicates {
        outcome.notes.push(format!(
            "  \"{}\" is a second copy of part {} (downloaded twice); it isn't needed.",
            label(&member.path),
            number + 1
        ));
    }
    for member in unknown {
        outcome.notes.push(format!(
            "  Couldn't tell which part \"{}\" is (encrypted or damaged); left it as it is.",
            member.name
        ));
    }
    outcome.notes.extend(chain_notes);
}

/// A parked file is shown by the name it had.
fn shown(path: &Path) -> String {
    let name = label(path);
    unparked(&name).map_or_else(|| name.clone(), str::to_string)
}

/// Sorts parts into archives. Each part joins the archive whose last part
/// it continues: right after it, the file cut at that part's end must be
/// the one this part starts with. Across a missing part, the same file
/// running through both is proof enough. Without proof a part may still
/// join, but only when nothing else competes for that place. `Err` holds a
/// volume number that could belong to more than one archive.
fn chain(mut parts: Vec<Part<'_>>) -> Result<Vec<Chain<'_>>, u32> {
    parts.sort_by_key(|p| (p.volume.number, p.member.name.clone()));
    let numbers: Vec<u32> = parts.iter().map(|p| p.volume.number).collect();

    let mut chains: Vec<Chain<'_>> = Vec::new();
    for part in parts {
        let mut strong = Vec::new();
        let mut weak = Vec::new();
        for (i, chain) in chains.iter().enumerate() {
            let Some((&last_number, last)) = chain.iter().next_back() else {
                continue;
            };
            if last.volume.new_numbering != part.volume.new_numbering
                || part.volume.number <= last_number
            {
                continue;
            }
            match fit(last.volume, part.volume) {
                Fit::Proven => strong.push(i),
                Fit::Possible => weak.push(i),
                Fit::No => {}
            }
        }
        let alone = numbers.iter().filter(|&&n| n == part.volume.number).count() == 1;
        let target = match (strong.as_slice(), weak.as_slice()) {
            ([i], _) => Some(*i),
            ([], [i]) if alone => Some(*i),
            ([], []) => None,
            _ => return Err(part.volume.number),
        };
        let number = part.volume.number;
        match target {
            Some(i) => {
                chains[i].insert(number, part);
            }
            // Nothing to continue: two first parts are two archives, and so
            // is a part that contradicts every archive so far.
            None => chains.push(BTreeMap::from([(number, part)])),
        }
    }
    Ok(chains)
}

enum Fit {
    /// The headers prove `part` continues this archive.
    Proven,
    /// Nothing speaks against it.
    Possible,
    /// The headers say it's another archive.
    No,
}

fn fit(last: &Volume, part: &Volume) -> Fit {
    if part.number == last.number + 1 {
        return match rar_volume::link(&last.tail, &part.head) {
            Link::Match => Fit::Proven,
            Link::Unknown => Fit::Possible,
            Link::Mismatch => Fit::No,
        };
    }
    // Parts in between are missing: proof only if one file runs through.
    match (&last.tail, &part.head) {
        (Edge::Split(a), Edge::Split(b)) if a == b => Fit::Proven,
        _ => Fit::Possible,
    }
}

/// The archive that already carries the plain title keeps it, so names
/// don't swap when a second archive arrives later. Then the one with the
/// earliest part, then by contents, so the order is the same every run.
fn order_chains(chains: &mut [Chain<'_>], title: &str) {
    chains.sort_by_cached_key(|chain| {
        let named = chain
            .iter()
            .filter(|(n, p)| {
                let style = p.volume.new_numbering;
                p.member
                    .name
                    .eq_ignore_ascii_case(&volume_name(title, **n, **n + 1, style))
                    || p.member
                        .name
                        .eq_ignore_ascii_case(&volume_name(title, **n, 100, style))
                    || name_number(&p.member.name) == Some(**n)
                        && shared_title(&p.member.name)
                            .is_some_and(|t| t.eq_ignore_ascii_case(title))
            })
            .count();
        let first = chain.keys().next().copied().unwrap_or(0);
        let contents = chain
            .values()
            .find_map(|p| match (&p.volume.head, &p.volume.tail) {
                (Edge::Split(f), _) | (_, Edge::Split(f)) => {
                    Some((f.name.clone(), f.size, f.mtime))
                }
                _ => None,
            });
        let file = chain
            .values()
            .next()
            .map(|p| p.member.name.clone())
            .unwrap_or_default();
        (std::cmp::Reverse(named), first, contents, file)
    });
}

/// `Title` for the first archive, `Title (2)`, `Title (3)` for the others.
fn chain_title(title: &str, index: usize) -> String {
    if index == 0 {
        title.to_string()
    } else {
        format!("{title} ({})", index + 1)
    }
}

/// Whether the names already lead name-based discovery to exactly these
/// files in exactly this order, in the archive's own naming style. Then
/// there's nothing to fix, and nothing is touched, whatever the padding.
fn already_works(
    chain: &Chain<'_>,
    display: &str,
    total: Option<u32>,
    new_numbering: bool,
    all_files: &[PathBuf],
) -> bool {
    if chain.values().any(|p| p.member.parked) {
        return false;
    }
    let ordered: Vec<&Path> = chain.values().map(|p| p.member.path.as_path()).collect();
    let contiguous = chain.keys().copied().eq(0..chain.len() as u32);
    let Some(first) = ordered.first() else {
        return true;
    };
    let titled = chain
        .values()
        .all(|p| shared_title(&p.member.name).is_some_and(|t| t.eq_ignore_ascii_case(display)));
    match discovery::discover(first, all_files) {
        Ok(set) => {
            // Names must also be in the archive's own style: a lone
            // `Title.rar` reads as a normal single archive, but if it's
            // part 1 of a `.partNN` set (the rest still to come), it needs
            // its part number too.
            let right_style = match set.scheme {
                Scheme::RarNew => new_numbering,
                Scheme::RarOld => !new_numbering,
                Scheme::Single => total == Some(1),
                _ => false,
            };
            titled
                && right_style
                && contiguous
                && set.parts.len() == ordered.len()
                && set
                    .parts
                    .iter()
                    .zip(&ordered)
                    .all(|(a, b)| a.as_path() == *b)
        }
        // Discovery naming a missing part from correct names is fine too:
        // every file's name already matches its header.
        Err(_) => {
            titled
                && chain.iter().all(|(&n, p)| {
                    p.member.name.eq_ignore_ascii_case(&volume_name(
                        display,
                        n,
                        n + 1,
                        p.volume.new_numbering,
                    )) || name_number(&p.member.name) == Some(n)
                })
        }
    }
}

/// The part number (0-based) a correct name states, if any: `x.part3.rar` → 2.
fn name_number(name: &str) -> Option<u32> {
    let stem = strip_suffix_ci(name, ".rar")?;
    let (_, tail) = stem.rsplit_once('.')?;
    let digits = tail
        .get(..4)
        .filter(|p| p.eq_ignore_ascii_case("part"))
        .map(|_| &tail[4..])?;
    digits.parse::<u32>().ok()?.checked_sub(1)
}

/// Renames every `(from, to)` or none of them. Files are parked under
/// temporary names first, so swaps and chains (`a → b`, `b → c`) are safe,
/// and every final step refuses to overwrite. If anything fails, everything
/// done so far is undone.
fn rename_all(plan: &[(PathBuf, PathBuf)]) -> Result<(), String> {
    let mut parked: Vec<(&Path, PathBuf, &Path)> = Vec::new();
    let undo_parking = |parked: &[(&Path, PathBuf, &Path)]| {
        for (from, temp, _) in parked.iter().rev() {
            if temp.as_path() != *from {
                let _ = move_no_replace(temp, from);
            }
        }
    };
    for (from, to) in plan {
        // Already parked by an interrupted run: it can move on from there.
        let temp = if is_parked(from) {
            from.clone()
        } else {
            match park(from) {
                Ok(temp) => temp,
                Err(e) => {
                    undo_parking(&parked);
                    return Err(format!("\"{}\" can't be renamed ({e})", label(from)));
                }
            }
        };
        parked.push((from, temp, to));
    }

    let mut done: Vec<(&Path, &Path)> = Vec::new();
    for (_, temp, to) in &parked {
        if let Err(e) = move_no_replace(temp, to) {
            for (temp, to) in done.iter().rev() {
                let _ = move_no_replace(to, temp);
            }
            undo_parking(&parked);
            return Err(if e.kind() == io::ErrorKind::AlreadyExists {
                format!(
                    "\"{}\" already exists and isn't part of this archive",
                    label(to)
                )
            } else {
                format!("\"{}\" can't be created ({e})", label(to))
            });
        }
        done.push((temp, to));
    }
    Ok(())
}

fn is_parked(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| unparked(n).is_some())
}

/// Moves `path` to a free `<name>.unpacker-renaming[N]` next to it.
fn park(path: &Path) -> io::Result<PathBuf> {
    let name = label(path);
    for n in 0..100 {
        let suffix = if n == 0 { String::new() } else { n.to_string() };
        let temp = path.with_file_name(format!("{name}{PARKED}{suffix}"));
        match move_no_replace(path, &temp) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            result => return result.map(|()| temp),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "too many leftover temporary names",
    ))
}

/// Renames `from` to `to`, failing with `AlreadyExists` instead of
/// replacing anything. A hard link is created first and the old name
/// removed after, because creating a link never replaces: the check and
/// the rename are one step, so no other program can slip a file in between.
/// Disks without hard links (FAT32, exFAT) fall back to check, then rename.
fn move_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    match fs::hard_link(from, to) {
        Ok(()) => fs::remove_file(from).inspect_err(|_| {
            let _ = fs::remove_file(to);
        }),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(e),
        Err(_) if to.exists() => Err(io::ErrorKind::AlreadyExists.into()),
        Err(_) => fs::rename(from, to),
    }
}

/// "Part 3 of 5 is missing", in terms of what people see in the chat.
fn missing_parts_note(have: &[u32], total: Option<u32>, display: &str) -> Option<String> {
    let end = total.unwrap_or_else(|| have.iter().max().map_or(0, |m| m + 1));
    let missing: Vec<u32> = (0..end)
        .filter(|n| !have.contains(n))
        .map(|n| n + 1)
        .collect();
    if missing.is_empty() {
        return None;
    }
    let of = total.map(|t| format!(" of {t}")).unwrap_or_default();
    let which = match missing.as_slice() {
        [one] => format!("Part {one}{of} is missing"),
        many => {
            let list: Vec<String> = many.iter().map(u32::to_string).collect();
            format!("Parts {}{of} are missing", list.join(", "))
        }
    };
    // Not "the 3rd file in the chat": uploaders post parts out of order.
    // Copies are recognised, so downloading everything again is safe.
    Some(format!(
        "  {which} from \"{display}\". The chat order can't be trusted, so download all \
         parts of it again into this folder: copies you already have are recognised."
    ))
}

/// The proper name for volume `number` (0-based) of a set of `count`.
/// At least two digits (`part01`), so 1–9 and 10+ line up even before the
/// last part (and so the real count) is known.
fn volume_name(title: &str, number: u32, count: u32, new_numbering: bool) -> String {
    if new_numbering {
        let width = count.max(number + 1).to_string().len().max(2);
        format!("{title}.part{:0width$}.rar", number + 1)
    } else if number == 0 {
        format!("{title}.rar")
    } else {
        rar_old_name(title, number - 1)
    }
}

/// The title a volume's name shares with its siblings: `.rar` off, then
/// download counters (` (1)` or `(1)`) and a part number, whole or cut off
/// (`.part12`, `.part1`, `.par`, `.p`), in either order: `T(1).part03.rar`
/// and `T.part3 (1).rar` are both `T`. `None` for non-`.rar` names.
fn shared_title(name: &str) -> Option<&str> {
    let mut stem = strip_suffix_ci(name, ".rar")?;
    stem = without_counter(stem);
    stem = without_part_marker(stem);
    stem = without_counter(stem);
    (!stem.is_empty()).then_some(stem)
}

/// `T (3)` or `T(3)` → `T`: counters from Windows, browsers, Telegram, or
/// added by hand. At most three digits: `Movie (2005)` is a year.
fn without_counter(stem: &str) -> &str {
    let Some(inner) = stem.strip_suffix(')') else {
        return stem;
    };
    let Some(open) = inner.rfind('(') else {
        return stem;
    };
    let digits = &inner[open + 1..];
    let before = inner[..open].trim_end_matches(' ');
    let is_counter = (1..=3).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit());
    if is_counter && !before.is_empty() {
        before
    } else {
        stem
    }
}

/// `T.part12`, `T.part1`, `T.par`, `T.p` → `T`.
fn without_part_marker(stem: &str) -> &str {
    let Some((title, tail)) = stem.rsplit_once('.') else {
        return stem;
    };
    let letters = tail.trim_end_matches(|c: char| c.is_ascii_digit());
    let is_part = !letters.is_empty()
        && "part".starts_with(&letters.to_ascii_lowercase())
        && (letters.len() == 4 || letters.len() == tail.len());
    if is_part && !title.is_empty() {
        title
    } else {
        stem
    }
}

/// Same size and same bytes at the start and end: a redownloaded copy.
fn same_contents(a: &Path, b: &Path) -> bool {
    const SAMPLE: u64 = 64 * 1024;
    let read = |path: &Path| -> Option<(u64, Vec<u8>, Vec<u8>)> {
        let mut f = File::open(path).ok()?;
        let len = f.metadata().ok()?.len();
        let mut head = Vec::new();
        (&mut f).take(SAMPLE).read_to_end(&mut head).ok()?;
        f.seek(SeekFrom::Start(len.saturating_sub(SAMPLE))).ok()?;
        let mut tail = Vec::new();
        f.read_to_end(&mut tail).ok()?;
        Some((len, head, tail))
    };
    matches!((read(a), read(b)), (Some(x), Some(y)) if x == y)
}

fn label(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_lose_counters_and_part_numbers_whole_or_cut() {
        let t = "Sjakie_En_De_Chocolade_Fabriek_2005_Vlaams_,_Nederlands_Gesproken";
        for name in [
            format!("{t}.rar"),
            format!("{t} (1).rar"),
            format!("{t} (12).rar"),
            format!("{t}.part1.rar"),
            format!("{t}.part01.rar"),
            format!("{t}.Part10.rar"),
            format!("{t}.par.rar"),
            format!("{t}.p.rar"),
            format!("{t}.part.rar"),
            format!("{t}.part1 (2).rar"),
        ] {
            assert_eq!(shared_title(&name), Some(t), "{name}");
        }
        // Counters without a space, before or after the part marker.
        for name in [
            format!("{t}(1).rar"),
            format!("{t}(4).rar"),
            format!("{t}(1).part03.rar"),
        ] {
            assert_eq!(shared_title(&name), Some(t), "{name}");
        }
        assert_eq!(shared_title("Movie(2005).rar"), Some("Movie(2005)"));
        assert_eq!(
            shared_title("(1).rar"),
            Some("(1)"),
            "nothing left but a counter"
        );
        // Dots in titles stay; only a trailing part marker goes.
        assert_eq!(
            shared_title("Some.Show.S01E01.part2.rar"),
            Some("Some.Show.S01E01")
        );
        assert_eq!(
            shared_title("Some.Show.S01E01.rar"),
            Some("Some.Show.S01E01")
        );
        assert_eq!(shared_title("Movie (2005).rar"), Some("Movie (2005)"));
        assert_eq!(shared_title("Some.Pack.rar"), Some("Some.Pack"));
        assert_eq!(shared_title("movie.r00"), None);
        assert_eq!(shared_title(".rar"), None);
    }

    #[test]
    fn volume_names_follow_the_archives_own_style() {
        assert_eq!(volume_name("T", 0, 3, true), "T.part01.rar");
        assert_eq!(volume_name("T", 11, 12, true), "T.part12.rar");
        assert_eq!(volume_name("T", 99, 120, true), "T.part100.rar");
        assert_eq!(volume_name("T", 2, 150, true), "T.part003.rar");
        assert_eq!(volume_name("T", 0, 3, false), "T.rar");
        assert_eq!(volume_name("T", 1, 3, false), "T.r00");
        assert_eq!(volume_name("T", 101, 150, false), "T.s00");
    }

    #[test]
    fn missing_parts_in_chat_terms() {
        let note = missing_parts_note(&[0, 1, 3, 4], Some(5), "Sjakie").unwrap();
        assert!(
            note.contains("Part 3 of 5 is missing from \"Sjakie\""),
            "{note}"
        );
        let note = missing_parts_note(&[1, 2], None, "S").unwrap();
        assert!(note.contains("Part 1 is missing"), "{note}");
        let note = missing_parts_note(&[0], Some(3), "S").unwrap();
        assert!(note.contains("Parts 2, 3 of 3 are missing"), "{note}");
        assert_eq!(missing_parts_note(&[0, 1, 2], Some(3), "S"), None);
        assert_eq!(
            missing_parts_note(&[0, 1], None, "S"),
            None,
            "last part unknown: can't tell"
        );
    }

    #[test]
    fn name_numbers() {
        assert_eq!(name_number("x.part3.rar"), Some(2));
        assert_eq!(name_number("x.part03.rar"), Some(2));
        assert_eq!(name_number("x.rar"), None);
        assert_eq!(name_number("x.part0.rar"), None);
    }
}
