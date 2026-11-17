//! Tar backend: `.tar`, plus `.tar.gz`/`.tgz`, `.tar.bz2`, `.tar.xz` and
//! `.tar.zst`, single or split into `.001`, `.002`, … parts.
//!
//! Tar is a plain stream of header + data blocks with no index, so:
//!
//! - **Listing reads the whole archive.** An uncompressed tar skips file
//!   data by seeking, so that's cheap; a compressed one has to decompress
//!   everything (the price of `.tar.gz` having no index).
//! - **Completeness has to be checked by us.** Tar has no checksums on file
//!   data and ends with zero blocks. The `tar` crate treats the stream simply
//!   running out at a header boundary as a normal end, so a split set
//!   missing its last part (cut at a 512-byte boundary, as 100 MB parts are)
//!   would "extract fine" with files silently missing, and the archives
//!   might then be deleted. [`Tracked`] notices the input running out
//!   instead. Compressed tars are also covered by the gzip/bzip2/xz/zstd
//!   checksums.
//!
//! Only regular files and directories are extracted. Symbolic and hard
//! links, device files and FIFOs are skipped with a warning.

use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use tar::EntryType;

use super::output::{EntryInfo, read_error, write_entry};
use super::{Entry, Extractor, Options, Progress, last_part_hint};
use crate::detect::{Codec, TAR_BLOCK, is_tar_header};
use crate::discovery::{Scheme, VolumeSet};
use crate::error::{Error, Result};
use crate::multi_reader::MultiReader;

type Glued = BufReader<MultiReader<File>>;

pub struct TarExtractor {
    set: VolumeSet,
    codec: Option<Codec>,
}

/// What the `tar` crate reads from: seekable when uncompressed.
enum Source {
    Plain(Tracked<Glued>),
    Compressed(Tracked<Box<dyn Read>>),
}

impl TarExtractor {
    /// `codec` is `None` for a plain `.tar`.
    pub fn new(set: &VolumeSet, codec: Option<Codec>) -> Result<Self> {
        let extractor = Self {
            set: set.clone(),
            codec,
        };
        // A `.gz` (etc.) of something that isn't a tar, like `movie.mkv.gz`:
        // say so now, instead of failing on garbage headers later.
        if codec.is_some() {
            let mut head = Vec::with_capacity(TAR_BLOCK);
            extractor
                .decompressed()?
                .take(TAR_BLOCK as u64)
                .read_to_end(&mut head)
                .map_err(|e| extractor.error(e))?;
            if !is_tar_header(&head) {
                return Err(Error::Unsupported(
                    "a compressed file that isn't a tar archive; \
                     only .tar.gz, .tar.bz2, .tar.xz and .tar.zst are supported"
                        .into(),
                ));
            }
        }
        Ok(extractor)
    }

    fn glued(&self) -> Result<Glued> {
        Ok(BufReader::new(MultiReader::open(&self.set.parts)?))
    }

    fn decompressed(&self) -> Result<Box<dyn Read>> {
        let glued = self.glued()?;
        Ok(match self.codec {
            None => Box::new(glued),
            // The "Multi" decoders read concatenated streams (`cat a.gz b.gz`,
            // pigz/pbzip2 output) to the end instead of stopping after one.
            Some(Codec::Gzip) => Box::new(flate2::read::MultiGzDecoder::new(glued)),
            Some(Codec::Bzip2) => Box::new(bzip2::read::MultiBzDecoder::new(glued)),
            Some(Codec::Xz) => Box::new(lzma_rust2::XzReader::new(glued, true)),
            Some(Codec::Zstd) => Box::new(zstd::stream::read::Decoder::with_buffer(glued)?),
        })
    }

    fn source(&self) -> Result<Source> {
        Ok(match self.codec {
            None => Source::Plain(Tracked::new(self.glued()?)),
            Some(_) => Source::Compressed(Tracked::new(self.decompressed()?)),
        })
    }

    /// Read errors mean damage; a split set cut short gets the hint about
    /// which part is probably missing.
    fn error(&self, e: io::Error) -> Error {
        let truncated = e.kind() == io::ErrorKind::UnexpectedEof;
        match read_error(e) {
            Error::Corrupt(_) if truncated && self.set.scheme == Scheme::Split => {
                Error::Corrupt(last_part_hint(&self.set))
            }
            other => other,
        }
    }

    /// Runs `f` over the entries, then insists the archive really ended.
    fn walk(&self, mut f: impl FnMut(&mut dyn TarEntry) -> Result<()>) -> Result<()> {
        match self.source()? {
            Source::Plain(reader) => {
                let mut archive = tar::Archive::new(reader);
                let entries = archive.entries_with_seek().map_err(|e| self.error(e))?;
                for entry in entries {
                    f(&mut entry.map_err(|e| self.error(e))?)?;
                }
                self.ensure_complete(archive.into_inner().ran_out)
            }
            Source::Compressed(reader) => {
                let mut archive = tar::Archive::new(reader);
                let entries = archive.entries().map_err(|e| self.error(e))?;
                for entry in entries {
                    f(&mut entry.map_err(|e| self.error(e))?)?;
                }
                self.ensure_complete(archive.into_inner().ran_out)
            }
        }
    }

    fn ensure_complete(&self, ran_out: bool) -> Result<()> {
        if !ran_out {
            return Ok(()); // stopped at the end-of-archive zero block
        }
        Err(Error::Corrupt(match self.set.scheme {
            Scheme::Split => last_part_hint(&self.set),
            _ => "the archive ends too early; is it incomplete?".into(),
        }))
    }
}

impl Extractor for TarExtractor {
    fn list(&mut self) -> Result<Vec<Entry>> {
        let mut out = Vec::new();
        self.walk(|entry| {
            if let Some(is_dir) = kind(entry.entry_type()).listed() {
                out.push(Entry {
                    name: entry.name(),
                    size: entry.size(),
                    is_dir,
                    encrypted: false,
                });
            }
            Ok(())
        })?;
        Ok(out)
    }

    fn needs_password(&mut self) -> Result<bool> {
        Ok(false) // tar has no encryption
    }

    fn extract(&mut self, dest: &Path, opts: &Options, progress: &mut dyn Progress) -> Result<()> {
        self.walk(|entry| {
            let name = entry.name();
            match kind(entry.entry_type()) {
                Kind::File | Kind::Dir => {
                    let info = EntryInfo {
                        name: &name,
                        size: entry.size(),
                        is_dir: kind(entry.entry_type()) == Kind::Dir,
                    };
                    write_entry(dest, &info, opts, progress, entry.data())
                }
                Kind::Link => {
                    progress.warning(&format!("Skipped \"{name}\": links are not extracted."));
                    Ok(())
                }
                Kind::Special => {
                    progress.warning(&format!("Skipped \"{name}\": not a regular file."));
                    Ok(())
                }
                Kind::Metadata => Ok(()),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Entries
// ---------------------------------------------------------------------------

/// The bits of `tar::Entry<R>` we use, without its reader type, so one
/// callback serves both plain and compressed archives.
trait TarEntry {
    fn name(&self) -> String;
    fn size(&self) -> u64;
    fn entry_type(&self) -> EntryType;
    fn data(&mut self) -> &mut dyn Read;
}

impl<R: Read> TarEntry for tar::Entry<'_, R> {
    fn name(&self) -> String {
        // `path_bytes` already applies GNU long names and PAX `path` records.
        String::from_utf8_lossy(&self.path_bytes()).into_owned()
    }
    fn size(&self) -> u64 {
        tar::Entry::size(self)
    }
    fn entry_type(&self) -> EntryType {
        self.header().entry_type()
    }
    fn data(&mut self) -> &mut dyn Read {
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    File,
    Dir,
    Link,
    /// Devices, FIFOs: nothing a download should contain.
    Special,
    /// PAX global headers and such: information, not entries.
    Metadata,
}

impl Kind {
    /// For listing: `Some(is_dir)` for what can end up on disk.
    fn listed(self) -> Option<bool> {
        match self {
            Kind::File => Some(false),
            Kind::Dir => Some(true),
            Kind::Link | Kind::Special | Kind::Metadata => None,
        }
    }
}

fn kind(t: EntryType) -> Kind {
    match t {
        // Continuous is an old variant of a regular file; GNU sparse files
        // are expanded to their full contents by the `tar` crate.
        EntryType::Regular | EntryType::Continuous | EntryType::GNUSparse => Kind::File,
        EntryType::Directory => Kind::Dir,
        EntryType::Symlink | EntryType::Link => Kind::Link,
        EntryType::Char | EntryType::Block | EntryType::Fifo => Kind::Special,
        EntryType::XGlobalHeader
        | EntryType::XHeader
        | EntryType::GNULongName
        | EntryType::GNULongLink => Kind::Metadata,
        _ => Kind::Special,
    }
}

// ---------------------------------------------------------------------------
// Noticing truncation
// ---------------------------------------------------------------------------

/// Passes reads through and remembers whether the input ever ran out.
///
/// The `tar` crate stops at the first all-zero header block (the end
/// marker) without reading further, so `ran_out` stays false for a complete
/// archive. If it stopped because the input ran out at a header boundary,
/// `ran_out` is true: the archive was cut short.
struct Tracked<R> {
    inner: R,
    ran_out: bool,
}

impl<R> Tracked<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            ran_out: false,
        }
    }
}

impl<R: Read> Read for Tracked<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n == 0 && !buf.is_empty() {
            self.ran_out = true;
        }
        Ok(n)
    }
}

impl<R: Seek> Seek for Tracked<R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_kinds() {
        assert_eq!(kind(EntryType::Regular), Kind::File);
        assert_eq!(kind(EntryType::Directory), Kind::Dir);
        assert_eq!(kind(EntryType::Symlink), Kind::Link);
        assert_eq!(kind(EntryType::Link), Kind::Link);
        assert_eq!(kind(EntryType::Fifo), Kind::Special);
        assert_eq!(kind(EntryType::XGlobalHeader), Kind::Metadata);
    }

    #[test]
    fn tracked_notices_running_out() {
        let mut t = Tracked::new(&b"abc"[..]);
        let mut buf = [0u8; 2];
        assert_eq!(t.read(&mut buf).unwrap(), 2);
        assert!(!t.ran_out);
        assert_eq!(t.read(&mut buf).unwrap(), 1);
        assert_eq!(t.read(&mut buf).unwrap(), 0);
        assert!(t.ran_out);
    }
}
