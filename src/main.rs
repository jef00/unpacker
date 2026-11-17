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

/// When launched by drag-and-drop, Windows closes the console the moment we
/// exit, so the user never sees the result. Wait for Enter first.
///
/// TODO: only pause when we own the console (GetConsoleProcessList == 1),
/// so running from an existing terminal doesn't pause.
fn pause_before_exit() {
    if cfg!(windows) {
        print!("\nPress Enter to close…");
        let _ = io::stdout().flush();
        let _ = io::stdin().read_line(&mut String::new());
    }
}
