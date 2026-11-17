//! Archive format detection by magic bytes. Extensions lie; headers don't.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Rar4,
    Rar5,
    SevenZ,
    Zip,
    /// An uncompressed tar archive.
    Tar,
    /// A compressed stream. Only a tar inside is supported (`.tar.gz` etc.);
    /// the tar backend checks that once it has decompressed the start.
    Compressed(Codec),
}

/// Compression formats used around tar archives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Gzip,
    Bzip2,
    Xz,
    Zstd,
}

const RAR5: &[u8] = b"Rar!\x1a\x07\x01\x00";
const RAR4: &[u8] = b"Rar!\x1a\x07\x00";
const SEVEN_Z: &[u8] = b"7z\xbc\xaf\x27\x1c";
const ZIP_LOCAL: &[u8] = b"PK\x03\x04";
const ZIP_EMPTY: &[u8] = b"PK\x05\x06";
/// First four bytes of the first segment of a spanned ZIP (`.z01`).
const ZIP_SPANNED: &[u8] = b"PK\x07\x08";

const GZIP: &[u8] = b"\x1f\x8b";
const XZ: &[u8] = b"\xfd7zXZ\x00";
const ZSTD: &[u8] = b"\x28\xb5\x2f\xfd";

/// A tar header block; its magic sits at offset 257.
pub const TAR_BLOCK: usize = 512;

/// Enough to see one whole tar header.
const PROBE_LEN: u64 = TAR_BLOCK as u64;

/// Identifies a format from the first bytes of a file.
pub fn from_bytes(head: &[u8]) -> Option<Format> {
    let table: [(&[u8], Format); 9] = [
        (RAR5, Format::Rar5), // check before RAR4: they share a prefix
        (RAR4, Format::Rar4),
        (SEVEN_Z, Format::SevenZ),
        (ZIP_LOCAL, Format::Zip),
        (ZIP_EMPTY, Format::Zip),
        (ZIP_SPANNED, Format::Zip),
        (GZIP, Format::Compressed(Codec::Gzip)),
        (XZ, Format::Compressed(Codec::Xz)),
        (ZSTD, Format::Compressed(Codec::Zstd)),
    ];
    let by_magic = table
        .into_iter()
        .find(|(magic, _)| head.starts_with(magic))
        .map(|(_, format)| format);
    by_magic
        .or_else(|| is_bzip2(head).then_some(Format::Compressed(Codec::Bzip2)))
        .or_else(|| is_tar_header(head).then_some(Format::Tar))
}

/// `BZh` plus the block size digit `1`–`9`: "BZh" alone is too short a
/// magic to trust (it's also how some text starts).
fn is_bzip2(head: &[u8]) -> bool {
    matches!(head, [b'B', b'Z', b'h', b'1'..=b'9', ..])
}

/// Whether `block` starts with a tar header: POSIX/GNU tar have "ustar" at
/// offset 257; old (v7) tar has no magic, so its header checksum must add up.
/// An all-zero block is an empty archive's end marker.
pub fn is_tar_header(block: &[u8]) -> bool {
    let Some(block) = block.get(..TAR_BLOCK) else {
        return false;
    };
    if &block[257..262] == b"ustar" || block.iter().all(|&b| b == 0) {
        return true;
    }
    // Checksum: all header bytes summed, with the checksum field as spaces.
    let sum: u32 = block
        .iter()
        .enumerate()
        .map(|(i, &b)| {
            if (148..156).contains(&i) {
                u32::from(b' ')
            } else {
                u32::from(b)
            }
        })
        .sum();
    let stored = std::str::from_utf8(&block[148..156])
        .ok()
        .map(|s| s.trim_matches(|c| c == ' ' || c == '\0'))
        .and_then(|s| u32::from_str_radix(s, 8).ok());
    block[0] != 0 && stored == Some(sum)
}

/// Reads the start of `path` and identifies it.
///
/// For any volume set, pass `set.first()`: the archive header always lives
/// in the first part, including for `.001` splits.
pub fn from_file(path: &Path) -> Result<Format> {
    let mut head = Vec::with_capacity(PROBE_LEN as usize);
    File::open(path)?.take(PROBE_LEN).read_to_end(&mut head)?;
    from_bytes(&head).ok_or_else(|| Error::UnknownFormat(path.to_path_buf()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_every_signature() {
        assert_eq!(from_bytes(b"Rar!\x1a\x07\x01\x00rest"), Some(Format::Rar5));
        assert_eq!(from_bytes(b"Rar!\x1a\x07\x00rest"), Some(Format::Rar4));
        assert_eq!(
            from_bytes(b"7z\xbc\xaf\x27\x1c\x00\x04"),
            Some(Format::SevenZ)
        );
        assert_eq!(from_bytes(b"PK\x03\x04\x14\x00"), Some(Format::Zip));
        assert_eq!(from_bytes(b"PK\x05\x06"), Some(Format::Zip));
        assert_eq!(from_bytes(b"PK\x07\x08PK\x03\x04"), Some(Format::Zip));
        let gz = Format::Compressed(Codec::Gzip);
        assert_eq!(from_bytes(b"\x1f\x8b\x08\x00"), Some(gz));
        assert_eq!(
            from_bytes(b"BZh91AY&SY"),
            Some(Format::Compressed(Codec::Bzip2))
        );
        assert_eq!(
            from_bytes(b"\xfd7zXZ\x00\x00"),
            Some(Format::Compressed(Codec::Xz))
        );
        assert_eq!(
            from_bytes(b"\x28\xb5\x2f\xfd\x04"),
            Some(Format::Compressed(Codec::Zstd))
        );
    }

    /// A minimal tar header for `name`, with or without the "ustar" magic.
    fn tar_header(name: &str, ustar: bool) -> Vec<u8> {
        let mut h = vec![0u8; TAR_BLOCK];
        h[..name.len()].copy_from_slice(name.as_bytes());
        h[100..108].copy_from_slice(b"0000644\0");
        h[124..136].copy_from_slice(b"00000000005\0");
        h[156] = b'0';
        if ustar {
            h[257..263].copy_from_slice(b"ustar\0");
        }
        let sum: u32 = h.iter().map(|&b| u32::from(b)).sum::<u32>() + 8 * u32::from(b' ');
        h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        h
    }

    #[test]
    fn recognises_tar_with_and_without_magic() {
        assert_eq!(from_bytes(&tar_header("a.txt", true)), Some(Format::Tar));
        assert_eq!(
            from_bytes(&tar_header("a.txt", false)),
            Some(Format::Tar),
            "v7 tar"
        );
        assert!(is_tar_header(&[0u8; TAR_BLOCK]), "empty archive");

        let mut bad = tar_header("a.txt", false);
        bad[0] = b'b'; // checksum no longer adds up, and no magic
        assert_eq!(from_bytes(&bad), None);
        assert!(
            !is_tar_header(&tar_header("a.txt", true)[..300]),
            "too short"
        );
    }

    #[test]
    fn rejects_unknown_and_short_input() {
        assert_eq!(from_bytes(b""), None);
        assert_eq!(from_bytes(b"Rar!"), None);
        assert_eq!(from_bytes(b"\x1a\x45\xdf\xa3"), None); // Matroska/MKV
        assert_eq!(from_bytes(b"MZ\x90\x00"), None); // .exe (SFX not handled yet)
        assert_eq!(from_bytes(b"BZh is how this text starts"), None);
    }

    #[test]
    fn from_file_reports_unknown_format() {
        let dir = std::env::temp_dir().join(format!("unpacker-detect-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("not-an-archive.txt");
        std::fs::write(&path, b"hello").unwrap();

        assert!(matches!(from_file(&path), Err(Error::UnknownFormat(_))));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
