//! RAR backend, on top of the `unrar` crate: RarLab's own UnRAR C++ source,
//! compiled into the binary (needs a C++ compiler at build time).
//!
//! libunrar opens volumes by *name* and writes files itself, so this backend
//! differs from ZIP/7z in two ways:
//!
//! - It opens `set.first()` and lets libunrar walk `.part2.rar` / `.r00` etc.
//!   A `.rar.001` byte split is first joined into one temporary file, since
//!   libunrar can't read from a stream.
//! - Instead of `write_entry`, it uses `plan_entry` + `TempPath` and has
//!   libunrar extract to the temp path. Same rules, same guarantees.
//!
//! License: the UnRAR source may be used to *extract* RAR archives, never to
//! create them. That's all we do.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use unrar::error::{Code, UnrarError, When};
use unrar::{Archive, FileHeader};

use super::output::{EntryInfo, Plan, TempPath, plan_entry, while_counting};
use super::{Entry, Extractor, Options, Progress};
use crate::discovery::{Scheme, VolumeSet};
use crate::error::{Error, Result};
use crate::multi_reader::MultiReader;

/// UnRAR keeps its error state in one process-wide object (`ErrHandler`),
/// which only opening an archive resets. Since UnRAR 7.2x every header read
/// and extract also returns that shared state, so two archives open at once
/// see each other's errors. Only one may be open at a time.
static UNRAR: Mutex<()> = Mutex::new(());

/// Exclusive use of UnRAR. Hold it from opening an archive until it's dropped.
/// The mutex guards no data, so a poisoned lock is still safe to take.
pub fn unrar_lock() -> MutexGuard<'static, ()> {
    UNRAR.lock().unwrap_or_else(PoisonError::into_inner)
}

pub struct RarExtractor {
    /// What libunrar opens: the first volume, or the joined copy of a split.
    archive_path: PathBuf,
    /// Owns the joined copy (if any) and deletes it when we're done.
    _joined: Option<TempPath>,
    password: Option<String>,
    /// What the next part would be called, for "part X is missing".
    next_part: Option<String>,
}

impl RarExtractor {
    pub fn new(set: &VolumeSet, password: Option<&str>) -> Result<Self> {
        let joined = match set.scheme {
            Scheme::Split => Some(join_split(set)?),
            _ => None,
        };
        let archive_path = joined
            .as_ref()
            .map_or_else(|| set.first().to_path_buf(), |t| t.path().to_path_buf());
        let extractor = Self {
            archive_path,
            _joined: joined,
            password: password.map(str::to_owned),
            next_part: set.next_part_name(),
        };
        // Read the first header now, so format and header-password problems
        // surface from `backend::open()` like they do for the other backends.
        // (With encrypted headers, opening succeeds; reading is what fails.)
        let _unrar = unrar_lock();
        extractor
            .archive()
            .open_for_listing()
            .map_err(|e| extractor.convert(e, false))?
            .next()
            .transpose()
            .map_err(|e| extractor.convert(e, false))?;
        Ok(extractor)
    }

    fn archive(&self) -> Archive<'_> {
        match &self.password {
            Some(password) => Archive::with_password(&self.archive_path, password.as_bytes()),
            None => Archive::new(&self.archive_path),
        }
    }

    fn headers(&self) -> Result<Vec<FileHeader>> {
        let _unrar = unrar_lock();
        let listing = self
            .archive()
            .open_for_listing()
            .map_err(|e| self.convert(e, false))?;
        listing
            .map(|header| header.map_err(|e| self.convert(e, false)))
            .collect()
    }

    /// Maps libunrar's status codes onto our errors. `encrypted_entry` says
    /// whether the entry being processed was encrypted, because RAR3 reports
    /// a wrong password only as a CRC error.
    fn convert(&self, e: UnrarError, encrypted_entry: bool) -> Error {
        let text = e.to_string();
        match (e.code, e.when) {
            (Code::MissingPassword, _) => Error::PasswordRequired,
            (Code::BadPassword, _) => Error::WrongPassword,
            (Code::BadData, When::Process) if encrypted_entry => Error::WrongPassword,
            // We only pass a password after the archive asked for one, so
            // headers that decrypt to garbage mean the password is wrong.
            (Code::BadData, When::Open | When::Read) if self.password.is_some() => {
                Error::WrongPassword
            }
            // libunrar couldn't open the next volume: a trailing part is
            // missing, which discovery can't see from filenames alone.
            (Code::EOpen, When::Read | When::Process) => match &self.next_part {
                Some(name) => Error::MissingVolume(name.clone()),
                None => Error::Corrupt("the archive continues in a part that is missing.".into()),
            },
            (Code::EOpen, When::Open) => Error::Io(io::Error::new(io::ErrorKind::NotFound, text)),
            (Code::ECreate | Code::EWrite | Code::EClose | Code::ERead, _) => {
                Error::Io(io::Error::other(text))
            }
            (Code::NoMemory, _) => {
                Error::Unsupported("not enough memory to unpack this archive".into())
            }
            (Code::UnknownFormat, _) => Error::Unsupported(text),
            (Code::EReference, _) => {
                Error::Unsupported("file references (archives made with `rar -oi`)".into())
            }
            _ => Error::Corrupt(text),
        }
    }
}

impl Extractor for RarExtractor {
    fn list(&mut self) -> Result<Vec<Entry>> {
        Ok(self
            .headers()?
            .iter()
            .map(|h| Entry {
                name: entry_name(h),
                size: h.unpacked_size,
                is_dir: h.is_directory(),
                encrypted: h.is_encrypted(),
            })
            .collect())
    }

    fn needs_password(&mut self) -> Result<bool> {
        match self.headers() {
            Ok(headers) => Ok(headers.iter().any(FileHeader::is_encrypted)),
            Err(Error::PasswordRequired) => Ok(true), // encrypted headers
            Err(e) => Err(e),
        }
    }

    fn extract(&mut self, dest: &Path, opts: &Options, progress: &mut dyn Progress) -> Result<()> {
        // Headers are cheap to scan: ask for the password before writing anything.
        if self.password.is_none() && self.needs_password()? {
            return Err(Error::PasswordRequired);
        }

        let _unrar = unrar_lock();
        let mut cursor = self
            .archive()
            .open_for_processing()
            .map_err(|e| self.convert(e, false))?;
        while let Some(header) = cursor.read_header().map_err(|e| self.convert(e, false))? {
            let entry = header.entry();
            let name = entry_name(entry);
            let encrypted = entry.is_encrypted();
            let info = EntryInfo {
                name: &name,
                size: entry.unpacked_size,
                is_dir: entry.is_directory(),
            };

            let plan = if is_link(entry.file_attr) {
                progress.warning(&format!(
                    "Skipped \"{name}\": symbolic links are not extracted."
                ));
                Plan::Skip
            } else {
                plan_entry(dest, &info, opts, progress)?
            };

            cursor = match plan {
                Plan::Skip | Plan::Directory => {
                    header.skip().map_err(|e| self.convert(e, encrypted))?
                }
                Plan::File(target) => {
                    progress.entry_started(&name, info.size);
                    let temp = TempPath::for_target(&target);
                    // Counted from UnRAR's data callback, not the file size:
                    // on Windows UnRAR sizes a large file fully up front.
                    let written = Arc::new(AtomicU64::new(0));
                    let (next, reported) = while_counting(&written, progress, || {
                        header.extract_to_counting(temp.path(), Arc::clone(&written))
                    });
                    let next = next.map_err(|e| self.convert(e, encrypted))?;
                    if is_regular_file(temp.path()) {
                        temp.commit(opts)?;
                        progress.bytes_written(info.size.saturating_sub(reported));
                    } else {
                        // Safety net: libunrar made a link (or nothing) after
                        // all. Dropping `temp` removes the link itself.
                        progress.warning(&format!("Skipped \"{name}\": not a regular file."));
                    }
                    next
                }
            };
        }
        Ok(())
    }
}

/// Copies a `.rar.001`, `.rar.002`, … byte split into one temporary `.rar`
/// next to it, because libunrar can only open real files.
fn join_split(set: &VolumeSet) -> Result<TempPath> {
    let temp = TempPath::for_target(&set.first().with_file_name(&set.base));
    let mut joined = File::create(temp.path())?;
    io::copy(&mut MultiReader::open(&set.parts)?, &mut joined)?;
    Ok(temp)
}

fn entry_name(header: &FileHeader) -> String {
    header.filename.to_string_lossy().into_owned()
}

/// RAR stores Unix mode bits (archives made on Unix) or Windows attributes.
/// Either can mark a symbolic link.
fn is_link(attrs: u32) -> bool {
    const S_IFMT: u32 = 0o170_000;
    const S_IFLNK: u32 = 0o120_000;
    const REPARSE_POINT: u32 = 0x400;
    attrs & S_IFMT == S_IFLNK || attrs & REPARSE_POINT != 0
}

fn is_regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_links() {
        assert!(is_link(0o120_777)); // Unix symlink
        assert!(is_link(0x420)); // Windows: archive + reparse point
        assert!(!is_link(0o100_644)); // Unix regular file
        assert!(!is_link(0o040_755)); // Unix directory
        assert!(!is_link(0x20)); // Windows: archive bit only
    }
}
