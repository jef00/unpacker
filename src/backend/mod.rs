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

use std::path::Path;

use crate::detect::{self, Format};
use crate::discovery::VolumeSet;
use crate::error::Result;

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
    Ok(match detect::from_file(set.first())? {
        Format::Rar4 | Format::Rar5 => Box::new(rar::RarExtractor::new(set, password)?),
        Format::SevenZ => Box::new(sevenz::SevenZExtractor::new(set, password)?),
        Format::Zip => Box::new(zip::ZipExtractor::new(set, password)?),
    })
}
