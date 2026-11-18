# unpacker

Drop archive parts on it, get your files out. One `.exe`, no install.

Made for people who download multi-part video archives (e.g. from Telegram)
and just want the video: drag *any* part onto the program, and it finds the
rest, asks for a password if needed, and unpacks next to the archive.

## Download

Always the newest version (links you can share):

- **Windows:** [unpacker-windows-x64.exe](https://github.com/jef00/unpacker/releases/latest/download/unpacker-windows-x64.exe)
- **Mac (Apple silicon):** [unpacker-macos-arm64](https://github.com/jef00/unpacker/releases/latest/download/unpacker-macos-arm64)
- **Linux (64-bit):** [unpacker-linux-x64](https://github.com/jef00/unpacker/releases/latest/download/unpacker-linux-x64)

Or pick a version on the [releases page](https://github.com/jef00/unpacker/releases).
Releases are built by CI: bump the version in `Cargo.toml`, add a
`CHANGELOG.md` section, then **Actions → CI → Run workflow** (or push a
`v<version>` tag).

## Using it

Drag any part of an archive onto the program. Or don't drag anything: put
the program next to the archives and double-click it. It unpacks every
archive in its own folder. (From a terminal, `unpacker` with no arguments
uses the folder you're in.)

While it works you see a progress bar with speed and time left. When every
file came out fine, it asks `Delete the 12 archive files permanently? [Y/n]`;
Enter means yes. Deleting is **permanent** (no Recycle Bin / Trash), so
answer `n` if you want to keep them. If anything was skipped, the archives
are always kept and you aren't asked. Without a console to answer on,
they're kept too.

For scripts: `--delete` deletes the archive files permanently without
asking; `--keep` keeps them. `--help` lists all
options; an unknown option (say, a typo like `--delet`) stops with an error
instead of being taken for a file name.

The last line per archive says what really happened: `✓ Extracted to …`,
with the number of skipped files if any, or `• Nothing extracted` when
everything was skipped (for example because it was already there).

## What it handles

| Format | Single | Multi-part                                        | Passwords                 |
|--------|--------|---------------------------------------------------|---------------------------|
| RAR    | ✅ RAR4/RAR5 | `.part1.rar`… · `.rar`+`.r00`… · `.rar.001`… | ✅ incl. encrypted headers |
| 7z     | ✅      | `.7z.001`, `.7z.002`, …                           | ✅ incl. encrypted headers |
| ZIP    | ✅      | `.zip.001`… · spanned `.z01`, `.z02`, …, `.zip`   | ✅ AES and ZipCrypto       |
| tar    | ✅ `.tar`, `.tar.gz`/`.tgz`, `.tar.bz2`, `.tar.xz`, `.tar.zst` | `.tar.001`, `.tar.gz.001`, … | — (tar has none) |

Tar archives: regular files and folders are extracted; symbolic links, hard
links and device files are skipped (with a warning, so the archive is kept).
Tar has no checksums on its contents, so a cut-off archive is caught by its
missing end marker instead: a split `.tar` with its last part missing is an
error, not a silently incomplete result. A lone `.gz`/`.xz` that isn't a tar
(say `notes.txt.gz`) is refused, and double-click mode leaves such files
alone.

Drop any part; the first one is found automatically. Missing parts are
reported by name: `Part "movie.part3.rar" is missing.`

No pointless folders:

- **One file** in the archive (even inside folders): it goes straight next
  to the archive. `movie.part1.rar` → `movie.mkv`.
- **One folder** holding everything: you get `movie/…`, not `movie/Movie/…`.
- **Anything else**: a folder named after the archive, `movie/`, with the
  archive's own structure inside.

An existing file is never overwritten, also not next to the archive.

On Windows, a drag-and-drop or double-click launch ends with "Press Enter
to close…" so you can read the result. Run from cmd, PowerShell or a script,
it exits straight away (no hanging batch jobs).

## Guarantees

- **Never writes outside the output folder.** Entry names are sanitised
  (`../`, absolute paths, drive letters, Windows-reserved names) and
  symbolic links are never extracted. Tested against a real malicious RAR.
- **Never leaves a half-written file.** Data goes to `*.unpacker-tmp` and
  is renamed only after it's complete and checksum-verified.
- **Never overwrites your files** unless asked (`Overwrite::Replace`).
- **Retrying with another password is always safe.** See the password
  contract on `Extractor::extract` for exactly when each error can occur.

## Architecture

```
main.rs          thin UI: args → discover → open → extract → report
discovery.rs     filenames → VolumeSet               (pure, heavily tested)
detect.rs        magic bytes → Format
multi_reader.rs  many files → one Read + Seek        (splits)
safety.rs        untrusted entry name → safe relative path
backend/
  mod.rs         Extractor trait, open(), password contract
  output.rs      plan_entry → TempPath → commit      (the only writer)
  zip.rs         `zip` crate, via MultiReader
  zip_spanned.rs spanned .z01 sets → rewritten single-disk view (ZIP64-aware)
  sevenz.rs      `sevenz-rust2`, via MultiReader
  rar.rs         `unrar` (UnRAR C++), opens volumes by name
  tar.rs         `tar` + gzip/bzip2/xz/zstd decoders, via MultiReader
```

Rules of the road:

- Discovery stays pure. New naming quirk? Add a test first.
- Every file written goes through `backend/output.rs`. Never write directly.
- No `unwrap`/`expect`/`panic` outside tests (enforced by clippy).
- Errors are user-facing: say what's wrong and what to do.

## Testing

```sh
cargo test             # 160 tests
cargo test --release   # same suite, optimised (what ships)
```

Most backend tests run against **real archives made by WinRAR, 7-Zip,
Info-ZIP and GNU tar** (`tests/fixtures/`, sources and licences in its README), with
expected checksums from independent tools (`rarfile`, `py7zr`). Several
tests run the actual binary, including typing passwords on stdin.

## Build

```sh
cargo build --release   # → target/release/unpacker(.exe), ~2 MB
```

- Needs Rust 1.93+ and a **C++ compiler** (UnRAR is C++): MSVC Build Tools
  on Windows, Xcode Command Line Tools on macOS, `g++` on Linux.
- The Windows build links the C/C++ runtime statically
  (`.cargo/config.toml`), so the `.exe` runs on a clean Windows install.
- The Linux release binary is fully static (musl), so it runs on any 64-bit
  Linux, old distros and NAS boxes included. CI builds it in Alpine; to do
  the same locally:

  ```sh
  docker run --rm -v "$PWD":/src -w /src rust:1-alpine \
    sh -c 'apk add --no-cache g++ musl-dev && cargo build --release'
  ```

CI uploads each binary as a plain file, no zip: `unpacker-windows-x64.exe`,
`unpacker-macos-arm64` and `unpacker-linux-x64`. (On macOS and Linux, run
`chmod +x` on it after downloading.)

### Vendored dependency

`vendor/unrar` is `unrar` 0.5.8 with a one-line fix for a buffer over-read
in its volume-change callback (undefined behaviour on every multi-volume
RAR). Details and removal instructions: `vendor/unrar/PATCHED.md`.

`build.rs` links `advapi32` on Windows, which `unrar_sys` 0.5.8 forgets
(it relied on Rust's std, which no longer links it). See the comment there.

## Licences

Own code: MIT. RAR support uses RarLab's UnRAR source, which may be used to
*extract* RAR archives but not to create them, which this program never does.

## Ideas / not done yet

- GUI window instead of a console.
