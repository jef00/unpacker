//! Reads just enough of a RAR file's headers to tell which volume of a set
//! it is, whatever the file is called.
//!
//! Telegram shortens long file names by cutting the end of the name before
//! the extension, which is exactly where `.part01` sits: every volume of a
//! set can arrive as the same `Title.rar`. The headers still know:
//!
//! - RAR5: the main header flags a volume and stores its number (absent on
//!   the first volume, which is number 0). The end header says whether
//!   another volume follows.
//! - RAR4 (RAR 3.x/4.x): the main header flags a volume, the first volume
//!   and `.partN` vs `.r00` naming. The end header holds the number and
//!   whether another volume follows.
//!
//! Each volume also says which file runs over its edges: a big file is cut
//! across volumes, and every piece's header repeats the file's name, size
//! and date. Matching one volume's last piece with the next volume's first
//! ties the parts of one archive together without trusting any name.
//!
//! Only headers are read, never file data (it's skipped by seeking), so this
//! is instant even for multi-GB parts. All input is untrusted: every size is
//! bounds-checked and the walk is capped.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const RAR5_SIG: &[u8] = b"Rar!\x1a\x07\x01\x00";
const RAR4_SIG: &[u8] = b"Rar!\x1a\x07\x00";

/// RAR5 caps a header at 2 MB; anything bigger is damage.
const MAX_HEADER: u64 = 2 * 1024 * 1024;
/// More headers than any real volume has; stops a hostile walk.
const MAX_HEADERS: usize = 1_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identity {
    /// Not a RAR file, or a RAR archive that isn't split into volumes.
    NotAVolume,
    /// A RAR whose volume number can't be read: encrypted headers, a very
    /// old RAR version, or damage.
    Unknown,
    Volume(Volume),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    /// 0 for the first volume.
    pub number: u32,
    /// Whether this is the last volume; `None` if the end header couldn't
    /// be reached: the file is cut off (still downloading, or broken).
    pub last: Option<bool>,
    /// `.part01.rar` naming (`true`) or old-style `.rar`, `.r00`, … (`false`).
    pub new_numbering: bool,
    /// The file running in from the previous volume.
    pub head: Edge,
    /// The file running on into the next volume.
    pub tail: Edge,
}

impl Volume {
    /// Whether the whole volume is there: its end header was read.
    pub fn complete(&self) -> bool {
        self.last.is_some()
    }
}

/// What a volume says about one of its edges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edge {
    /// Not known: the headers there couldn't be read.
    Unknown,
    /// No file crosses this edge.
    Clean,
    /// This file is cut here and continues on the other side.
    Split(SplitFile),
}

/// A file cut across volumes, as every piece's header describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitFile {
    pub name: Vec<u8>,
    /// The whole file's unpacked size, the same in every piece.
    pub size: u64,
    pub mtime: Option<u32>,
}

/// Whether volume `a` and the volume right after it, `b`, belong together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Link {
    /// The file cut at the end of `a` continues at the start of `b`.
    Match,
    /// They disagree: `b` is from another archive.
    Mismatch,
    /// Nothing to compare (no file crosses the edge, or it couldn't be read).
    Unknown,
}

pub fn link(a_tail: &Edge, b_head: &Edge) -> Link {
    match (a_tail, b_head) {
        (Edge::Unknown, _) | (_, Edge::Unknown) | (Edge::Clean, Edge::Clean) => Link::Unknown,
        (Edge::Split(a), Edge::Split(b)) if a == b => Link::Match,
        _ => Link::Mismatch,
    }
}

/// Edges seen so far while walking a volume's headers.
#[derive(Default)]
struct Edges {
    /// Set by the first file header.
    head: Option<Edge>,
    /// The last file header seen: split after, or not.
    tail: Option<Edge>,
}

impl Edges {
    fn file(&mut self, split_before: bool, split_after: bool, file: SplitFile) {
        if self.head.is_none() {
            self.head = Some(if split_before {
                Edge::Split(file.clone())
            } else {
                Edge::Clean
            });
        }
        self.tail = Some(if split_after {
            Edge::Split(file)
        } else {
            Edge::Clean
        });
    }

    /// The end header was read: every file header was seen.
    fn finished(self) -> (Edge, Edge) {
        (
            self.head.unwrap_or(Edge::Clean),
            self.tail.unwrap_or(Edge::Clean),
        )
    }

    /// The walk stopped early: the tail is unknown, and so is the head if
    /// no file header was reached.
    fn cut_off(self) -> (Edge, Edge) {
        (self.head.unwrap_or(Edge::Unknown), Edge::Unknown)
    }
}

/// Identifies the file at `path`. Unreadable files are `NotAVolume`.
pub fn identify(path: &Path) -> Identity {
    match File::open(path) {
        Ok(mut file) => identify_reader(&mut file),
        Err(_) => Identity::NotAVolume,
    }
}

pub fn identify_reader<R: Read + Seek>(r: &mut R) -> Identity {
    let Ok(len) = r.seek(SeekFrom::End(0)) else {
        return Identity::NotAVolume;
    };
    let Some(sig) = read_at(r, 0, RAR5_SIG.len() as u64, len) else {
        return Identity::NotAVolume;
    };
    if sig.starts_with(RAR5_SIG) {
        rar5(r, len)
    } else if sig.starts_with(RAR4_SIG) {
        rar4(r, len)
    } else {
        Identity::NotAVolume
    }
}

// ---------------------------------------------------------------------------
// RAR5
// ---------------------------------------------------------------------------

const RAR5_MAIN: u64 = 1;
const RAR5_FILE: u64 = 2;
const RAR5_ENCRYPTION: u64 = 4;
const RAR5_END: u64 = 5;

/// Header flags: data continues from the previous / into the next volume.
const RAR5_SPLIT_BEFORE: u64 = 0x08;
const RAR5_SPLIT_AFTER: u64 = 0x10;

fn rar5<R: Read + Seek>(r: &mut R, len: u64) -> Identity {
    let mut pos = RAR5_SIG.len() as u64;
    let mut number = None;
    let mut edges = Edges::default();

    for _ in 0..MAX_HEADERS {
        let Some(header) = rar5_header(r, pos, len) else {
            break;
        };
        let mut body = Bytes(&header.body);
        match header.kind {
            // The archive is encrypted from here on (`rar -hp`): the number
            // is inside the encrypted part.
            RAR5_ENCRYPTION => return Identity::Unknown,
            RAR5_MAIN => {
                let Some(flags) = body.vint() else {
                    return Identity::Unknown;
                };
                if flags & 0x1 == 0 {
                    return Identity::NotAVolume;
                }
                // Flag 0x2: the number is stored. The first volume omits it.
                number = Some(if flags & 0x2 != 0 {
                    match body.vint().and_then(|n| u32::try_from(n).ok()) {
                        Some(n) => n,
                        None => return Identity::Unknown,
                    }
                } else {
                    0
                });
            }
            RAR5_FILE => {
                if let Some(file) = rar5_file(&mut body) {
                    edges.file(
                        header.flags & RAR5_SPLIT_BEFORE != 0,
                        header.flags & RAR5_SPLIT_AFTER != 0,
                        file,
                    );
                }
            }
            RAR5_END => {
                let not_last = body.vint().map(|flags| flags & 0x1 != 0);
                let (head, tail) = edges.finished();
                return match number {
                    Some(number) => Identity::Volume(Volume {
                        number,
                        last: not_last.map(|not_last| !not_last),
                        new_numbering: true,
                        head,
                        tail,
                    }),
                    None => Identity::Unknown,
                };
            }
            _ => {}
        }
        pos = header.next;
    }

    // Ran out before the end header: a cut-off file. The number is still
    // good if the main header was read.
    let (head, tail) = edges.cut_off();
    match number {
        Some(number) => Identity::Volume(Volume {
            number,
            last: None,
            new_numbering: true,
            head,
            tail,
        }),
        None => Identity::Unknown,
    }
}

/// File header fields: flags, unpacked size, attributes, [mtime], [CRC],
/// compression, host OS, name length, name.
fn rar5_file(body: &mut Bytes<'_>) -> Option<SplitFile> {
    let flags = body.vint()?;
    let size = body.vint()?;
    body.vint()?; // attributes
    let mtime = if flags & 0x2 != 0 {
        Some(body.u32()?)
    } else {
        None
    };
    if flags & 0x4 != 0 {
        body.u32()?; // CRC32 of this piece's data, differs per piece
    }
    body.vint()?; // compression
    body.vint()?; // host OS
    let name_len = usize::try_from(body.vint()?).ok()?;
    let name = body.take(name_len)?.to_vec();
    Some(SplitFile { name, size, mtime })
}

struct Rar5Header {
    kind: u64,
    flags: u64,
    /// Type-specific fields, after the common ones.
    body: Vec<u8>,
    /// Where the next header starts.
    next: u64,
}

/// Block layout: CRC32 (4), header size (vint), then `size` bytes holding
/// type, flags, [extra area size], [data size], fields. Data follows.
fn rar5_header<R: Read + Seek>(r: &mut R, pos: u64, len: u64) -> Option<Rar5Header> {
    let prefix =
        read_at(r, pos, 4 + 3, len).or_else(|| read_at(r, pos, len.checked_sub(pos)?, len))?;
    let mut size_field = Bytes(prefix.get(4..)?);
    let size = size_field.vint()?;
    if size == 0 || size > MAX_HEADER {
        return None;
    }
    let header_start = pos + 4 + size_field.consumed(prefix.len() - 4) as u64;
    let block = read_at(r, header_start, size, len)?;

    let mut c = Bytes(&block);
    let kind = c.vint()?;
    let flags = c.vint()?;
    if flags & 0x1 != 0 {
        c.vint()?; // extra area size: part of `size`, nothing to skip
    }
    let data_size = if flags & 0x2 != 0 { c.vint()? } else { 0 };
    let next = header_start.checked_add(size)?.checked_add(data_size)?;
    Some(Rar5Header {
        kind,
        flags,
        body: c.0.to_vec(),
        next,
    })
}

// ---------------------------------------------------------------------------
// RAR4 (RAR 3.x / 4.x)
// ---------------------------------------------------------------------------

const RAR4_MAIN: u8 = 0x73;
const RAR4_FILE: u8 = 0x74;
const RAR4_SUBBLOCK: u8 = 0x7A;
const RAR4_END: u8 = 0x7B;

const MHD_VOLUME: u16 = 0x0001;
const MHD_NEWNUMBERING: u16 = 0x0010;
const MHD_PASSWORD: u16 = 0x0080;
const MHD_FIRSTVOLUME: u16 = 0x0100;
const LHD_SPLIT_BEFORE: u16 = 0x0001;
const LHD_SPLIT_AFTER: u16 = 0x0002;
const LHD_LARGE: u16 = 0x0100;
const LONG_BLOCK: u16 = 0x8000;
const EARC_NEXT_VOLUME: u16 = 0x0001;
const EARC_DATACRC: u16 = 0x0002;
const EARC_VOLNUMBER: u16 = 0x0008;

fn rar4<R: Read + Seek>(r: &mut R, len: u64) -> Identity {
    let mut pos = RAR4_SIG.len() as u64;
    let mut main: Option<(bool, bool)> = None; // (first volume, new numbering)
    let mut edges = Edges::default();

    for _ in 0..MAX_HEADERS {
        // Base block: CRC (2), type (1), flags (2), size (2).
        let Some(base) = read_at(r, pos, 7, len) else {
            break;
        };
        let kind = base[2];
        let flags = u16::from_le_bytes([base[3], base[4]]);
        let size = u64::from(u16::from_le_bytes([base[5], base[6]]));
        if size < 7 {
            break;
        }
        let Some(header) = read_at(r, pos, size, len) else {
            break;
        };

        // Data after the header: file and sub-blocks store their packed
        // size; other long blocks an "add size".
        let data_size = match kind {
            RAR4_FILE | RAR4_SUBBLOCK => {
                let low = u64::from(u32_at(&header, 7).unwrap_or(0));
                let high = if flags & LHD_LARGE != 0 {
                    u64::from(u32_at(&header, 32).unwrap_or(0))
                } else {
                    0
                };
                (high << 32) | low
            }
            _ if flags & LONG_BLOCK != 0 => u64::from(u32_at(&header, 7).unwrap_or(0)),
            _ => 0,
        };

        match kind {
            RAR4_MAIN => {
                if flags & MHD_VOLUME == 0 {
                    return Identity::NotAVolume;
                }
                let first = flags & MHD_FIRSTVOLUME != 0;
                let new_numbering = flags & MHD_NEWNUMBERING != 0;
                // Encrypted headers: nothing after this can be read. Only
                // "first volume" is known.
                if flags & MHD_PASSWORD != 0 {
                    return if first {
                        Identity::Volume(Volume {
                            number: 0,
                            last: None,
                            new_numbering,
                            head: Edge::Unknown,
                            tail: Edge::Unknown,
                        })
                    } else {
                        Identity::Unknown
                    };
                }
                main = Some((first, new_numbering));
            }
            RAR4_FILE => {
                if let Some(file) = rar4_file(&header, flags) {
                    edges.file(
                        flags & LHD_SPLIT_BEFORE != 0,
                        flags & LHD_SPLIT_AFTER != 0,
                        file,
                    );
                }
            }
            RAR4_END => {
                let Some((first, new_numbering)) = main else {
                    return Identity::Unknown;
                };
                let last = Some(flags & EARC_NEXT_VOLUME == 0);
                let number_at = 7 + if flags & EARC_DATACRC != 0 { 4 } else { 0 };
                let stored = (flags & EARC_VOLNUMBER != 0)
                    .then(|| header.get(number_at..number_at + 2))
                    .flatten()
                    .map(|b| u32::from(u16::from_le_bytes([b[0], b[1]])));
                let (head, tail) = edges.finished();
                let number = match (first, stored) {
                    (true, _) => 0,
                    (false, Some(number)) => number,
                    (false, None) => return Identity::Unknown, // RAR before 3.0
                };
                return Identity::Volume(Volume {
                    number,
                    last,
                    new_numbering,
                    head,
                    tail,
                });
            }
            _ => {}
        }

        let Some(next) = pos.checked_add(size).and_then(|p| p.checked_add(data_size)) else {
            break;
        };
        pos = next;
    }

    // No end header reached. Only the first volume is known without one.
    let (head, tail) = edges.cut_off();
    match main {
        Some((true, new_numbering)) => Identity::Volume(Volume {
            number: 0,
            last: None,
            new_numbering,
            head,
            tail,
        }),
        _ => Identity::Unknown,
    }
}

/// File header: packed size @7, unpacked size @11, mtime @20, name size
/// @26, then the high halves of both sizes (large files) and the name.
fn rar4_file(header: &[u8], flags: u16) -> Option<SplitFile> {
    let low = u64::from(u32_at(header, 11)?);
    let mtime = u32_at(header, 20)?;
    let name_size = usize::from(u16::from_le_bytes(header.get(26..28)?.try_into().ok()?));
    let (high, name_at): (u64, usize) = if flags & LHD_LARGE != 0 {
        (u64::from(u32_at(header, 36)?), 40)
    } else {
        (0, 32)
    };
    let name = header
        .get(name_at..name_at.checked_add(name_size)?)?
        .to_vec();
    Some(SplitFile {
        name,
        size: (high << 32) | low,
        mtime: Some(mtime),
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Reads exactly `n` bytes at `pos`, or `None` if they aren't all there.
fn read_at<R: Read + Seek>(r: &mut R, pos: u64, n: u64, len: u64) -> Option<Vec<u8>> {
    if n > MAX_HEADER || pos.checked_add(n)? > len {
        return None;
    }
    r.seek(SeekFrom::Start(pos)).ok()?;
    let mut buf = vec![0u8; usize::try_from(n).ok()?];
    r.read_exact(&mut buf).ok()?;
    Some(buf)
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4)
        .and_then(|s| s.try_into().ok())
        .map(u32::from_le_bytes)
}

/// Reads RAR5 variable-length integers from a byte slice.
struct Bytes<'a>(&'a [u8]);

impl Bytes<'_> {
    /// 7 bits per byte, low bits first, high bit set on all but the last.
    fn vint(&mut self) -> Option<u64> {
        let mut value = 0u64;
        for (i, &byte) in self.0.iter().enumerate().take(10) {
            value |= u64::from(byte & 0x7F) << (7 * i);
            if byte & 0x80 == 0 {
                self.0 = &self.0[i + 1..];
                return Some(value);
            }
        }
        None
    }

    fn u32(&mut self) -> Option<u32> {
        let bytes = self.take(4)?;
        Some(u32::from_le_bytes(bytes.try_into().ok()?))
    }

    fn take(&mut self, n: usize) -> Option<&[u8]> {
        if n > self.0.len() {
            return None;
        }
        let (taken, rest) = self.0.split_at(n);
        self.0 = rest;
        Some(taken)
    }

    /// How many bytes were read, given the slice length it started with.
    fn consumed(&self, started_with: usize) -> usize {
        started_with - self.0.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor as IoCursor;

    fn fixture(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/rar")
            .join(name)
    }

    /// Number, last flag and naming style, without the edges.
    fn vol(number: u32, last: bool, new_numbering: bool) -> Option<(u32, Option<bool>, bool)> {
        Some((number, Some(last), new_numbering))
    }

    fn id(identity: Identity) -> Option<(u32, Option<bool>, bool)> {
        match identity {
            Identity::Volume(v) => Some((v.number, v.last, v.new_numbering)),
            _ => None,
        }
    }

    fn volume(name: &str) -> Volume {
        match identify(&fixture(name)) {
            Identity::Volume(v) => v,
            other => panic!("{name}: {other:?}"),
        }
    }

    #[test]
    fn rar5_volumes() {
        assert_eq!(
            id(identify(&fixture("rar5-vols.part1.rar"))),
            vol(0, false, true)
        );
        assert_eq!(
            id(identify(&fixture("rar5-vols.part2.rar"))),
            vol(1, false, true)
        );
        assert_eq!(
            id(identify(&fixture("rar5-vols.part3.rar"))),
            vol(2, true, true)
        );
    }

    #[test]
    fn rar4_new_style_volumes() {
        assert_eq!(
            id(identify(&fixture("rar3-vols.part1.rar"))),
            vol(0, false, true)
        );
        assert_eq!(
            id(identify(&fixture("rar3-vols.part2.rar"))),
            vol(1, false, true)
        );
        assert_eq!(
            id(identify(&fixture("rar3-vols.part3.rar"))),
            vol(2, true, true)
        );
    }

    #[test]
    fn rar4_old_style_volumes() {
        assert_eq!(id(identify(&fixture("rar3-old.rar"))), vol(0, false, false));
        assert_eq!(id(identify(&fixture("rar3-old.r00"))), vol(1, false, false));
        assert_eq!(id(identify(&fixture("rar3-old.r01"))), vol(2, true, false));
    }

    #[test]
    fn single_archives_and_other_files_are_not_volumes() {
        assert_eq!(identify(&fixture("rar5-solid.rar")), Identity::NotAVolume);
        assert_eq!(identify(&fixture("rar3-solid.rar")), Identity::NotAVolume);
        assert_eq!(identify(&fixture("../7z/copy.7z")), Identity::NotAVolume);
        assert_eq!(
            identify(Path::new("/does/not/exist.rar")),
            Identity::NotAVolume
        );
    }

    #[test]
    fn encrypted_headers_are_unknown() {
        // Not a volume set at all, but with `-hp` even that can't be read.
        assert_eq!(identify(&fixture("rar5-hpsw.rar")), Identity::Unknown);
    }

    #[test]
    fn cut_off_volume_keeps_its_number() {
        let bytes = std::fs::read(fixture("rar5-vols.part2.rar")).unwrap();
        let mut cut = IoCursor::new(bytes[..200].to_vec());
        let Identity::Volume(v) = identify_reader(&mut cut) else {
            panic!("not a volume");
        };
        assert_eq!((v.number, v.last, v.complete()), (1, None, false));
        // The first file header made it, the end didn't.
        assert!(matches!(v.head, Edge::Split(_)), "{:?}", v.head);
        assert_eq!(v.tail, Edge::Unknown);
    }

    #[test]
    fn neighbouring_parts_agree_on_the_file_between_them() {
        for set in ["rar5-vols", "rar3-vols"] {
            let [p1, p2, p3] = [1, 2, 3].map(|n| volume(&format!("{set}.part{n}.rar")));
            assert_eq!(p1.head, Edge::Clean, "{set}");
            assert_eq!(link(&p1.tail, &p2.head), Link::Match, "{set}");
            assert_eq!(link(&p2.tail, &p3.head), Link::Match, "{set}");
            assert_eq!(p3.tail, Edge::Clean, "{set}");
            assert!(p1.complete() && p3.complete());
        }
        // Old-style naming, same rules.
        let (a, b) = (volume("rar3-old.rar"), volume("rar3-old.r00"));
        assert_eq!(link(&a.tail, &b.head), Link::Match);
    }

    #[test]
    fn parts_of_different_archives_disagree() {
        // Same file name and size inside, but packed at different times.
        let a = volume("rar5-vols.part1.rar");
        let b = volume("rar3-vols.part2.rar");
        assert_eq!(link(&a.tail, &b.head), Link::Mismatch);
        // A part that starts fresh can't follow one whose file runs on.
        assert_eq!(link(&a.tail, &Edge::Clean), Link::Mismatch);
        assert_eq!(link(&Edge::Clean, &Edge::Clean), Link::Unknown);
        assert_eq!(link(&Edge::Unknown, &b.head), Link::Unknown);
    }

    #[test]
    fn garbage_after_a_signature_never_panics() {
        for sig in [RAR5_SIG, RAR4_SIG] {
            for seed in 0..300u32 {
                let mut bytes = sig.to_vec();
                let mut x = seed.wrapping_mul(2_654_435_761);
                for _ in 0..(seed % 97) {
                    x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
                    bytes.push((x >> 16) as u8);
                }
                let _ = identify_reader(&mut IoCursor::new(bytes));
            }
        }
    }

    #[test]
    fn vint_decoding() {
        assert_eq!(Bytes(&[0x05]).vint(), Some(5));
        assert_eq!(Bytes(&[0x80 | 0x01, 0x02]).vint(), Some(1 | (2 << 7)));
        assert_eq!(Bytes(&[0x80, 0x80]).vint(), None, "unterminated");
        assert_eq!(Bytes(&[0xFF; 11]).vint(), None, "too long");
    }
}
