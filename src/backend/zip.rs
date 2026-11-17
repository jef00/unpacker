//! ZIP backend, on top of the `zip` crate.
//!
//! Single `.zip` files and `.zip.001` splits both go through `MultiReader`,
//! so they share one code path. Spanned archives (`.z01`, `.z02`, …, `.zip`)
//! are multi-disk ZIPs whose offsets are relative to each disk; the `zip`
//! crate doesn't support those, so we say so clearly instead of failing with
//! a confusing "invalid archive".

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use zip::ZipArchive;
use zip::result::ZipError;

use super::output::{EntryInfo, write_entry};
use super::{Entry, Extractor, Options, Progress, last_part_hint};
use crate::discovery::{Scheme, VolumeSet};
use crate::error::{Error, Result};
use crate::multi_reader::MultiReader;

type Archive = ZipArchive<BufReader<MultiReader<File>>>;

pub struct ZipExtractor {
    archive: Archive,
    password: Option<Vec<u8>>,
}

impl ZipExtractor {
    pub fn new(set: &VolumeSet, password: Option<&str>) -> Result<Self> {
        if set.scheme == Scheme::ZipSpanned {
            return Err(Error::Unsupported(
                "spanned ZIP archives (.z01, .z02, …). Open it with 7-Zip, \
                 or ask for a single .zip or a .zip.001 split instead."
                    .into(),
            ));
        }
        let reader = BufReader::new(MultiReader::open(&set.parts)?);
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
        (_, other) => other,
    }
}
