# Changelog

The release workflow publishes the section matching the pushed tag as the
release notes, so keep one `## [x.y.z]` heading per version.

## [0.1.0] - 2026-11-18

First release. Drag any part of an archive onto the program and get your
files out: it finds the other parts, asks for a password if needed, and
unpacks next to the archive. Or put it next to your downloads and
double-click it to unpack everything in that folder.

### Downloads

| You have | Download |
|----------|----------|
| Windows (64-bit) | `unpacker-windows-x64.exe` |
| Mac with Apple silicon (M1 or newer) | `unpacker-macos-arm64` |
| Linux (64-bit, any distribution) | `unpacker-linux-x64` |

No installation needed. On Windows, SmartScreen may say "Windows protected
your PC" because the program isn't signed yet: click **More info → Run
anyway**. On Mac and Linux, make the file executable first
(`chmod +x unpacker-…`). `SHA256SUMS` lets you verify a download.

### Formats

- **RAR** (RAR4 and RAR5): `.part1.rar` sets, old-style `.rar` + `.r00`
  sets, and `.rar.001` splits. Passwords, also with encrypted file lists.
- **7z**: single files and `.7z.001` splits. Passwords, also with encrypted
  file lists.
- **ZIP**: single files, `.zip.001` splits and spanned `.z01` sets.
  AES and classic ZIP passwords.
- **tar**: `.tar`, `.tar.gz`/`.tgz`, `.tar.bz2`, `.tar.xz`, `.tar.zst`, also
  split into `.001` parts.

### What you can count on

- Drop any part; missing parts are named: `Part "movie.part3.rar" is missing`.
- No pointless folders: one file comes out next to the archive, and one
  top-level folder isn't nested twice.
- Nothing is ever written outside the target folder, existing files are
  never overwritten, and a failed file never stays behind half-written.
- When everything came out fine, it offers to delete the archive files
  (permanently; Enter means yes). If anything was skipped, they're kept.
- A progress bar with speed and time left. `--help` lists the options for
  scripts (`--keep`, `--delete`).
