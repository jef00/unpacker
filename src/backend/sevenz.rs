//! 7z backend, on top of `sevenz-rust2`.
//!
//! `.7z.001` splits are plain byte splits, so they go through `MultiReader`
//! exactly like single files.
//!
//! Passwords: unlike ZIP, 7z has no password check value. A wrong password
//! is only noticed when decrypted data fails to decompress or fails its CRC.
//! So for 7z, `WrongPassword` can come mid-extraction; `output.rs` guarantees
//! the failing file is never left half-written. With encrypted *headers*
//! (`7z a -mhe`), a missing or wrong password is caught when opening.

use std::fs::File;
use std::io::{self, BufReader};
use std::path::Path;

use sevenz_rust2::{ArchiveEntry, ArchiveReader, EntryDecision, Password};

use super::output::{EntryInfo, write_entry};
use super::{Entry, Extractor, Options, Progress, last_part_hint};
use crate::discovery::{Scheme, VolumeSet};
use crate::error::{Error, Result};
use crate::multi_reader::MultiReader;

type Source = BufReader<MultiReader<File>>;

pub struct SevenZExtractor {
    reader: ArchiveReader<Source>,
    has_password: bool,
}

impl SevenZExtractor {
    pub fn new(set: &VolumeSet, password: Option<&str>) -> Result<Self> {
        let source = BufReader::new(MultiReader::open(&set.parts)?);
        let secret = password.map_or_else(Password::empty, Password::from);
        let reader = ArchiveReader::new(source, secret)
            .map_err(|e| open_error(set, password.is_some(), e))?;
        Ok(Self {
            reader,
            has_password: password.is_some(),
        })
    }

    fn is_encrypted(&self) -> bool {
        let archive = self.reader.archive();
        (0..archive.files.len()).any(|i| archive.is_file_encrypted(i))
    }
}

impl Extractor for SevenZExtractor {
    fn list(&mut self) -> Result<Vec<Entry>> {
        let archive = self.reader.archive();
        Ok(archive
            .files
            .iter()
            .enumerate()
            .filter(|(_, f)| !f.is_anti_item())
            .map(|(i, f)| Entry {
                name: f.name().to_string(),
                size: f.size(),
                is_dir: f.is_directory(),
                encrypted: archive.is_file_encrypted(i),
            })
            .collect())
    }

    fn needs_password(&mut self) -> Result<bool> {
        Ok(self.is_encrypted())
    }

    fn extract(&mut self, dest: &Path, opts: &Options, progress: &mut dyn Progress) -> Result<()> {
        let encrypted = self.is_encrypted();
        if encrypted && !self.has_password {
            return Err(Error::PasswordRequired);
        }

        // The callback must return the crate's error type, so our own error
        // is parked here and the iteration told to stop.
        let mut failure: Option<Error> = None;
        let iteration = self.reader.for_each_entries_with_decision(|entry, data| {
            if entry.is_anti_item() {
                return Ok(EntryDecision::Continue); // a "deleted" marker, not a file
            }
            if is_symlink(entry) {
                progress.warning(&format!(
                    "Skipped \"{}\": symbolic links are not extracted.",
                    entry.name()
                ));
                return Ok(EntryDecision::Continue);
            }
            let info = EntryInfo {
                name: entry.name(),
                size: entry.size(),
                is_dir: entry.is_directory(),
            };
            match write_entry(dest, &info, opts, progress, data) {
                // Unread data (skipped entries) is drained and CRC-checked by
                // the crate, which keeps solid archives in sync.
                Ok(()) => Ok(EntryDecision::Continue),
                Err(e) => {
                    failure = Some(e);
                    Ok(EntryDecision::Stop)
                }
            }
        });

        let outcome = match failure {
            Some(e) => Err(e),
            None => iteration.map_err(convert),
        };
        match outcome {
            // Garbage from a wrong key fails decompression or the CRC.
            Err(Error::Corrupt(_)) if encrypted => Err(Error::WrongPassword),
            other => other,
        }
    }
}

/// 7-Zip stores Unix mode bits in the high 16 bits of the attributes when
/// bit 0x8000 is set; Windows symlinks carry FILE_ATTRIBUTE_REPARSE_POINT.
fn is_symlink(entry: &ArchiveEntry) -> bool {
    const UNIX_EXTENSION: u32 = 0x8000;
    const REPARSE_POINT: u32 = 0x400;
    const S_IFMT: u32 = 0o170_000;
    const S_IFLNK: u32 = 0o120_000;

    let attrs = entry.windows_attributes();
    let unix_link = attrs & UNIX_EXTENSION != 0 && (attrs >> 16) & S_IFMT == S_IFLNK;
    unix_link || attrs & REPARSE_POINT != 0
}

fn convert(e: sevenz_rust2::Error) -> Error {
    use sevenz_rust2::Error as Sz;
    match e {
        Sz::PasswordRequired => Error::PasswordRequired,
        Sz::MaybeBadPassword(_) => Error::WrongPassword,
        Sz::Io(io, _) | Sz::FileOpen(io, _) if io.raw_os_error().is_some() => Error::Io(io),
        Sz::Io(io, _) if io.kind() == io::ErrorKind::UnexpectedEof => {
            Error::Corrupt("the archive ends too early; is a part missing or incomplete?".into())
        }
        Sz::UnsupportedCompressionMethod(method) => {
            Error::Unsupported(format!("7z compression method {method}"))
        }
        Sz::Unsupported(what) => Error::Unsupported(what.into_owned()),
        Sz::MaxMemLimited { max_kb, actaul_kb } => Error::Unsupported(format!(
            "this archive needs {} MB of memory to unpack (limit {} MB)",
            actaul_kb / 1024,
            max_kb / 1024
        )),
        other => Error::Corrupt(other.to_string()),
    }
}

fn open_error(set: &VolumeSet, password_given: bool, e: sevenz_rust2::Error) -> Error {
    match (convert(e), password_given, set.scheme) {
        // We only ever pass a password after the archive asked for one, so
        // a header that decrypts to garbage means the password is wrong.
        (Error::Corrupt(_), true, _) => Error::WrongPassword,
        (Error::Corrupt(_), false, Scheme::Split) => Error::Corrupt(last_part_hint(set)),
        (other, ..) => other,
    }
}
