//! Format backends behind one trait.
//!
//! The trait takes a whole [`VolumeSet`] rather than a `Read + Seek`,
//! because the hardest backend (RAR via libunrar) wants file paths and
//! walks the volumes itself. ZIP and 7z wrap the set in a `MultiReader`
//! internally. Callers never see the difference.

mod output;
mod rar;
mod sevenz;
mod zip;
mod zip_spanned;

use std::path::{Component, Path, PathBuf};

use crate::detect::{self, Format};
use crate::discovery::VolumeSet;
use crate::error::{Error, Result};
use crate::safety::sanitize_entry_path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Name as stored in the archive. Untrusted: run it through
    /// `safety::sanitize_entry_path` before touching the filesystem.
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
    pub encrypted: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Overwrite {
    /// Leave existing files alone (safe default).
    #[default]
    Skip,
    Replace,
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub overwrite: Overwrite,
    /// Leave this leading folder out of every entry's path, so an archive
    /// that holds just `Movie/…` doesn't unpack to `movie/Movie/…`.
    /// Use [`single_top_folder`] to find it. Entries outside it are written
    /// unchanged.
    pub strip_top_folder: Option<PathBuf>,
}

/// If everything in the archive sits inside one top-level folder, returns
/// that folder (as it will be named on disk), so the caller can skip the
/// extra nesting level via [`Options::strip_top_folder`].
///
/// Returns `None` when there is more than one top-level item, when the only
/// top-level item is a file, or when the folder is empty. Names that would be
/// skipped as unsafe don't count. Comparison is exact (case-sensitive), so on
/// Windows `Movie/` and `movie/` count as two folders: the safe side.
pub fn single_top_folder(entries: &[Entry]) -> Option<PathBuf> {
    let mut top: Option<PathBuf> = None;
    let mut has_content = false;
    for entry in entries {
        let Ok(path) = sanitize_entry_path(&entry.name) else {
            continue;
        };
        let mut components = path.components();
        let first = match components.next() {
            Some(Component::Normal(first)) => PathBuf::from(first),
            _ => return None,
        };
        let nested = components.next().is_some();
        if !nested && !entry.is_dir {
            return None; // a file at the top level
        }
        has_content |= nested;
        match &top {
            Some(existing) if *existing != first => return None,
            Some(_) => {}
            None => top = Some(first),
        }
    }
    top.filter(|_| has_content)
}

/// Progress callbacks, so backends stay independent of the UI.
pub trait Progress {
    fn entry_started(&mut self, name: &str, size: u64);
    fn bytes_written(&mut self, bytes: u64);
    fn warning(&mut self, message: &str);
}

pub trait Extractor {
    fn list(&mut self) -> Result<Vec<Entry>>;

    /// Whether any entry is encrypted, so the UI can ask for a password
    /// *before* anything is written.
    fn needs_password(&mut self) -> Result<bool>;

    /// Extracts everything into `dest` (created if needed).
    ///
    /// Password contract, so callers can retry with another password:
    /// - `PasswordRequired` always comes before any file is written.
    /// - `WrongPassword` comes before any file is written for ZIP and for
    ///   archives with encrypted headers. 7z and RAR without encrypted headers
    ///   have no cheap password check, so there it comes at the first
    ///   encrypted file (possibly after unencrypted ones were written).
    /// - Either way, the file being written when an error hits is removed;
    ///   no half-written file is ever left behind.
    fn extract(&mut self, dest: &Path, opts: &Options, progress: &mut dyn Progress) -> Result<()>;
}

/// Archive indexes (ZIP central directory, 7z header) sit at the very end.
/// If they can't be read in a split set, the likeliest cause is a missing
/// last part, which discovery can't detect from filenames alone.
pub(crate) fn last_part_hint(set: &VolumeSet) -> String {
    match set.next_part_name() {
        Some(next) => {
            format!("couldn't find the end of the archive. Part \"{next}\" may be missing.")
        }
        None => "couldn't find the end of the archive. A part may be missing.".into(),
    }
}

/// Picks the right backend by looking at the first part's magic bytes.
pub fn open(set: &VolumeSet, password: Option<&str>) -> Result<Box<dyn Extractor>> {
    let format = match detect::from_file(set.first()) {
        // The `.zip` of a spanned set usually starts mid-file, so it has no
        // magic bytes. Say which part is missing instead of "not an archive".
        Err(Error::UnknownFormat(_)) if zip_spanned::lone_last_disk(set) => {
            return Err(Error::MissingVolume(format!("{}.z01", set.base)));
        }
        other => other?,
    };
    Ok(match format {
        Format::Rar4 | Format::Rar5 => Box::new(rar::RarExtractor::new(set, password)?),
        Format::SevenZ => Box::new(sevenz::SevenZExtractor::new(set, password)?),
        Format::Zip => Box::new(zip::ZipExtractor::new(set, password)?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(list: &[(&str, bool)]) -> Vec<Entry> {
        list.iter()
            .map(|&(name, is_dir)| Entry {
                name: name.into(),
                size: 0,
                is_dir,
                encrypted: false,
            })
            .collect()
    }

    fn top(list: &[(&str, bool)]) -> Option<PathBuf> {
        single_top_folder(&entries(list))
    }

    #[test]
    fn one_folder_is_found() {
        let found = Some(PathBuf::from("Movie"));
        // With and without an explicit directory entry, any separator.
        assert_eq!(top(&[("Movie/", true), ("Movie/a.mkv", false)]), found);
        assert_eq!(
            top(&[("Movie/a.mkv", false), ("Movie/b.srt", false)]),
            found
        );
        assert_eq!(top(&[("Movie\\sub\\a.mkv", false)]), found);
        assert_eq!(top(&[("./Movie/a.mkv", false)]), found);
    }

    #[test]
    fn found_name_is_the_sanitised_one() {
        assert_eq!(top(&[("What?/a.mkv", false)]), Some(PathBuf::from("What_")));
    }

    #[test]
    fn anything_else_keeps_the_nesting() {
        assert_eq!(top(&[]), None);
        assert_eq!(top(&[("a.mkv", false)]), None); // single file
        assert_eq!(top(&[("Movie/a.mkv", false), ("b.nfo", false)]), None);
        assert_eq!(top(&[("A/a.mkv", false), ("B/b.mkv", false)]), None);
        assert_eq!(top(&[("Movie/a", false), ("movie/b", false)]), None);
        assert_eq!(top(&[("Empty/", true)]), None); // nothing inside
    }

    #[test]
    fn unsafe_names_are_ignored() {
        // They're skipped at extraction anyway.
        assert_eq!(
            top(&[("Movie/a.mkv", false), ("../evil", false)]),
            Some(PathBuf::from("Movie"))
        );
    }
}
