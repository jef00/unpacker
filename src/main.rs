//! Command-line / drag-and-drop front-end.
//!
//! Usage: drag one or more archive parts onto `unpacker.exe`, or
//! `unpacker <file>...`. Each archive is extracted into a folder next to it.

use std::collections::HashSet;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use unpacker::backend::{self, Options, Progress};
use unpacker::{Error, Result, VolumeSet, discovery};

fn main() -> ExitCode {
    let dropped: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();

    let code = if dropped.is_empty() {
        println!("Drag archive files onto this program to unpack them.");
        ExitCode::SUCCESS
    } else {
        unpack_all(&dropped)
    };

    pause_before_exit();
    code
}

/// Unpacks every distinct archive among the dropped files.
/// One bad archive doesn't stop the others.
fn unpack_all(dropped: &[PathBuf]) -> ExitCode {
    let mut seen = HashSet::new();
    let mut failures = 0;

    for path in dropped {
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
        if let Err(e) = unpack(&set) {
            eprintln!("✗ {}: {e}", set.output_name());
            failures += 1;
        }
    }

    if failures == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

const MAX_PASSWORD_ATTEMPTS: u32 = 3;

fn unpack(set: &VolumeSet) -> Result<()> {
    let parts = set.parts.len();
    println!(
        "→ {} ({parts} part{})",
        set.output_name(),
        if parts == 1 { "" } else { "s" }
    );

    let dest = set
        .first()
        .parent()
        .unwrap_or(Path::new("."))
        .join(set.output_name());

    // Retrying from scratch is safe: backends never leave a half-written
    // file, and anything finished on an earlier attempt is skipped as
    // "already exists" (see the password contract on `Extractor::extract`).
    let mut password: Option<String> = None;
    let mut attempts = 0;
    loop {
        match try_unpack(set, &dest, password.as_deref()) {
            Ok(()) => break,
            Err(Error::PasswordRequired) if attempts < MAX_PASSWORD_ATTEMPTS => {}
            Err(Error::WrongPassword) if attempts < MAX_PASSWORD_ATTEMPTS => {
                println!("  Wrong password, try again.");
            }
            Err(e) => return Err(e),
        }
        password = Some(ask_password()?);
        attempts += 1;
    }

    // Paths found via read_dir(".") start with "./"; don't show that.
    let shown = dest.strip_prefix(".").unwrap_or(&dest);
    println!("✓ Extracted to {}", shown.display());
    Ok(())
}

fn try_unpack(set: &VolumeSet, dest: &Path, password: Option<&str>) -> Result<()> {
    let mut extractor = backend::open(set, password)?;
    extractor.extract(dest, &Options::default(), &mut ConsoleProgress)
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

struct ConsoleProgress;

impl Progress for ConsoleProgress {
    fn entry_started(&mut self, name: &str, size: u64) {
        println!("  {name} ({:.1} MB)", size as f64 / 1_048_576.0);
    }

    fn bytes_written(&mut self, _bytes: u64) {
        // TODO: progress bar.
    }

    fn warning(&mut self, message: &str) {
        eprintln!("  ! {message}");
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

    /// `cargo test` always runs inside a terminal or CI shell, never as a
    /// console of its own, so it must never be treated as drag-and-drop.
    #[test]
    fn test_runner_does_not_count_as_drag_and_drop() {
        assert!(!alone_on_console());
    }
}
