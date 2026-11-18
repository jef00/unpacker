//! Writing extracted entries to disk, safely. The rules live here once:
//!
//! 1. The name goes through `sanitize_entry_path`; unsafe names are skipped
//!    with a warning instead of aborting the whole archive.
//! 2. Existing files are respected according to `Options::overwrite`.
//! 3. Data goes to `<name>.unpacker-tmp` and is renamed into place only once
//!    it's complete and verified, so a crash or a CRC error never leaves a
//!    half-written file that looks finished.
//!
//! Streaming backends (ZIP, 7z) call [`write_entry`]. Backends whose library
//! writes files itself (RAR) use [`plan_entry`] + [`TempPath`] directly.

use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use super::{Options, Overwrite, Progress};
use crate::error::{Error, Result};
use crate::safety::sanitize_entry_path;

const TEMP_SUFFIX: &str = ".unpacker-tmp";
const BUFFER_SIZE: usize = 256 * 1024;

/// What a backend knows about an entry before reading its data.
pub(crate) struct EntryInfo<'a> {
    pub name: &'a str,
    pub size: u64,
    pub is_dir: bool,
}

/// What to do with one entry, decided before touching its data.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Plan {
    /// Unsafe name or existing file: a warning was already reported.
    Skip,
    /// A directory entry; the directory now exists.
    Directory,
    /// Write the data to this (sanitised, in-`dest`) path via a [`TempPath`].
    File(PathBuf),
}

pub(crate) fn plan_entry(
    dest: &Path,
    entry: &EntryInfo<'_>,
    opts: &Options,
    progress: &mut dyn Progress,
) -> Result<Plan> {
    let relative = match sanitize_entry_path(entry.name) {
        Ok(path) => path,
        Err(e @ Error::UnsafePath(_)) => {
            progress.warning(&e.to_string());
            return Ok(Plan::Skip);
        }
        Err(e) => return Err(e),
    };
    let relative = match &opts.strip_prefix {
        Some(prefix) => {
            // A stripped folder's own entry (or a parent's): that's `dest` now.
            if entry.is_dir && prefix.starts_with(&relative) {
                fs::create_dir_all(dest)?;
                return Ok(Plan::Directory);
            }
            match relative.strip_prefix(prefix) {
                Ok(rest) if !rest.as_os_str().is_empty() => rest.to_path_buf(),
                _ => relative,
            }
        }
        None => relative,
    };
    let target = dest.join(relative);

    if entry.is_dir {
        fs::create_dir_all(&target)?;
        return Ok(Plan::Directory);
    }
    if target.exists() && opts.overwrite == Overwrite::Skip {
        progress.skipped_existing(entry.name);
        return Ok(Plan::Skip);
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    Ok(Plan::File(target))
}

/// Plans the entry and, if it should be written, streams `data` into place.
pub(crate) fn write_entry(
    dest: &Path,
    entry: &EntryInfo<'_>,
    opts: &Options,
    progress: &mut dyn Progress,
    data: &mut dyn Read,
) -> Result<()> {
    let Plan::File(target) = plan_entry(dest, entry, opts, progress)? else {
        return Ok(());
    };
    progress.entry_started(entry.name, entry.size);
    let temp = TempPath::for_target(&target);
    let file = File::create(temp.path())?;
    copy_with_progress(data, &file, progress)?;
    // Close before renaming: Windows refuses to rename a file that's open.
    drop(file);
    temp.commit()
}

/// Like `io::copy`, but tells read errors (a damaged archive) apart from
/// write errors (disk full, permissions) and reports progress.
fn copy_with_progress(data: &mut dyn Read, out: &File, progress: &mut dyn Progress) -> Result<()> {
    let mut out = BufWriter::with_capacity(BUFFER_SIZE, out);
    let mut buf = vec![0u8; BUFFER_SIZE];
    loop {
        let n = match data.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(read_error(e)),
        };
        out.write_all(&buf[..n])?;
        progress.bytes_written(n as u64);
    }
    out.flush()?;
    Ok(())
}

/// Errors from the *read* side come from a decompressor. Unless the OS itself
/// reported them (a real disk/network failure), they mean the data is bad.
/// Decoders don't agree on an `ErrorKind` for that: `zip` uses `InvalidData`,
/// `sevenz-rust2` uses `Other` for a checksum mismatch.
pub(crate) fn read_error(e: io::Error) -> Error {
    if e.raw_os_error().is_some() {
        return Error::Io(e);
    }
    match e.kind() {
        io::ErrorKind::UnexpectedEof => {
            Error::Corrupt("the archive ends too early; is a part missing or incomplete?".into())
        }
        _ => Error::Corrupt(e.to_string()),
    }
}

/// How often [`while_counting`] passes the count on as progress.
const WATCH_INTERVAL: Duration = Duration::from_millis(200);

/// Runs `work`, which adds the bytes it writes to `written` (libunrar does,
/// through its data callback), and meanwhile reports that count as
/// progress. For libraries that write files themselves and would otherwise
/// leave the progress bar stuck until a 10 GB file is done.
///
/// Not the output file's size: on Windows UnRAR sets a large file to its
/// full size before writing, so the size says 100% from the start.
///
/// Returns `work`'s result and how many bytes were reported, so the caller
/// can report the rest once the file is complete.
pub(crate) fn while_counting<T>(
    written: &AtomicU64,
    progress: &mut dyn Progress,
    work: impl FnOnce() -> T,
) -> (T, u64) {
    let done = AtomicBool::new(false);
    thread::scope(|scope| {
        let watcher = scope.spawn(|| {
            let mut reported = 0;
            while !done.load(Ordering::Acquire) {
                thread::park_timeout(WATCH_INTERVAL);
                let now = written.load(Ordering::Relaxed);
                if now > reported {
                    progress.bytes_written(now - reported);
                    reported = now;
                }
            }
            reported
        });
        let result = work();
        done.store(true, Ordering::Release);
        watcher.thread().unpark();
        // The watcher can't panic (no unwrap, no indexing); 0 is a safe fallback.
        (result, watcher.join().unwrap_or(0))
    })
}

/// A `<target>.unpacker-tmp` path that is deleted on drop unless
/// [`TempPath::commit`] renames it to the target first.
///
/// Holds no file handle, so a library can write to the path itself, and the
/// rename works on Windows (which can't rename open files).
pub(crate) struct TempPath {
    temp_path: PathBuf,
    final_path: PathBuf,
    committed: bool,
}

impl TempPath {
    pub(crate) fn for_target(final_path: &Path) -> Self {
        let mut temp_name = final_path.file_name().unwrap_or_default().to_os_string();
        temp_name.push(TEMP_SUFFIX);
        Self {
            temp_path: final_path.with_file_name(temp_name),
            final_path: final_path.to_path_buf(),
            committed: false,
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.temp_path
    }

    pub(crate) fn commit(mut self) -> Result<()> {
        // `fs::rename` replaces an existing target on Windows too.
        fs::rename(&self.temp_path, &self.final_path)?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for TempPath {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(&self.temp_path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[derive(Default)]
    struct Recorder {
        started: Vec<String>,
        bytes: u64,
        warnings: Vec<String>,
    }

    impl Progress for Recorder {
        fn entry_started(&mut self, name: &str, _size: u64) {
            self.started.push(name.into());
        }
        fn bytes_written(&mut self, bytes: u64) {
            self.bytes += bytes;
        }
        fn warning(&mut self, message: &str) {
            self.warnings.push(message.into());
        }
    }

    /// A reader that yields some bytes and then fails like a CRC mismatch.
    struct FailsHalfway(bool);

    impl Read for FailsHalfway {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if std::mem::replace(&mut self.0, true) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Invalid checksum",
                ));
            }
            buf[..4].copy_from_slice(b"half");
            Ok(4)
        }
    }

    fn file(name: &str) -> EntryInfo<'_> {
        EntryInfo {
            name,
            size: 0,
            is_dir: false,
        }
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(TEMP_SUFFIX))
            .collect()
    }

    #[test]
    fn writes_file_and_creates_parents() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = Recorder::default();
        write_entry(
            dir.path(),
            &file("a/b/c.txt"),
            &Options::default(),
            &mut p,
            &mut Cursor::new("hello"),
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("a/b/c.txt")).unwrap(),
            "hello"
        );
        assert_eq!(p.bytes, 5);
        assert_eq!(p.started, ["a/b/c.txt"]);
    }

    #[test]
    fn corrupt_data_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = Recorder::default();
        let err = write_entry(
            dir.path(),
            &file("x.mkv"),
            &Options::default(),
            &mut p,
            &mut FailsHalfway(false),
        );
        assert!(matches!(err, Err(Error::Corrupt(_))), "{err:?}");
        assert!(!dir.path().join("x.mkv").exists());
        assert!(leftovers(dir.path()).is_empty());
    }

    #[test]
    fn existing_file_is_kept_by_default() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("x.txt"), "mine").unwrap();
        let mut p = Recorder::default();
        write_entry(
            dir.path(),
            &file("x.txt"),
            &Options::default(),
            &mut p,
            &mut Cursor::new("new"),
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("x.txt")).unwrap(),
            "mine"
        );
        assert_eq!(p.warnings.len(), 1);
    }

    #[test]
    fn existing_file_is_replaced_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("x.txt"), "mine").unwrap();
        let opts = Options {
            overwrite: Overwrite::Replace,
            ..Options::default()
        };
        write_entry(
            dir.path(),
            &file("x.txt"),
            &opts,
            &mut Recorder::default(),
            &mut Cursor::new("new"),
        )
        .unwrap();
        assert_eq!(fs::read_to_string(dir.path().join("x.txt")).unwrap(), "new");
    }

    #[test]
    fn unsafe_name_is_skipped_with_warning() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out");
        let mut p = Recorder::default();
        write_entry(
            &dest,
            &file("../evil.txt"),
            &Options::default(),
            &mut p,
            &mut Cursor::new("x"),
        )
        .unwrap();
        assert!(!dir.path().join("evil.txt").exists());
        assert_eq!(p.warnings.len(), 1);
    }

    #[test]
    fn decoder_errors_are_corrupt_os_errors_are_io() {
        let checksum = io::Error::other("ChecksumVerificationFailed");
        assert!(matches!(read_error(checksum), Error::Corrupt(_)));
        let disk = io::Error::from_raw_os_error(5); // EIO / ERROR_ACCESS_DENIED
        assert!(matches!(read_error(disk), Error::Io(_)));
    }

    #[test]
    fn temp_path_is_removed_unless_committed() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("video.mkv");
        {
            let temp = TempPath::for_target(&target);
            fs::write(temp.path(), "partial").unwrap();
        }
        assert!(leftovers(dir.path()).is_empty());
        assert!(!target.exists());

        let temp = TempPath::for_target(&target);
        fs::write(temp.path(), "done").unwrap();
        temp.commit().unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "done");
        assert!(leftovers(dir.path()).is_empty());
    }

    #[test]
    fn plan_reports_directories_and_skips() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = Recorder::default();
        let opts = Options::default();
        let d = EntryInfo {
            name: "a/b/",
            size: 0,
            is_dir: true,
        };
        assert_eq!(
            plan_entry(dir.path(), &d, &opts, &mut p).unwrap(),
            Plan::Directory
        );
        assert!(dir.path().join("a/b").is_dir());
        assert_eq!(
            plan_entry(dir.path(), &file("../x"), &opts, &mut p).unwrap(),
            Plan::Skip
        );
        assert_eq!(
            plan_entry(dir.path(), &file("a/c.txt"), &opts, &mut p).unwrap(),
            Plan::File(dir.path().join("a").join("c.txt"))
        );
    }

    #[test]
    fn strips_the_top_folder_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out");
        let mut p = Recorder::default();
        let opts = Options {
            strip_prefix: Some(PathBuf::from("Movie")),
            ..Options::default()
        };
        let top = EntryInfo {
            name: "Movie/",
            size: 0,
            is_dir: true,
        };
        assert_eq!(
            plan_entry(&dest, &top, &opts, &mut p).unwrap(),
            Plan::Directory
        );
        assert!(dest.is_dir());
        assert_eq!(
            plan_entry(&dest, &file("Movie\\subs/nl.srt"), &opts, &mut p).unwrap(),
            Plan::File(dest.join("subs").join("nl.srt"))
        );
        // Only whole components match: "MovieX" is not inside "Movie".
        assert_eq!(
            plan_entry(&dest, &file("MovieX/a.txt"), &opts, &mut p).unwrap(),
            Plan::File(dest.join("MovieX").join("a.txt"))
        );
        assert!(p.warnings.is_empty());
    }

    #[test]
    fn strips_several_levels_for_a_single_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = Recorder::default();
        let opts = Options {
            strip_prefix: Some(PathBuf::from("A/B")),
            ..Options::default()
        };
        for parent in ["A/", "A/B/"] {
            let d = EntryInfo {
                name: parent,
                size: 0,
                is_dir: true,
            };
            assert_eq!(
                plan_entry(dir.path(), &d, &opts, &mut p).unwrap(),
                Plan::Directory
            );
        }
        assert!(!dir.path().join("A").exists());
        assert_eq!(
            plan_entry(dir.path(), &file("A/B/a.mkv"), &opts, &mut p).unwrap(),
            Plan::File(dir.path().join("a.mkv"))
        );
    }

    #[test]
    fn counting_reports_progress_while_working() {
        let written = AtomicU64::new(0);
        let mut p = Recorder::default();
        let (result, reported) = while_counting(&written, &mut p, || {
            for _ in 0..3 {
                written.fetch_add(1000, Ordering::Relaxed);
                thread::sleep(WATCH_INTERVAL * 2);
            }
            "done"
        });
        assert_eq!(result, "done");
        assert!(reported > 0 && reported <= 3000, "{reported}");
        assert_eq!(p.bytes, reported);
    }

    #[test]
    fn truncated_data_mentions_missing_part() {
        let err = read_error(io::Error::from(io::ErrorKind::UnexpectedEof));
        assert!(err.to_string().contains("part missing"), "{err}");
    }
}
