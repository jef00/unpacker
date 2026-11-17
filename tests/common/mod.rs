//! Helpers shared by the backend integration tests.

#![allow(clippy::unwrap_used, clippy::panic, dead_code)]

use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;
use unpacker::backend::{self, Options, Progress};
use unpacker::discovery;

#[derive(Default)]
pub struct Recorder {
    pub started: Vec<String>,
    pub warnings: Vec<String>,
}

impl Progress for Recorder {
    fn entry_started(&mut self, name: &str, _size: u64) {
        self.started.push(name.into());
    }
    fn bytes_written(&mut self, _bytes: u64) {}
    fn warning(&mut self, message: &str) {
        self.warnings.push(message.into());
    }
}

pub fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, bytes).unwrap();
    path
}

/// Cuts `bytes` into `n` roughly equal `.001`, `.002`, … files.
pub fn write_split(dir: &Path, base: &str, bytes: &[u8], n: usize) -> Vec<PathBuf> {
    bytes
        .chunks(bytes.len().div_ceil(n))
        .enumerate()
        .map(|(i, chunk)| write(dir, &format!("{base}.{:03}", i + 1), chunk))
        .collect()
}

pub fn extract(
    dropped: &Path,
    dest: &Path,
    password: Option<&str>,
) -> (unpacker::Result<()>, Recorder) {
    let mut rec = Recorder::default();
    let result = discovery::discover_on_disk(dropped)
        .and_then(|set| backend::open(&set, password))
        .and_then(|mut ex| ex.extract(dest, &Options::default(), &mut rec));
    (result, rec)
}

pub fn read(path: impl AsRef<Path>) -> String {
    fs::read_to_string(path).unwrap()
}

/// Every file under `dir`, as relative `/`-separated paths, sorted.
pub fn tree(dir: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                let rel = path.strip_prefix(root).unwrap();
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let mut out = Vec::new();
    if dir.exists() {
        walk(dir, dir, &mut out);
    }
    out.sort();
    out
}

pub fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Copies fixtures into a fresh temp dir, so discovery only sees that set.
pub fn copy_fixtures(names: &[&str]) -> TempDir {
    let tmp = TempDir::new().unwrap();
    for name in names {
        let file = Path::new(name).file_name().unwrap();
        fs::copy(fixture(name), tmp.path().join(file)).unwrap();
    }
    tmp
}

pub fn crc32(bytes: &[u8]) -> u32 {
    // Small table-less CRC-32 (IEEE), good enough for test assertions.
    let mut crc = !0u32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}
