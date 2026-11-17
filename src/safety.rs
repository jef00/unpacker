//! Turning untrusted entry names from an archive into safe relative paths.
//!
//! Every path a backend writes MUST go through [`sanitize_entry_path`].
//! Archives are attacker-controlled input: an entry called
//! `..\..\AppData\Roaming\Microsoft\Windows\Start Menu\Programs\Startup\x.exe`
//! is a classic "zip slip".

use std::path::PathBuf;

use crate::error::{Error, Result};

/// Converts an archive entry name into a relative path that is guaranteed to
/// stay inside the destination folder and to be valid on Windows.
///
/// - `..` anywhere → rejected (the caller should skip the entry and warn)
/// - leading `/`, `\` and drive letters → neutralised
/// - characters Windows forbids → replaced with `_`
/// - reserved device names (`CON`, `NUL`, `COM1`, …) → prefixed with `_`
pub fn sanitize_entry_path(raw: &str) -> Result<PathBuf> {
    let mut out = PathBuf::new();
    for component in raw.split(['/', '\\']) {
        match component {
            "" | "." => continue,
            ".." => return Err(Error::UnsafePath(raw.to_string())),
            name => out.push(sanitize_component(name)),
        }
    }
    if out.as_os_str().is_empty() {
        return Err(Error::UnsafePath(raw.to_string()));
    }
    Ok(out)
}

fn sanitize_component(name: &str) -> String {
    let mut clean: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();

    // Windows silently drops trailing dots and spaces, which would make two
    // different entries collide on disk.
    let kept = clean.trim_end_matches(['.', ' ']).len();
    clean.truncate(kept);

    if clean.is_empty() {
        clean.push('_');
    }
    if is_reserved_on_windows(&clean) {
        clean.insert(0, '_');
    }
    clean
}

fn is_reserved_on_windows(name: &str) -> bool {
    // `CON.txt` is just as reserved as `CON`.
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    match stem.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" => true,
        s => {
            let digit = s.strip_prefix("COM").or_else(|| s.strip_prefix("LPT"));
            matches!(digit, Some(d) if d.len() == 1 && d.as_bytes()[0].is_ascii_digit() && d != "0")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn ok(raw: &str) -> PathBuf {
        sanitize_entry_path(raw).unwrap()
    }

    #[test]
    fn normal_paths_pass_through() {
        assert_eq!(
            ok("Season 1/Episode 01.mkv"),
            Path::new("Season 1").join("Episode 01.mkv")
        );
        assert_eq!(
            ok("Café/Ünïcode – ok.mkv"),
            Path::new("Café").join("Ünïcode – ok.mkv")
        );
    }

    #[test]
    fn backslashes_are_separators() {
        assert_eq!(ok(r"a\b\c.txt"), Path::new("a").join("b").join("c.txt"));
    }

    #[test]
    fn parent_traversal_is_rejected() {
        for raw in ["../evil.exe", r"..\..\evil.exe", "a/../../b", "ok/.."] {
            assert!(
                matches!(sanitize_entry_path(raw), Err(Error::UnsafePath(_))),
                "{raw}"
            );
        }
    }

    #[test]
    fn absolute_paths_and_drive_letters_are_made_relative() {
        assert_eq!(ok("/etc/passwd"), Path::new("etc").join("passwd"));
        assert_eq!(
            ok(r"C:\Windows\x.dll"),
            Path::new("C_").join("Windows").join("x.dll")
        );
        assert_eq!(
            ok(r"\\server\share\f"),
            Path::new("server").join("share").join("f")
        );
    }

    #[test]
    fn forbidden_characters_are_replaced() {
        assert_eq!(ok("what?.mkv"), Path::new("what_.mkv"));
        assert_eq!(ok("a<b>c|d*e\"f"), Path::new("a_b_c_d_e_f"));
        assert_eq!(ok("tab\there"), Path::new("tab_here"));
    }

    #[test]
    fn trailing_dots_and_spaces_are_trimmed() {
        assert_eq!(
            ok("folder. /file .txt"),
            Path::new("folder").join("file .txt")
        );
        assert_eq!(ok("..."), Path::new("_"));
    }

    #[test]
    fn reserved_names_are_prefixed() {
        assert_eq!(ok("con"), Path::new("_con"));
        assert_eq!(ok("NUL.txt"), Path::new("_NUL.txt"));
        assert_eq!(ok("dir/com1.log"), Path::new("dir").join("_com1.log"));
        assert_eq!(ok("COM0"), Path::new("COM0")); // not reserved
        assert_eq!(ok("console.txt"), Path::new("console.txt"));
    }

    #[test]
    fn empty_names_are_rejected() {
        for raw in ["", "/", "./", r"\\"] {
            assert!(sanitize_entry_path(raw).is_err(), "{raw:?}");
        }
    }
}
