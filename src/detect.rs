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
}

const RAR5: &[u8] = b"Rar!\x1a\x07\x01\x00";
const RAR4: &[u8] = b"Rar!\x1a\x07\x00";
const SEVEN_Z: &[u8] = b"7z\xbc\xaf\x27\x1c";
const ZIP_LOCAL: &[u8] = b"PK\x03\x04";
const ZIP_EMPTY: &[u8] = b"PK\x05\x06";
/// First four bytes of the first segment of a spanned ZIP (`.z01`).
const ZIP_SPANNED: &[u8] = b"PK\x07\x08";

/// Longest signature we check.
const PROBE_LEN: u64 = 8;

/// Identifies a format from the first bytes of a file.
pub fn from_bytes(head: &[u8]) -> Option<Format> {
    let table: [(&[u8], Format); 6] = [
        (RAR5, Format::Rar5), // check before RAR4: they share a prefix
        (RAR4, Format::Rar4),
        (SEVEN_Z, Format::SevenZ),
        (ZIP_LOCAL, Format::Zip),
        (ZIP_EMPTY, Format::Zip),
        (ZIP_SPANNED, Format::Zip),
    ];
    table
        .into_iter()
        .find(|(magic, _)| head.starts_with(magic))
        .map(|(_, format)| format)
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
    }

    #[test]
    fn rejects_unknown_and_short_input() {
        assert_eq!(from_bytes(b""), None);
        assert_eq!(from_bytes(b"Rar!"), None);
        assert_eq!(from_bytes(b"\x1a\x45\xdf\xa3"), None); // Matroska/MKV
        assert_eq!(from_bytes(b"MZ\x90\x00"), None); // .exe (SFX not handled yet)
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
