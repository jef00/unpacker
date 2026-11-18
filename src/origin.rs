//! Mark of the Web: the "this came from the internet" tag.
//!
//! Browsers and Telegram Desktop tag what they download. On Windows the tag
//! is an NTFS stream called `Zone.Identifier`; on macOS it's the
//! `com.apple.quarantine` attribute. Windows warns before running a tagged
//! program and opens tagged Office files in Protected View. macOS
//! Gatekeeper checks tagged programs.
//!
//! Files we unpack are new files, so they'd have no tag: a fake
//! `movie.mkv.exe` from a downloaded archive would run without a warning.
//! Like WinRAR and 7-Zip, we copy the archive's tag onto every file we unpack.
//! Other systems have no such tag, so there this does nothing.

use std::io;
use std::path::{Path, PathBuf};

/// The tag of a downloaded archive, to be copied onto what we unpack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    mark: Vec<u8>,
}

impl Origin {
    /// The archive's tag, or `None` if it has none (made locally, copied
    /// from a USB stick, or a system without tags). For a set of parts the
    /// strictest tag wins, so one part from the internet is enough.
    pub fn of(parts: &[PathBuf]) -> Option<Self> {
        strictest(parts.iter().filter_map(|part| platform::read(part))).map(|mark| Self { mark })
    }

    /// Tags `file`. It must be a regular file we just created.
    pub fn apply(&self, file: &Path) -> io::Result<()> {
        platform::write(file, &self.mark)
    }
}

/// Picks the strictest tag: the highest Windows `ZoneId` (3 = Internet,
/// 4 = Restricted). A tag without a readable ZoneId counts as Internet, the
/// safe guess. macOS tags carry no ZoneId, so the first one wins there.
fn strictest(marks: impl Iterator<Item = Vec<u8>>) -> Option<Vec<u8>> {
    let mut best: Option<(u32, Vec<u8>)> = None;
    for mark in marks {
        let zone = zone_id(&mark).unwrap_or(3);
        if best.as_ref().is_none_or(|(b, _)| zone > *b) {
            best = Some((zone, mark));
        }
    }
    best.map(|(_, mark)| mark)
}

/// `ZoneId=3` from a `Zone.Identifier` stream (`[ZoneTransfer]` section).
fn zone_id(mark: &[u8]) -> Option<u32> {
    String::from_utf8_lossy(mark).lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        key.trim()
            .eq_ignore_ascii_case("ZoneId")
            .then(|| value.trim().parse().ok())?
    })
}

#[cfg(windows)]
mod platform {
    use std::ffi::OsString;
    use std::fs::{self, File};
    use std::io::{self, Read};
    use std::path::{Path, PathBuf};

    /// More than any real tag needs; we never read an unbounded stream.
    const MAX_MARK: u64 = 64 * 1024;

    /// `movie.rar` → `movie.rar:Zone.Identifier`, which Windows opens as the
    /// file's hidden stream of that name.
    fn stream(path: &Path) -> PathBuf {
        let mut name = OsString::from(path.as_os_str());
        name.push(":Zone.Identifier");
        PathBuf::from(name)
    }

    pub(super) fn read(path: &Path) -> Option<Vec<u8>> {
        let mut mark = Vec::new();
        File::open(stream(path))
            .ok()?
            .take(MAX_MARK)
            .read_to_end(&mut mark)
            .ok()?;
        (!mark.is_empty()).then_some(mark)
    }

    pub(super) fn write(path: &Path, mark: &[u8]) -> io::Result<()> {
        fs::write(stream(path), mark)
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::io;
    use std::path::Path;

    /// More than any real tag needs.
    const MAX_MARK: usize = 64 * 1024;
    const QUARANTINE: &str = "com.apple.quarantine";

    pub(super) fn read(path: &Path) -> Option<Vec<u8>> {
        let mark = xattr::get(path, QUARANTINE).ok()??;
        (!mark.is_empty() && mark.len() <= MAX_MARK).then_some(mark)
    }

    pub(super) fn write(path: &Path, mark: &[u8]) -> io::Result<()> {
        xattr::set(path, QUARANTINE, mark)
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod platform {
    use std::io;
    use std::path::Path;

    pub(super) fn read(_path: &Path) -> Option<Vec<u8>> {
        None
    }

    pub(super) fn write(_path: &Path, _mark: &[u8]) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zone(n: u32) -> Vec<u8> {
        format!("[ZoneTransfer]\r\nZoneId={n}\r\nHostUrl=https://t.me/\r\n").into_bytes()
    }

    #[test]
    fn reads_the_zone_id() {
        assert_eq!(zone_id(&zone(3)), Some(3));
        assert_eq!(zone_id(b"[ZoneTransfer]\nzoneid = 4\n"), Some(4));
        assert_eq!(zone_id(b"[ZoneTransfer]\nHostUrl=x\n"), None);
        assert_eq!(zone_id(b"0083;65f1a2b3;Telegram;"), None); // a macOS tag
    }

    #[test]
    fn the_strictest_tag_wins() {
        let picked = strictest([zone(2), zone(4), zone(3)].into_iter());
        assert_eq!(picked, Some(zone(4)));
        // Unreadable counts as Internet (3): stricter than Trusted (2).
        let unreadable = b"[ZoneTransfer]\n".to_vec();
        let picked = strictest([zone(2), unreadable.clone()].into_iter());
        assert_eq!(picked, Some(unreadable));
        // Equal: the first one stays.
        let a = b"0083;1;Telegram;A".to_vec();
        let b = b"0083;1;Safari;B".to_vec();
        assert_eq!(strictest([a.clone(), b].into_iter()), Some(a));
        assert_eq!(strictest(std::iter::empty()), None);
    }
}
