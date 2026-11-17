//! ZIP backend, on top of the `zip` crate.
//!
//! Single `.zip` files and `.zip.001` splits go through `MultiReader`.
//! Spanned archives (`.z01`, `.z02`, …, `.zip`) go through `zip_spanned`,
//! which presents them as a single-disk ZIP. After that, one code path.

use std::io::{BufReader, Read, Seek};
use std::path::Path;

use zip::ZipArchive;
use zip::result::ZipError;

use super::output::{EntryInfo, write_entry};
use super::zip_spanned;
use super::{Entry, Extractor, Options, Progress, last_part_hint};
use crate::discovery::{Scheme, VolumeSet};
use crate::error::{Error, Result};
use crate::multi_reader::MultiReader;

/// Any seekable source: glued parts, or a rewritten spanned set.
trait Source: Read + Seek {}
impl<T: Read + Seek> Source for T {}

type Archive = ZipArchive<BufReader<Box<dyn Source>>>;

pub struct ZipExtractor {
    archive: Archive,
    password: Option<Vec<u8>>,
}

impl ZipExtractor {
    pub fn new(set: &VolumeSet, password: Option<&str>) -> Result<Self> {
        let source: Box<dyn Source> = match set.scheme {
            Scheme::ZipSpanned => Box::new(zip_spanned::open(set)?),
            _ => Box::new(MultiReader::open(&set.parts)?),
        };
        let reader = BufReader::new(source);
        let archive = ZipArchive::new(reader).map_err(|e| open_error(set, e))?;
        Ok(Self {
            archive,
            password: password.map(|p| p.as_bytes().to_vec()),
        })
    }

    fn first_encrypted(&mut self) -> Result<Option<usize>> {
        for i in 0..self.archive.len() {
            if self.archive.by_index_raw(i).map_err(convert)?.encrypted() {
                return Ok(Some(i));
            }
        }
        Ok(None)
    }

    /// Fails fast on a missing or wrong password, before any file is written.
    /// (Opening an encrypted entry checks the password-verification bytes.)
    fn check_password(&mut self) -> Result<()> {
        let Some(i) = self.first_encrypted()? else {
            return Ok(());
        };
        let Some(password) = &self.password else {
            return Err(Error::PasswordRequired);
        };
        self.archive
            .by_index_decrypt(i, password)
            .map_err(convert)?;
        Ok(())
    }
}

impl Extractor for ZipExtractor {
    fn list(&mut self) -> Result<Vec<Entry>> {
        (0..self.archive.len())
            .map(|i| {
                let file = self.archive.by_index_raw(i).map_err(convert)?;
                Ok(Entry {
                    name: file.name().map_err(convert)?.into_owned(),
                    size: file.size(),
                    is_dir: file.is_dir(),
                    encrypted: file.encrypted(),
                })
            })
            .collect()
    }

    fn needs_password(&mut self) -> Result<bool> {
        Ok(self.first_encrypted()?.is_some())
    }

    fn extract(&mut self, dest: &Path, opts: &Options, progress: &mut dyn Progress) -> Result<()> {
        self.check_password()?;

        for i in 0..self.archive.len() {
            let encrypted = self.archive.by_index_raw(i).map_err(convert)?.encrypted();
            let mut file = match (&self.password, encrypted) {
                (Some(password), true) => self.archive.by_index_decrypt(i, password),
                _ => self.archive.by_index(i),
            }
            .map_err(convert)?;

            let name = file.name().map_err(convert)?.into_owned();
            if file.is_symlink() {
                // A symlink entry could point anywhere on disk; videos never need one.
                progress.warning(&format!(
                    "Skipped \"{name}\": symbolic links are not extracted."
                ));
                continue;
            }

            let info = EntryInfo {
                name: &name,
                size: file.size(),
                is_dir: file.is_dir(),
            };
            match write_entry(dest, &info, opts, progress, &mut file) {
                // ZipCrypto's password check is only 1 byte: about 1 in 256
                // wrong passwords slip through and then fail the CRC instead.
                Err(Error::Corrupt(_)) if encrypted => return Err(Error::WrongPassword),
                result => result?,
            }
        }
        Ok(())
    }
}

fn convert(e: ZipError) -> Error {
    match e {
        ZipError::Io(io) => Error::Io(io),
        ZipError::InvalidPassword => Error::WrongPassword,
        ZipError::UnsupportedArchive(msg) if msg == ZipError::PASSWORD_REQUIRED => {
            Error::PasswordRequired
        }
        ZipError::UnsupportedArchive(msg) => Error::Unsupported(msg.into()),
        ZipError::CompressionMethodNotSupported(method) => {
            Error::Unsupported(format!("ZIP compression method {method}"))
        }
        ZipError::InvalidArchive(msg) => Error::Corrupt(msg.into_owned()),
        other => Error::Corrupt(other.to_string()),
    }
}

fn open_error(set: &VolumeSet, e: ZipError) -> Error {
    match (set.scheme, convert(e)) {
        (Scheme::Split, Error::Corrupt(_)) => Error::Corrupt(last_part_hint(set)),
        // A lone last disk that happens to start with a local header passes
        // detection, then fails here as multi-disk. (Usually `backend::open`
        // catches it first.)
        (Scheme::Single, err @ (Error::Corrupt(_) | Error::Unsupported(_))) => {
            if zip_spanned::lone_last_disk(set) {
                Error::MissingVolume(format!("{}.z01", set.base))
            } else {
                err
            }
        }
        (_, other) => other,
    }
}
