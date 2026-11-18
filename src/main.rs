//! Command-line / drag-and-drop front-end.
//!
//! Usage:
//! - drag one or more archive parts onto `unpacker.exe`, or run
//!   `unpacker <file>...`;
//! - or just double-click it: it unpacks every archive in its own folder
//!   (or, if there are none, in the current folder).
//!
//! Afterwards it offers to delete the archive files (permanently; Enter
//! means yes). For scripts, `--delete` deletes them without asking and
//! `--keep` keeps them. `--help` lists the options; unknown ones are errors.

use std::collections::HashSet;
use std::ffi::OsString;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use indicatif::{ProgressBar, ProgressStyle};
use unpacker::backend::{self, Layout, Options, Progress};
use unpacker::origin::Origin;
use unpacker::{Error, Result, VolumeSet, discovery};

/// What to do with the archive files after a fully successful unpack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cleanup {
    /// Ask on the console (only when someone is there to answer).
    Ask,
    /// Delete them permanently, without asking.
    Delete,
    /// Leave them.
    Keep,
}

#[derive(Debug, PartialEq, Eq)]
struct Args {
    files: Vec<PathBuf>,
    cleanup: Cleanup,
}

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Unpack(Args),
    Help,
    Version,
}

const USAGE: &str = "\
Usage: unpacker [OPTIONS] [FILE]...

Unpacks RAR, 7z, ZIP and tar archives (also .tar.gz, .tar.bz2, .tar.xz and
.tar.zst), including all multi-part styles. Give any
part of an archive; the other parts are found automatically. With no FILE,
unpacks every archive in the program's folder (or the current folder).

Options:
  --keep      Keep the archive files afterwards (don't ask)
  --delete    Delete the archive files permanently afterwards (don't ask)
  --help      Show this help
  --version   Show the version
  --          Treat everything after this as a file name

Without --keep or --delete, it asks whether to delete the archive files
permanently (Enter means yes), but only if every file came out fine and
someone can answer.";

/// Parses the arguments. Anything that looks like an option but isn't one
/// is an error: silently treating a typo like `--delet` as a file name
/// would make the user believe they'd asked for something they didn't get.
fn parse_args(args: impl IntoIterator<Item = OsString>) -> std::result::Result<Command, String> {
    let mut files = Vec::new();
    let (mut keep, mut delete) = (false, false);
    let mut options_done = false;
    for arg in args {
        if options_done {
            files.push(PathBuf::from(arg));
            continue;
        }
        match arg.to_str() {
            Some("--") => options_done = true,
            Some("--delete") => delete = true,
            Some("--keep") => keep = true,
            Some("--help" | "-h" | "/?") => return Ok(Command::Help),
            Some("--version" | "-V") => return Ok(Command::Version),
            Some(option) if option.starts_with('-') && option.len() > 1 => {
                return Err(format!("unknown option {option}"));
            }
            _ => files.push(PathBuf::from(arg)),
        }
    }
    let cleanup = match (keep, delete) {
        (true, true) => return Err("use either --keep or --delete, not both".into()),
        (true, false) => Cleanup::Keep,
        (false, true) => Cleanup::Delete,
        (false, false) => Cleanup::Ask,
    };
    Ok(Command::Unpack(Args { files, cleanup }))
}

fn main() -> ExitCode {
    let args = match parse_args(std::env::args_os().skip(1)) {
        Ok(Command::Unpack(args)) => args,
        Ok(Command::Help) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Ok(Command::Version) => {
            println!("unpacker {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Err(problem) => {
            eprintln!("✗ {problem}\n\n{USAGE}");
            pause_before_exit();
            return ExitCode::from(2);
        }
    };

    let files = if args.files.is_empty() {
        archives_nearby()
    } else {
        restore_volume_names(&args.files)
    };
    let code = if files.is_empty() {
        println!("No archives found. Put this program next to the archive files and");
        println!("double-click it, or drag archive files onto it.");
        ExitCode::SUCCESS
    } else {
        unpack_all(&files, args.cleanup)
    };

    pause_before_exit();
    code
}

/// No files given: the archives in the first of these folders that has any.
/// See [`search_order`] for which folder comes first.
fn archives_nearby() -> Vec<PathBuf> {
    let own_folder = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    let current = std::env::current_dir().ok();
    #[cfg(unix)]
    let home = std::env::var_os("HOME").map(PathBuf::from);
    #[cfg(not(unix))]
    let home: Option<PathBuf> = None;
    for dir in search_order(current, own_folder, home) {
        if archives_in(&dir).is_empty() {
            continue;
        }
        // Repair first, then list: `Title.rar`, `Title (1).rar` and
        // `Title (2).rar` are one archive, not three.
        report(&unpacker::repair::fix_volume_names(&dir));
        let found = archives_in(&dir);
        println!("Found {} archive(s) in {}", found.len(), dir.display());
        return found;
    }
    Vec::new()
}

/// Dropped files: restores lost part numbers in their folders (see
/// `unpacker::repair`) and returns the paths, renamed where needed.
fn restore_volume_names(files: &[PathBuf]) -> Vec<PathBuf> {
    let folder = |f: &PathBuf| match f.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let mut renamed: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut done: HashSet<PathBuf> = HashSet::new();
    for dir in files.iter().map(folder) {
        if done.insert(dir.clone()) {
            let outcome = unpacker::repair::fix_volume_names(&dir);
            report(&outcome);
            renamed.extend(outcome.renamed);
        }
    }
    // Same folder + same file name: compare by those, since `x.rar` and
    // `./x.rar` are the same file.
    files
        .iter()
        .map(|f| {
            renamed
                .iter()
                .find(|(old, _)| {
                    old.file_name() == f.file_name() && old.parent() == Some(folder(f).as_path())
                })
                .map_or_else(
                    || f.clone(),
                    |(_, new)| f.with_file_name(new.file_name().unwrap_or_default()),
                )
        })
        .collect()
}

fn report(outcome: &unpacker::repair::Outcome) {
    for line in &outcome.notes {
        println!("{line}");
    }
}

/// Where to look for archives when no files are given:
///
/// - From a terminal, the folder you're in comes first: that's what
///   `unpacker` with no arguments means there.
/// - Double-clicked on Windows, the current folder *is* the program's
///   folder, so this changes nothing.
/// - Double-clicked on a Mac, Terminal opens in the home folder, which says
///   nothing about where the archives are: then the program's folder first.
fn search_order(
    current: Option<PathBuf>,
    own_folder: Option<PathBuf>,
    home: Option<PathBuf>,
) -> Vec<PathBuf> {
    let current_is_home = current.is_some() && current == home;
    let (first, second) = if current_is_home {
        (own_folder, current)
    } else {
        (current, own_folder)
    };
    let mut order: Vec<PathBuf> = first.into_iter().collect();
    order.extend(second.filter(|dir| !order.contains(dir)));
    order
}

fn archives_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file())
        .collect();
    discovery::archives_among_on_disk(&files)
}

/// Unpacks every distinct archive among `files`, then deals with the
/// archive files. One bad archive doesn't stop the others.
fn unpack_all(files: &[PathBuf], cleanup: Cleanup) -> ExitCode {
    let mut seen = HashSet::new();
    let mut failures = 0;
    // Archives that unpacked completely, with nothing skipped: only these
    // are offered for deletion.
    let mut clean: Vec<VolumeSet> = Vec::new();

    for path in files {
        if !path.is_file() {
            eprintln!("✗ {}: file not found.", path.display());
            failures += 1;
            continue;
        }
        let set = match discovery::discover_on_disk(path) {
            Ok(set) => set,
            Err(e) => {
                eprintln!("✗ {}: {e}", file_label(path));
                failures += 1;
                continue;
            }
        };
        // Dropping all 12 parts of one archive should unpack it once.
        if !seen.insert(set.first().to_path_buf()) {
            continue;
        }
        match unpack(&set) {
            Ok(Outcome { warnings: 0, .. }) => clean.push(set),
            Ok(_) => {
                println!("  Some files were skipped, so the archive files are kept.");
            }
            Err(e) => {
                eprintln!("✗ {}: {e}", set.output_name());
                failures += 1;
            }
        }
    }

    remove_archives(&clean, cleanup);

    if failures == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Deals with the archive files of `sets`: only called with archives that
/// unpacked completely. Deleting is permanent (no Recycle Bin / Trash): the
/// extracted files are the point, and multi-GB parts often don't fit in the
/// Recycle Bin or live on drives without one anyway.
fn remove_archives(sets: &[VolumeSet], cleanup: Cleanup) {
    let parts: Vec<&Path> = sets
        .iter()
        .flat_map(|s| s.parts.iter().map(PathBuf::as_path))
        .collect();
    if parts.is_empty() {
        return;
    }
    let count = parts.len();
    let what = if count == 1 {
        "the archive file".to_string()
    } else {
        format!("the {count} archive files")
    };

    let delete = match cleanup {
        Cleanup::Delete => true,
        Cleanup::Keep => false,
        // No console to answer on (a script, a pipe): keep them, the safe side.
        Cleanup::Ask if !io::stdin().is_terminal() => false,
        Cleanup::Ask => {
            print!("\nDelete {what} permanently? [Y/n] ");
            let _ = io::stdout().flush();
            let mut answer = String::new();
            match io::stdin().read_line(&mut answer) {
                Ok(0) | Err(_) => false,
                Ok(_) => is_yes(&answer),
            }
        }
    };
    if delete {
        delete_permanently(&parts, &what);
    }
}

/// Removes every part; one that can't be removed doesn't stop the rest,
/// and is reported by name.
fn delete_permanently(parts: &[&Path], what: &str) {
    let mut failed = 0;
    for part in parts {
        if let Err(e) = std::fs::remove_file(part) {
            eprintln!("✗ Couldn't delete {}: {e}", file_label(part));
            failed += 1;
        }
    }
    if failed == 0 {
        println!("🗑 Deleted {what}.");
    }
}

/// Enter alone means yes; so do "y", "yes", "j" and "ja".
fn is_yes(answer: &str) -> bool {
    matches!(
        answer.trim().to_lowercase().as_str(),
        "" | "y" | "yes" | "j" | "ja"
    )
}

const MAX_PASSWORD_ATTEMPTS: u32 = 3;

struct Outcome {
    /// Where the result went: the single file, or the folder.
    result: PathBuf,
    /// Files written (over all password attempts).
    written: usize,
    /// Files skipped (already there, unsafe name, …).
    warnings: usize,
}

/// The line that sums up one archive. Never claims success for files that
/// were skipped: "✓ Extracted to movie.mkv" when `movie.mkv` was already
/// there (and is someone else's) would be a lie.
fn summary(shown: &Path, written: usize, skipped: usize) -> String {
    let shown = shown.display();
    match (written, skipped) {
        (0, 0) => "✓ Nothing to extract: the archive is empty.".to_string(),
        (0, n) => format!("• Nothing extracted: {} skipped (see above).", files(n)),
        (_, 0) => format!("✓ Extracted to {shown}"),
        (_, n) => format!("✓ Extracted to {shown} ({} skipped, see above)", files(n)),
    }
}

/// "1 file", "3 files".
fn files(n: usize) -> String {
    if n == 1 {
        "1 file".into()
    } else {
        format!("{n} files")
    }
}

fn unpack(set: &VolumeSet) -> Result<Outcome> {
    let parts = set.parts.len();
    println!(
        "→ {} ({parts} part{})",
        set.output_name(),
        if parts == 1 { "" } else { "s" }
    );

    // Retrying from scratch is safe: backends never leave a half-written
    // file, and anything finished on an earlier attempt is skipped as
    // "already exists" (see the password contract on `Extractor::extract`).
    // `Tally` makes sure those skips don't count as warnings: they're ours.
    let mut password: Option<String> = None;
    let mut attempts = 0;
    let mut tally = Tally::default();
    let outcome = loop {
        match try_unpack(set, password.as_deref(), &mut tally) {
            Ok(outcome) => break outcome,
            Err(Error::PasswordRequired) if attempts < MAX_PASSWORD_ATTEMPTS => {}
            Err(Error::WrongPassword) if attempts < MAX_PASSWORD_ATTEMPTS => {
                println!("  Wrong password, try again.");
            }
            Err(e) => return Err(e),
        }
        password = Some(ask_password()?);
        attempts += 1;
    };

    // Paths found via read_dir(".") start with "./"; don't show that.
    let shown = outcome.result.strip_prefix(".").unwrap_or(&outcome.result);
    println!("{}", summary(shown, outcome.written, outcome.warnings));
    Ok(outcome)
}

/// Unpacks without pointless nesting, and says where the result went:
/// - one file → straight next to the archive (`movie.rar` → `movie.mkv`)
/// - one folder → `movie/…`, not `movie/Movie/…`
/// - anything else → `movie/…` with the archive's own structure
fn try_unpack(set: &VolumeSet, password: Option<&str>, tally: &mut Tally) -> Result<Outcome> {
    let mut extractor = backend::open(set, password)?;
    let entries = extractor.list()?;
    let beside = set.first().parent().unwrap_or(Path::new("."));
    let folder = beside.join(set.output_name());

    let (dest, strip_prefix, result) = match backend::layout(&entries) {
        Layout::SingleFile { folders, name } => {
            let file = beside.join(name);
            let strip = (!folders.as_os_str().is_empty()).then_some(folders);
            (beside.to_path_buf(), strip, file)
        }
        Layout::SingleFolder(top) => (folder.clone(), Some(top), folder),
        Layout::Nested => (folder.clone(), None, folder),
    };
    let opts = Options {
        strip_prefix,
        origin: Origin::of(&set.parts),
        ..Options::default()
    };
    let total = entries.iter().filter(|e| !e.is_dir).map(|e| e.size).sum();
    let mut progress = ConsoleProgress::new(total, tally);
    extractor.extract(&dest, &opts, &mut progress)?;
    Ok(Outcome {
        result,
        written: progress.tally.written.len(),
        warnings: progress.tally.warnings,
    })
}

/// Reads a password from the console.
///
/// Input is visible on purpose: archive passwords shared on Telegram are
/// usually public (often the channel name), and seeing what you typed or
/// pasted avoids a lot of "wrong password" frustration.
fn ask_password() -> Result<String> {
    print!("  Password: ");
    io::stdout().flush()?;
    let mut line = String::new();
    if io::stdin().read_line(&mut line)? == 0 {
        return Err(Error::PasswordRequired); // stdin closed: nobody to ask
    }
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

/// What happened to one archive, across password attempts.
#[derive(Default)]
struct Tally {
    /// Entries written so far, by any attempt. A retry skips the earlier
    /// ones as "already exists", which is expected and not a warning.
    written: HashSet<String>,
    /// Problems worth keeping the archive for (skipped files).
    warnings: usize,
}

/// File names as they start, plus one progress bar for the whole archive
/// (hidden automatically when the output isn't a terminal).
struct ConsoleProgress<'a> {
    bar: ProgressBar,
    tally: &'a mut Tally,
}

impl<'a> ConsoleProgress<'a> {
    fn new(total: u64, tally: &'a mut Tally) -> Self {
        let bar = ProgressBar::new(total);
        if let Ok(style) = ProgressStyle::with_template(
            "  [{bar:30}] {percent:>3}%  {binary_bytes}/{binary_total_bytes}  \
             {binary_bytes_per_sec}  {eta} left",
        ) {
            // Plain ASCII: renders in every Windows console font.
            bar.set_style(style.progress_chars("=> "));
        }
        bar.enable_steady_tick(Duration::from_millis(250));
        Self { bar, tally }
    }

    fn say(&self, line: String) {
        if self.bar.is_hidden() {
            println!("{line}");
        } else {
            self.bar.println(line);
        }
    }
}

impl Progress for ConsoleProgress<'_> {
    fn entry_started(&mut self, name: &str, size: u64) {
        self.tally.written.insert(name.to_string());
        self.say(format!("  {name} ({:.1} MB)", size as f64 / 1_048_576.0));
    }

    fn bytes_written(&mut self, bytes: u64) {
        self.bar.inc(bytes);
    }

    fn warning(&mut self, message: &str) {
        self.tally.warnings += 1;
        self.bar.suspend(|| eprintln!("  ! {message}"));
    }

    fn skipped_existing(&mut self, name: &str) {
        if !self.tally.written.contains(name) {
            self.warning(&format!("Skipped \"{name}\": file already exists."));
        }
    }
}

impl Drop for ConsoleProgress<'_> {
    fn drop(&mut self) {
        // Done or failed: either way, clear the bar so the result line
        // (✓ or ✗) is what stays on screen.
        self.bar.finish_and_clear();
    }
}

fn file_label(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// When launched by drag-and-drop or double-click, Windows gives us a console
/// window of our own and closes it the moment we exit, so the user would
/// never see the result. Then (and only then) wait for Enter.
///
/// From cmd, PowerShell or a script we exit straight away: the terminal stays
/// open anyway, and a pause would hang batch jobs waiting for input.
fn pause_before_exit() {
    if alone_on_console() {
        print!("\nPress Enter to close…");
        let _ = io::stdout().flush();
        let _ = io::stdin().read_line(&mut String::new());
    }
}

/// Whether Windows created this console just for us. A console started by
/// Explorer has exactly one process attached (us); inside a terminal, the
/// shell is attached too. Same trick 7-Zip and others use.
#[cfg(windows)]
#[allow(unsafe_code)]
fn alone_on_console() -> bool {
    use windows_sys::Win32::System::Console::GetConsoleProcessList;

    // We only need to tell "1" from "more than 1", so room for 2 IDs is
    // enough: if there are more, the call still returns the total count.
    let mut process_ids = [0u32; 2];
    // SAFETY: the pointer and length describe a valid, writable buffer that
    // outlives the call; the API writes at most `len` IDs into it. It returns
    // 0 on failure (e.g. no console at all), which counts as "not alone".
    let count =
        unsafe { GetConsoleProcessList(process_ids.as_mut_ptr(), process_ids.len() as u32) };
    count == 1
}

#[cfg(not(windows))]
fn alone_on_console() -> bool {
    false // Terminals on macOS/Linux don't vanish when a program exits.
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn unpack(args: &[&str]) -> Args {
        match parse_args(os(args)) {
            Ok(Command::Unpack(a)) => a,
            other => panic!("{args:?}: {other:?}"),
        }
    }

    #[test]
    fn flags_and_files_are_told_apart() {
        let args = unpack(&["a.rar", "--delete", "b.zip"]);
        assert_eq!(args.files, [PathBuf::from("a.rar"), PathBuf::from("b.zip")]);
        assert_eq!(args.cleanup, Cleanup::Delete);
        assert_eq!(unpack(&["--keep"]).cleanup, Cleanup::Keep);
        assert_eq!(unpack(&[]).cleanup, Cleanup::Ask);
    }

    #[test]
    fn unknown_options_are_errors_not_files() {
        for typo in ["--delet", "--Keep", "-k", "--rm"] {
            let err = parse_args(os(&["a.rar", typo])).unwrap_err();
            assert!(err.contains(typo), "{err}");
        }
        assert!(parse_args(os(&["--keep", "--delete"])).is_err());
    }

    #[test]
    fn help_version_and_double_dash() {
        assert_eq!(parse_args(os(&["a.rar", "--help"])), Ok(Command::Help));
        assert_eq!(parse_args(os(&["-h"])), Ok(Command::Help));
        assert_eq!(parse_args(os(&["--version"])), Ok(Command::Version));
        // After `--`, even `-weird.rar` is a file; a lone `-` is too.
        let args = unpack(&["--keep", "--", "-weird.rar", "--delete"]);
        assert_eq!(
            args.files,
            [PathBuf::from("-weird.rar"), PathBuf::from("--delete")]
        );
        assert_eq!(args.cleanup, Cleanup::Keep);
        assert_eq!(unpack(&["-"]).files, [PathBuf::from("-")]);
    }

    #[test]
    fn summary_never_claims_skipped_files_were_extracted() {
        let at = Path::new("movie.mkv");
        assert_eq!(summary(at, 1, 0), "✓ Extracted to movie.mkv");
        assert_eq!(
            summary(at, 0, 1),
            "• Nothing extracted: 1 file skipped (see above)."
        );
        assert_eq!(
            summary(Path::new("movie"), 4, 2),
            "✓ Extracted to movie (2 files skipped, see above)"
        );
        assert_eq!(
            summary(at, 0, 0),
            "✓ Nothing to extract: the archive is empty."
        );
    }

    #[test]
    fn enter_means_yes() {
        for yes in ["\n", "y\r\n", "Yes\n", "j\n", "JA\n"] {
            assert!(is_yes(yes), "{yes:?}");
        }
        for no in ["n\n", "nee\n", "no\n", "k\n"] {
            assert!(!is_yes(no), "{no:?}");
        }
    }

    #[test]
    fn current_folder_first_unless_it_is_home() {
        let p = |s: &str| Some(PathBuf::from(s));
        let order = |c, o, h| search_order(c, o, h);
        // Terminal in ~/Downloads, program in /usr/local/bin.
        assert_eq!(
            order(p("/h/dl"), p("/bin"), p("/h")),
            [PathBuf::from("/h/dl"), PathBuf::from("/bin")]
        );
        // Mac double-click: Terminal starts in the home folder.
        assert_eq!(
            order(p("/h"), p("/h/dl"), p("/h")),
            [PathBuf::from("/h/dl"), PathBuf::from("/h")]
        );
        // Windows double-click: both are the program's folder; look once.
        assert_eq!(
            order(p("C:/dl"), p("C:/dl"), None),
            [PathBuf::from("C:/dl")]
        );
        assert_eq!(order(None, p("/bin"), None), [PathBuf::from("/bin")]);
    }

    /// `cargo test` always runs inside a terminal or CI shell, never as a
    /// console of its own, so it must never be treated as drag-and-drop.
    #[test]
    fn test_runner_does_not_count_as_drag_and_drop() {
        assert!(!alone_on_console());
    }
}
