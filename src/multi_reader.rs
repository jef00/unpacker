//! Presents several files as one continuous `Read + Seek` stream.
//!
//! Used for `.001/.002/…` splits and spanned ZIPs, where the parts are just a
//! big file cut into pieces. (RAR volumes are *not* like that — each one has
//! its own headers — so the RAR backend lets libunrar walk them itself.)

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

pub struct MultiReader<R> {
    parts: Vec<R>,
    /// Offset in the combined stream where each part begins.
    starts: Vec<u64>,
    total_len: u64,
    pos: u64,
}

impl MultiReader<File> {
    pub fn open(paths: &[impl AsRef<Path>]) -> io::Result<Self> {
        let files = paths
            .iter()
            .map(File::open)
            .collect::<io::Result<Vec<_>>>()?;
        Self::new(files)
    }
}

impl<R: Read + Seek> MultiReader<R> {
    pub fn new(mut parts: Vec<R>) -> io::Result<Self> {
        let mut starts = Vec::with_capacity(parts.len());
        let mut total_len = 0u64;
        for part in &mut parts {
            starts.push(total_len);
            total_len += part.seek(SeekFrom::End(0))?;
        }
        Ok(Self {
            parts,
            starts,
            total_len,
            pos: 0,
        })
    }

    pub fn len(&self) -> u64 {
        self.total_len
    }

    pub fn is_empty(&self) -> bool {
        self.total_len == 0
    }

    /// Offset in the combined stream where each part begins.
    pub fn part_starts(&self) -> &[u64] {
        &self.starts
    }

    /// Index of the part holding byte `pos`, or `None` at/after the end.
    fn part_at(&self, pos: u64) -> Option<usize> {
        if pos >= self.total_len {
            return None;
        }
        // Last part starting at or before `pos`. Empty parts are skipped
        // naturally because the next part shares their start offset.
        Some(self.starts.partition_point(|&start| start <= pos) - 1)
    }
}

impl<R: Read + Seek> Read for MultiReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let Some(i) = self.part_at(self.pos) else {
            return Ok(0);
        };
        let part = &mut self.parts[i];
        part.seek(SeekFrom::Start(self.pos - self.starts[i]))?;
        // A short read at a part boundary is fine: the `Read` contract allows
        // it, and the next call continues in the following part.
        let n = part.read(buf)?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl<R: Read + Seek> Seek for MultiReader<R> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let target = match from {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::End(delta) => self.total_len.checked_add_signed(delta),
            SeekFrom::Current(delta) => self.pos.checked_add_signed(delta),
        };
        let target = target.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "seek before start of stream")
        })?;
        self.pos = target;
        Ok(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn reader(parts: &[&[u8]]) -> MultiReader<Cursor<Vec<u8>>> {
        MultiReader::new(parts.iter().map(|p| Cursor::new(p.to_vec())).collect()).unwrap()
    }

    fn read_n(r: &mut impl Read, n: usize) -> Vec<u8> {
        let mut buf = vec![0; n];
        r.read_exact(&mut buf).unwrap();
        buf
    }

    #[test]
    fn reads_across_parts_including_empty_ones() {
        let mut r = reader(&[b"abc", b"", b"de", b"", b"fgh"]);
        assert_eq!(r.len(), 8);
        let mut all = Vec::new();
        r.read_to_end(&mut all).unwrap();
        assert_eq!(all, b"abcdefgh");
    }

    #[test]
    fn seeks_into_the_middle_and_across_a_boundary() {
        let mut r = reader(&[b"abc", b"de", b"fgh"]);
        r.seek(SeekFrom::Start(2)).unwrap();
        assert_eq!(read_n(&mut r, 4), b"cdef");
        r.seek(SeekFrom::End(-1)).unwrap();
        assert_eq!(read_n(&mut r, 1), b"h");
        r.seek(SeekFrom::Current(-3)).unwrap();
        assert_eq!(read_n(&mut r, 2), b"fg");
    }

    #[test]
    fn eof_and_past_end() {
        let mut r = reader(&[b"ab"]);
        r.seek(SeekFrom::Start(10)).unwrap();
        let mut buf = [0u8; 4];
        assert_eq!(r.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn seeking_before_start_is_an_error() {
        let mut r = reader(&[b"ab"]);
        assert!(r.seek(SeekFrom::Current(-1)).is_err());
        assert!(r.seek(SeekFrom::End(-3)).is_err());
    }

    #[test]
    fn no_parts_is_empty() {
        let mut r = reader(&[]);
        assert!(r.is_empty());
        let mut all = Vec::new();
        r.read_to_end(&mut all).unwrap();
        assert!(all.is_empty());
    }
}
