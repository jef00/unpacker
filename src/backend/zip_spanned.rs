//! Spanned ZIP support (`x.z01`, `x.z02`, …, `x.zip`).
//!
//! A spanned ZIP is one archive cut into "disks", but unlike a plain `.001`
//! split, every offset in it is *relative to the disk it points into*, paired
//! with a disk number. Gluing the parts together therefore isn't enough; the
//! `zip` crate (like most readers) rejects multi-disk archives outright.
//!
//! So we present a rewritten, single-disk view:
//!
//! ```text
//!   [ all parts glued, up to the original central directory ]  ← read from disk
//!   [ rewritten central directory: absolute offsets, disk 0  ]  ← in memory
//!   [ new end records (ZIP64 if needed), comment preserved    ]  ← in memory
//! ```
//!
//! Absolute offset = (where that disk starts in the glued stream) + (relative
//! offset). The `PK\x07\x08` marker at the start of `.z01` stays in place:
//! disk 0's offsets already count it.
//!
//! ZIP64 matters even for small parts: three 2 GB parts make offsets past
//! 4 GB, which only fit in a ZIP64 extra field. Entries get one when needed.
//!
//! Format reference: PKWARE APPNOTE.TXT, sections 4.3.12–4.3.16 and 4.5.3.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use crate::discovery::{Scheme, VolumeSet};
use crate::error::{Error, Result};
use crate::multi_reader::MultiReader;

const CENTRAL_HEADER_SIG: u32 = 0x0201_4b50;
const EOCD_SIG: u32 = 0x0605_4b50;
const ZIP64_EOCD_SIG: u32 = 0x0606_4b50;
const ZIP64_LOCATOR_SIG: u32 = 0x0706_4b50;

const CENTRAL_HEADER_LEN: usize = 46;
const EOCD_LEN: usize = 22;
const ZIP64_EOCD_LEN: usize = 56;
const ZIP64_LOCATOR_LEN: usize = 20;
/// The EOCD comment is at most 64 KiB, so the EOCD starts within this window.
const EOCD_SEARCH_WINDOW: u64 = EOCD_LEN as u64 + 0xFFFF;

const ZIP64_EXTRA_ID: u16 = 0x0001;
const MAX16: u16 = 0xFFFF;
const MAX32: u32 = 0xFFFF_FFFF;

/// Opens a spanned set as a seekable single-disk ZIP stream.
pub(crate) fn open(set: &VolumeSet) -> Result<Spliced<MultiReader<File>>> {
    let mut glued = MultiReader::open(&set.parts)?;
    let disk_starts = glued.part_starts().to_vec();
    let end = read_end_records(&mut glued, &disk_starts)?;

    // The `.zip` is the last disk. If it says it's disk 4 but we only have
    // three parts, the trailing `.z03`/`.z04` are missing. (Gaps in the
    // middle were already caught by discovery.)
    let expected_parts = end.this_disk as usize + 1;
    if set.parts.len() < expected_parts {
        return Err(Error::MissingVolume(format!(
            "{}.z{:02}",
            set.base,
            set.parts.len()
        )));
    }

    let cd_start = disk_start(&disk_starts, end.cd_disk)? + end.cd_offset;
    let mut cd = vec![
        0u8;
        usize::try_from(end.cd_size)
            .map_err(|_| corrupt("central directory too large"))?
    ];
    glued.seek(SeekFrom::Start(cd_start))?;
    glued
        .read_exact(&mut cd)
        .map_err(|_| corrupt("central directory runs past the end"))?;

    let tail = rewrite(&cd, end.entries, &disk_starts, cd_start, &end.comment)?;
    Ok(Spliced::new(glued, cd_start, tail))
}

/// Whether `set` is a lone `x.zip` that is really the last disk of a
/// spanned set, so we can say "part `x.z01` is missing" instead of "not a
/// valid archive".
pub(crate) fn lone_last_disk(set: &VolumeSet) -> bool {
    let is_zip = Path::new(set.first())
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("zip"));
    if set.scheme != Scheme::Single || !is_zip {
        return false;
    }
    let Ok(mut file) = File::open(set.first()) else {
        return false;
    };
    // The ZIP64 record may live on another disk we don't have, so fall back
    // to the classic EOCD's disk number.
    read_end_records(&mut file, &[0]).is_ok_and(|end| end.this_disk > 0)
        || eocd_disk_number(&mut file).is_ok_and(|disk| disk > 0)
}

// ---------------------------------------------------------------------------
// Reading the end records
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct EndRecords {
    this_disk: u32,
    cd_disk: u32,
    entries: u64,
    cd_size: u64,
    /// Relative to the start of `cd_disk`.
    cd_offset: u64,
    comment: Vec<u8>,
}

/// Locates the EOCD (and ZIP64 records if present) at the end of the stream.
fn read_end_records<R: Read + Seek>(r: &mut R, disk_starts: &[u64]) -> Result<EndRecords> {
    let (eocd_pos, window, window_start) = find_eocd(r)?;
    let at = usize::try_from(eocd_pos - window_start).map_err(|_| corrupt("EOCD"))?;
    let eocd = &window[at..];

    let this_disk = u16_at(eocd, 4)?;
    let cd_disk = u16_at(eocd, 6)?;
    let entries = u16_at(eocd, 10)?;
    let cd_size = u32_at(eocd, 12)?;
    let cd_offset = u32_at(eocd, 16)?;
    let comment_len = usize::from(u16_at(eocd, 20)?);
    let comment = eocd
        .get(EOCD_LEN..EOCD_LEN + comment_len)
        .ok_or_else(|| corrupt("EOCD comment"))?
        .to_vec();

    let locator = at
        .checked_sub(ZIP64_LOCATOR_LEN)
        .and_then(|l| window.get(l..at))
        .filter(|l| u32_at(l, 0).ok() == Some(ZIP64_LOCATOR_SIG));

    let Some(locator) = locator else {
        return Ok(EndRecords {
            this_disk: this_disk.into(),
            cd_disk: cd_disk.into(),
            entries: entries.into(),
            cd_size: cd_size.into(),
            cd_offset: cd_offset.into(),
            comment,
        });
    };

    // ZIP64: the locator says on which disk, and where, the real record is.
    let record_disk = u32_at(locator, 4)?;
    let record_pos = disk_start(disk_starts, record_disk)? + u64_at(locator, 8)?;
    let mut record = [0u8; ZIP64_EOCD_LEN];
    r.seek(SeekFrom::Start(record_pos))?;
    r.read_exact(&mut record)
        .map_err(|_| corrupt("ZIP64 end record"))?;
    if u32_at(&record, 0)? != ZIP64_EOCD_SIG {
        return Err(corrupt("ZIP64 end record signature"));
    }
    Ok(EndRecords {
        this_disk: u32_at(&record, 16)?,
        cd_disk: u32_at(&record, 20)?,
        entries: u64_at(&record, 32)?,
        cd_size: u64_at(&record, 40)?,
        cd_offset: u64_at(&record, 48)?,
        comment,
    })
}

/// Returns (EOCD position, the window read from the end, window start).
fn find_eocd<R: Read + Seek>(r: &mut R) -> Result<(u64, Vec<u8>, u64)> {
    let len = r.seek(SeekFrom::End(0))?;
    let window_start = len.saturating_sub(EOCD_SEARCH_WINDOW);
    let mut window = Vec::new();
    r.seek(SeekFrom::Start(window_start))?;
    r.read_to_end(&mut window)?;

    // Search backwards; prefer a record whose comment ends exactly at EOF.
    let candidates = (0..window.len().saturating_sub(EOCD_LEN - 1))
        .rev()
        .filter(|&i| u32_at(&window, i).ok() == Some(EOCD_SIG));
    let mut fallback = None;
    for i in candidates {
        let comment_len = u16_at(&window, i + 20).map_or(usize::MAX, usize::from);
        if i + EOCD_LEN + comment_len == window.len() {
            return Ok((window_start + i as u64, window, window_start));
        }
        fallback.get_or_insert(i);
    }
    match fallback {
        Some(i) => Ok((window_start + i as u64, window, window_start)),
        None => Err(corrupt("no end-of-central-directory record found")),
    }
}

fn eocd_disk_number<R: Read + Seek>(r: &mut R) -> Result<u16> {
    let (pos, window, start) = find_eocd(r)?;
    let at = usize::try_from(pos - start).map_err(|_| corrupt("EOCD"))?;
    u16_at(&window, at + 4)
}

// ---------------------------------------------------------------------------
// Rewriting
// ---------------------------------------------------------------------------

/// Rewrites the central directory with absolute offsets on disk 0, and
/// appends fresh end records. `new_cd_offset` is where it will sit in the
/// spliced stream.
fn rewrite(
    cd: &[u8],
    entries: u64,
    disk_starts: &[u64],
    new_cd_offset: u64,
    comment: &[u8],
) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(cd.len() + 1024);
    let mut pos = 0usize;
    for _ in 0..entries {
        pos += rewrite_entry(&cd[pos.min(cd.len())..], disk_starts, &mut out)?;
    }
    let cd_size = out.len() as u64;
    write_end_records(&mut out, entries, cd_size, new_cd_offset, comment)?;
    Ok(out)
}

/// Rewrites one central directory header; returns how many bytes it used.
fn rewrite_entry(cd: &[u8], disk_starts: &[u64], out: &mut Vec<u8>) -> Result<usize> {
    let header = cd
        .get(..CENTRAL_HEADER_LEN)
        .ok_or_else(|| corrupt("central directory ends early"))?;
    if u32_at(header, 0)? != CENTRAL_HEADER_SIG {
        return Err(corrupt("bad central directory header"));
    }
    let compressed = u32_at(header, 20)?;
    let uncompressed = u32_at(header, 24)?;
    let name_len = usize::from(u16_at(header, 28)?);
    let extra_len = usize::from(u16_at(header, 30)?);
    let comment_len = usize::from(u16_at(header, 32)?);
    let disk = u16_at(header, 34)?;
    let offset = u32_at(header, 42)?;

    let total = CENTRAL_HEADER_LEN + name_len + extra_len + comment_len;
    let variable = cd
        .get(CENTRAL_HEADER_LEN..total)
        .ok_or_else(|| corrupt("central directory entry ends early"))?;
    let (name, rest) = variable.split_at(name_len);
    let (extra, comment) = rest.split_at(extra_len);

    // ZIP64 extra: values present only for header fields that are maxed out,
    // in this order (APPNOTE 4.5.3).
    let (zip64, other_extras) = split_zip64_extra(extra);
    let mut z = Zip64Reader(zip64.unwrap_or_default());
    let uncompressed64 = if uncompressed == MAX32 {
        Some(z.u64()?)
    } else {
        None
    };
    let compressed64 = if compressed == MAX32 {
        Some(z.u64()?)
    } else {
        None
    };
    let offset64 = if offset == MAX32 {
        z.u64()?
    } else {
        offset.into()
    };
    let disk32 = if disk == MAX16 { z.u32()? } else { disk.into() };

    let absolute = disk_start(disk_starts, disk32)?
        .checked_add(offset64)
        .ok_or_else(|| corrupt("offset overflow"))?;

    let mut new_zip64 = Vec::new();
    new_zip64.extend(uncompressed64.iter().flat_map(|v| v.to_le_bytes()));
    new_zip64.extend(compressed64.iter().flat_map(|v| v.to_le_bytes()));
    let offset_field = match u32::try_from(absolute) {
        Ok(fits) if fits != MAX32 => fits,
        _ => {
            new_zip64.extend(absolute.to_le_bytes());
            MAX32
        }
    };

    let mut new_extra = Vec::with_capacity(extra.len() + 12);
    if !new_zip64.is_empty() {
        new_extra.extend(ZIP64_EXTRA_ID.to_le_bytes());
        new_extra.extend((new_zip64.len() as u16).to_le_bytes());
        new_extra.extend(&new_zip64);
    }
    new_extra.extend(other_extras);
    let new_extra_len =
        u16::try_from(new_extra.len()).map_err(|_| corrupt("extra field too long"))?;

    out.extend(&header[..30]);
    out.extend(new_extra_len.to_le_bytes());
    out.extend(&header[32..34]); // comment length
    out.extend(0u16.to_le_bytes()); // disk number start: everything is disk 0 now
    out.extend(&header[36..42]); // internal + external attributes
    out.extend(offset_field.to_le_bytes());
    out.extend(name);
    out.extend(&new_extra);
    out.extend(comment);
    Ok(total)
}

/// Splits extra fields into the ZIP64 field's data and all other fields
/// (kept byte-for-byte). Malformed trailing bytes are kept verbatim.
fn split_zip64_extra(extra: &[u8]) -> (Option<&[u8]>, Vec<u8>) {
    let mut zip64 = None;
    let mut others = Vec::with_capacity(extra.len());
    let mut pos = 0;
    while let (Ok(id), Ok(len)) = (u16_at(extra, pos), u16_at(extra, pos + 2)) {
        let end = pos + 4 + usize::from(len);
        let Some(data) = extra.get(pos + 4..end) else {
            break;
        };
        if id == ZIP64_EXTRA_ID {
            zip64 = Some(data);
        } else {
            others.extend(&extra[pos..end]);
        }
        pos = end;
    }
    others.extend(extra.get(pos..).unwrap_or_default());
    (zip64, others)
}

struct Zip64Reader<'a>(&'a [u8]);

impl Zip64Reader<'_> {
    fn u64(&mut self) -> Result<u64> {
        let value = u64_at(self.0, 0).map_err(|_| corrupt("ZIP64 extra field too short"))?;
        self.0 = &self.0[8..];
        Ok(value)
    }
    fn u32(&mut self) -> Result<u32> {
        let value = u32_at(self.0, 0).map_err(|_| corrupt("ZIP64 extra field too short"))?;
        self.0 = &self.0[4..];
        Ok(value)
    }
}

/// Single-disk end records; ZIP64 versions only when a value doesn't fit.
fn write_end_records(
    out: &mut Vec<u8>,
    entries: u64,
    cd_size: u64,
    cd_offset: u64,
    comment: &[u8],
) -> Result<()> {
    let comment_len = u16::try_from(comment.len()).map_err(|_| corrupt("comment too long"))?;
    let entries16 = u16::try_from(entries).ok().filter(|&n| n != MAX16);
    let size32 = u32::try_from(cd_size).ok().filter(|&n| n != MAX32);
    let offset32 = u32::try_from(cd_offset).ok().filter(|&n| n != MAX32);

    if entries16.is_none() || size32.is_none() || offset32.is_none() {
        let record_pos = cd_offset + cd_size;
        out.extend(ZIP64_EOCD_SIG.to_le_bytes());
        out.extend(((ZIP64_EOCD_LEN - 12) as u64).to_le_bytes()); // size after this field
        out.extend(45u16.to_le_bytes()); // version made by: 4.5 (ZIP64)
        out.extend(45u16.to_le_bytes()); // version needed
        out.extend(0u32.to_le_bytes()); // this disk
        out.extend(0u32.to_le_bytes()); // disk with central directory
        out.extend(entries.to_le_bytes()); // entries on this disk
        out.extend(entries.to_le_bytes()); // entries total
        out.extend(cd_size.to_le_bytes());
        out.extend(cd_offset.to_le_bytes());

        out.extend(ZIP64_LOCATOR_SIG.to_le_bytes());
        out.extend(0u32.to_le_bytes()); // disk with ZIP64 end record
        out.extend(record_pos.to_le_bytes());
        out.extend(1u32.to_le_bytes()); // total disks
    }

    out.extend(EOCD_SIG.to_le_bytes());
    out.extend(0u16.to_le_bytes()); // this disk
    out.extend(0u16.to_le_bytes()); // disk with central directory
    let entries_field = entries16.unwrap_or(MAX16).to_le_bytes();
    out.extend(entries_field); // entries on this disk
    out.extend(entries_field); // entries total
    out.extend(size32.unwrap_or(MAX32).to_le_bytes());
    out.extend(offset32.unwrap_or(MAX32).to_le_bytes());
    out.extend(comment_len.to_le_bytes());
    out.extend(comment);
    Ok(())
}

// ---------------------------------------------------------------------------
// The spliced stream
// ---------------------------------------------------------------------------

/// `head[..head_len]` followed by `tail`, as one `Read + Seek` stream.
pub(crate) struct Spliced<R> {
    head: R,
    head_len: u64,
    tail: Vec<u8>,
    pos: u64,
}

impl<R: Read + Seek> Spliced<R> {
    fn new(head: R, head_len: u64, tail: Vec<u8>) -> Self {
        Self {
            head,
            head_len,
            tail,
            pos: 0,
        }
    }

    fn len(&self) -> u64 {
        self.head_len + self.tail.len() as u64
    }
}

impl<R: Read + Seek> Read for Spliced<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = if self.pos < self.head_len {
            let want = (self.head_len - self.pos).min(buf.len() as u64) as usize;
            self.head.seek(SeekFrom::Start(self.pos))?;
            self.head.read(&mut buf[..want])?
        } else {
            let from = usize::try_from(self.pos - self.head_len).unwrap_or(usize::MAX);
            let available = self.tail.get(from..).unwrap_or_default();
            let n = available.len().min(buf.len());
            buf[..n].copy_from_slice(&available[..n]);
            n
        };
        self.pos += n as u64;
        Ok(n)
    }
}

impl<R: Read + Seek> Seek for Spliced<R> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let target = match from {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::End(delta) => self.len().checked_add_signed(delta),
            SeekFrom::Current(delta) => self.pos.checked_add_signed(delta),
        };
        self.pos = target.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "seek before start of stream")
        })?;
        Ok(self.pos)
    }
}

// ---------------------------------------------------------------------------
// Little helpers
// ---------------------------------------------------------------------------

fn disk_start(disk_starts: &[u64], disk: u32) -> Result<u64> {
    usize::try_from(disk)
        .ok()
        .and_then(|d| disk_starts.get(d).copied())
        .ok_or_else(|| corrupt("refers to a part that doesn't exist"))
}

fn corrupt(what: &str) -> Error {
    Error::Corrupt(format!("spanned ZIP: {what}"))
}

fn u16_at(b: &[u8], at: usize) -> Result<u16> {
    b.get(at..at + 2)
        .and_then(|s| s.try_into().ok())
        .map(u16::from_le_bytes)
        .ok_or_else(|| corrupt("truncated record"))
}

fn u32_at(b: &[u8], at: usize) -> Result<u32> {
    b.get(at..at + 4)
        .and_then(|s| s.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or_else(|| corrupt("truncated record"))
}

fn u64_at(b: &[u8], at: usize) -> Result<u64> {
    b.get(at..at + 8)
        .and_then(|s| s.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or_else(|| corrupt("truncated record"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A central directory header with the given fields; name "f".
    fn entry(disk: u16, offset: u32, compressed: u32, uncompressed: u32, extra: &[u8]) -> Vec<u8> {
        let mut e = Vec::new();
        e.extend(CENTRAL_HEADER_SIG.to_le_bytes());
        e.extend([20, 0, 20, 0, 0, 0, 8, 0, 0, 0, 0, 0]); // versions, flags, method, time, date
        e.extend(0xDEAD_BEEFu32.to_le_bytes()); // crc
        e.extend(compressed.to_le_bytes());
        e.extend(uncompressed.to_le_bytes());
        e.extend(1u16.to_le_bytes()); // name length
        e.extend((extra.len() as u16).to_le_bytes());
        e.extend(0u16.to_le_bytes()); // comment length
        e.extend(disk.to_le_bytes());
        e.extend([0, 0, 0, 0, 0, 0]); // attributes
        e.extend(offset.to_le_bytes());
        e.push(b'f');
        e.extend(extra);
        e
    }

    fn zip64_extra(values: &[u64], disk: Option<u32>) -> Vec<u8> {
        let mut data: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        data.extend(disk.iter().flat_map(|d| d.to_le_bytes()));
        let mut e = ZIP64_EXTRA_ID.to_le_bytes().to_vec();
        e.extend((data.len() as u16).to_le_bytes());
        e.extend(data);
        e
    }

    fn rewritten(e: &[u8], starts: &[u64]) -> Vec<u8> {
        let mut out = Vec::new();
        assert_eq!(rewrite_entry(e, starts, &mut out).unwrap(), e.len());
        out
    }

    fn field_u16(e: &[u8], at: usize) -> u16 {
        u16_at(e, at).unwrap()
    }

    fn field_u32(e: &[u8], at: usize) -> u32 {
        u32_at(e, at).unwrap()
    }

    #[test]
    fn relative_offset_becomes_absolute_on_disk_zero() {
        let out = rewritten(&entry(2, 10, 5, 5, &[]), &[0, 100, 250]);
        assert_eq!(field_u16(&out, 34), 0, "disk");
        assert_eq!(field_u32(&out, 42), 260, "offset");
        assert_eq!(field_u16(&out, 30), 0, "no extra added");
        assert_eq!(out.len(), CENTRAL_HEADER_LEN + 1);
    }

    #[test]
    fn offset_past_4gb_moves_into_new_zip64_field() {
        let out = rewritten(&entry(1, 7, 5, 5, &[]), &[0, 5_000_000_000]);
        assert_eq!(field_u32(&out, 42), MAX32);
        let extra = &out[CENTRAL_HEADER_LEN + 1..];
        assert_eq!(extra, zip64_extra(&[5_000_000_007], None));
        assert_eq!(usize::from(field_u16(&out, 30)), extra.len());
    }

    #[test]
    fn existing_zip64_fields_are_read_in_order_and_disk_dropped() {
        // Sizes, offset and disk all in the ZIP64 field.
        let extra = zip64_extra(&[6_000_000_000, 5_900_000_000, 42], Some(3));
        let e = entry(MAX16, MAX32, MAX32, MAX32, &extra);
        let out = rewritten(&e, &[0, 1, 2, 1000]);

        assert_eq!(field_u16(&out, 34), 0, "disk");
        assert_eq!(
            field_u32(&out, 42),
            1042,
            "offset fits again: back in the header"
        );
        let new_extra = &out[CENTRAL_HEADER_LEN + 1..];
        assert_eq!(
            new_extra,
            zip64_extra(&[6_000_000_000, 5_900_000_000], None)
        );
    }

    #[test]
    fn other_extra_fields_are_kept() {
        let ut = [0x55, 0x54, 5, 0, 1, 2, 3, 4, 5]; // "UT" timestamp field
        let mut extra = zip64_extra(&[9], None);
        extra.extend(ut);
        let out = rewritten(&entry(0, MAX32, 5, 5, &extra), &[0]);
        assert_eq!(field_u32(&out, 42), 9);
        assert_eq!(&out[CENTRAL_HEADER_LEN + 1..], ut);
    }

    #[test]
    fn missing_disk_and_truncation_are_errors() {
        let mut out = Vec::new();
        assert!(rewrite_entry(&entry(5, 0, 1, 1, &[]), &[0, 1], &mut out).is_err());
        let e = entry(0, 0, 1, 1, &[]);
        assert!(rewrite_entry(&e[..40], &[0], &mut out).is_err());
        // Header says ZIP64 offset, but the extra field is missing.
        assert!(rewrite_entry(&entry(0, MAX32, 1, 1, &[]), &[0], &mut out).is_err());
    }

    #[test]
    fn end_records_round_trip_through_the_reader() {
        // 70 000 entries doesn't fit the classic 16-bit field: forces ZIP64.
        for (entries, cd_size, cd_offset) in [(3u64, 200u64, 1000u64), (70_000, 200, 1000)] {
            let mut stream = vec![0u8; (cd_offset + cd_size) as usize];
            write_end_records(&mut stream, entries, cd_size, cd_offset, b"hi").unwrap();

            let end = read_end_records(&mut Cursor::new(stream), &[0]).unwrap();
            assert_eq!(
                end,
                EndRecords {
                    this_disk: 0,
                    cd_disk: 0,
                    entries,
                    cd_size,
                    cd_offset,
                    comment: b"hi".to_vec(),
                }
            );
        }
    }

    #[test]
    fn central_directory_past_4gb_uses_zip64_end_records() {
        let mut out = Vec::new();
        write_end_records(&mut out, 2, 300, 6_000_000_000, b"").unwrap();

        assert_eq!(u32_at(&out, 0).unwrap(), ZIP64_EOCD_SIG);
        assert_eq!(u64_at(&out, 48).unwrap(), 6_000_000_000, "ZIP64 cd offset");
        let locator = &out[ZIP64_EOCD_LEN..];
        assert_eq!(u32_at(locator, 0).unwrap(), ZIP64_LOCATOR_SIG);
        assert_eq!(
            u64_at(locator, 8).unwrap(),
            6_000_000_300,
            "record position"
        );
        let eocd = &out[ZIP64_EOCD_LEN + ZIP64_LOCATOR_LEN..];
        assert_eq!(
            u32_at(eocd, 16).unwrap(),
            MAX32,
            "classic offset field maxed"
        );
        assert_eq!(u16_at(eocd, 10).unwrap(), 2, "entry count still fits");
    }

    #[test]
    fn spliced_reads_and_seeks_across_the_seam() {
        let mut s = Spliced::new(Cursor::new(b"abcdefXXXX".to_vec()), 6, b"ghi".to_vec());
        let mut all = Vec::new();
        s.read_to_end(&mut all).unwrap();
        assert_eq!(all, b"abcdefghi"); // XXXX after head_len is cut off
        s.seek(SeekFrom::Start(4)).unwrap();
        let mut buf = [0u8; 4];
        s.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"efgh");
        assert_eq!(s.seek(SeekFrom::End(-1)).unwrap(), 8);
        assert!(s.seek(SeekFrom::Current(-9)).is_err());
    }
}
