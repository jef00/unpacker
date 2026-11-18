//! Format backends behind one trait.
//!
//! The trait takes a whole [`VolumeSet`] rather than a `Read + Seek`,
//! because the hardest backend (RAR via libunrar) wants file paths and
//! walks the volumes itself. ZIP and 7z wrap the set in a `MultiReader`
//! internally. Callers never see the difference.

mod output;
mod rar;
mod sevenz;
mod tar;
mod zip;
mod zip_spanned;

pub use rar::unrar_lock;

use std::path::{Component, Path, PathBuf};

use crate::detect::{self, Format};
use crate::discovery::VolumeSet;
use crate::error::{Error, Result};
use crate::origin::Origin;
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
    /// Leave these leading folders out of every entry's path, so an archive
    /// that holds just `Movie/…` doesn't unpack to `movie/Movie/…`.
    /// Use [`layout`] to find them. Directory entries for these folders (or
    /// their parents) become `dest` itself; entries outside them are written
    /// unchanged.
    pub strip_prefix: Option<PathBuf>,
    /// The archive's "from the internet" tag, copied onto every file written.
    /// See [`crate::origin`].
    pub origin: Option<Origin>,
}

/// How an archive's contents are arranged, so the UI can avoid pointless
/// nesting. Paths are as they will be named on disk (sanitised).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Layout {
    /// Exactly one file (possibly inside folders, with nothing else in
    /// them): put it straight next to the archive. `folders` is the path to
    /// strip (empty if the file is at the top level); `name` is the file name.
    SingleFile { folders: PathBuf, name: PathBuf },
    /// Everything sits in this one top-level folder: skip that level.
    SingleFolder(PathBuf),
    /// Anything else: keep the archive's structure as it is.
    Nested,
}

/// Works out the [`Layout`] from an archive listing.
///
/// Names that would be skipped as unsafe don't count. Comparison is exact
/// (case-sensitive), so on Windows `Movie/` and `movie/` count as two
/// folders: the safe side.
pub fn layout(entries: &[Entry]) -> Layout {
    let safe: Vec<(PathBuf, bool)> = entries
        .iter()
        .filter_map(|e| sanitize_entry_path(&e.name).ok().map(|p| (p, e.is_dir)))
        .collect();

    let mut files = safe.iter().filter(|(_, is_dir)| !is_dir);
    if let (Some((file, _)), None) = (files.next(), files.next()) {
        let folders = file.parent().map(Path::to_path_buf).unwrap_or_default();
        // Any directory entry must be one of the file's own folders;
        // an extra (empty) folder means there's more than one thing here.
        let only_ancestors = safe
            .iter()
            .filter(|(_, is_dir)| *is_dir)
            .all(|(dir, _)| folders.starts_with(dir));
        if let (true, Some(name)) = (only_ancestors, file.file_name()) {
            return Layout::SingleFile {
                folders,
                name: PathBuf::from(name),
            };
        }
    }

    let mut top: Option<PathBuf> = None;
    let mut has_content = false;
    for (path, is_dir) in &safe {
        let mut components = path.components();
        let first = match components.next() {
            Some(Component::Normal(first)) => PathBuf::from(first),
            _ => return Layout::Nested,
        };
        let nested = components.next().is_some();
        if !nested && !is_dir {
            return Layout::Nested; // a file next to other things
        }
        has_content |= nested;
        match &top {
            Some(existing) if *existing != first => return Layout::Nested,
            Some(_) => {}
            None => top = Some(first),
        }
    }
    match top {
        Some(top) if has_content => Layout::SingleFolder(top),
        _ => Layout::Nested,
    }
}

/// Progress callbacks, so backends stay independent of the UI.
///
/// `Send` because the RAR backend reports bytes from a watcher thread while
/// libunrar writes a file (calls never overlap).
pub trait Progress: Send {
    fn entry_started(&mut self, name: &str, size: u64);
    fn bytes_written(&mut self, bytes: u64);
    fn warning(&mut self, message: &str);

    /// An entry wasn't written because a file with that name already exists
    /// (and `Overwrite::Skip` is in effect). By default just a warning.
    fn skipped_existing(&mut self, name: &str) {
        self.warning(&format!("Skipped \"{name}\": file already exists."));
    }
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
        Format::Tar => Box::new(tar::TarExtractor::new(set, None)?),
        Format::Compressed(codec) => Box::new(tar::TarExtractor::new(set, Some(codec))?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout_of(list: &[(&str, bool)]) -> Layout {
        let entries: Vec<Entry> = list
            .iter()
            .map(|&(name, is_dir)| Entry {
                name: name.into(),
                size: 0,
                is_dir,
                encrypted: false,
            })
            .collect();
        layout(&entries)
    }

    fn folder(name: &str) -> Layout {
        Layout::SingleFolder(PathBuf::from(name))
    }

    fn file(folders: &str, name: &str) -> Layout {
        Layout::SingleFile {
            folders: PathBuf::from(folders),
            name: PathBuf::from(name),
        }
    }

    #[test]
    fn one_file_goes_next_to_the_archive() {
        assert_eq!(layout_of(&[("a.mkv", false)]), file("", "a.mkv"));
        assert_eq!(
            layout_of(&[("Movie/", true), ("Movie/a.mkv", false)]),
            file("Movie", "a.mkv")
        );
        assert_eq!(
            layout_of(&[("A/", true), ("A/B/", true), ("A\\B\\a.mkv", false)]),
            file("A/B", "a.mkv")
        );
        assert_eq!(layout_of(&[("What?.mkv", false)]), file("", "What_.mkv"));
    }

    #[test]
    fn one_file_plus_an_empty_folder_is_not_single() {
        assert_eq!(
            layout_of(&[("Movie/a.mkv", false), ("Movie/extras/", true)]),
            folder("Movie")
        );
        assert_eq!(
            layout_of(&[("a.mkv", false), ("extras/", true)]),
            Layout::Nested
        );
    }

    #[test]
    fn one_folder_is_found() {
        let both = [("Movie/a.mkv", false), ("Movie/b.srt", false)];
        assert_eq!(layout_of(&both), folder("Movie"));
        assert_eq!(
            layout_of(&[("Movie/", true), ("Movie/a.mkv", false), ("Movie/b", false)]),
            folder("Movie")
        );
        assert_eq!(
            layout_of(&[("./What?/a", false), ("What?\\b", false)]),
            folder("What_")
        );
    }

    #[test]
    fn anything_else_keeps_the_nesting() {
        assert_eq!(layout_of(&[]), Layout::Nested);
        assert_eq!(layout_of(&[("Empty/", true)]), Layout::Nested);
        assert_eq!(
            layout_of(&[("Movie/a.mkv", false), ("b.nfo", false)]),
            Layout::Nested
        );
        assert_eq!(
            layout_of(&[("A/a.mkv", false), ("B/b.mkv", false)]),
            Layout::Nested
        );
        assert_eq!(
            layout_of(&[("Movie/a", false), ("movie/b", false)]),
            Layout::Nested
        );
    }

    #[test]
    fn unsafe_names_are_ignored() {
        // They're skipped at extraction anyway.
        assert_eq!(
            layout_of(&[
                ("Movie/a.mkv", false),
                ("Movie/b", false),
                ("../evil", false)
            ]),
            folder("Movie")
        );
        assert_eq!(
            layout_of(&[("a.mkv", false), ("../evil", false)]),
            file("", "a.mkv")
        );
    }
}
