# Test fixtures

Real archives made by the real tools, so the tests check against
what users actually download, not just against what our own test code writes.

## `zip/`

| File            | Made with                                      |
|-----------------|------------------------------------------------|
| `zipcrypto.zip` | Info-ZIP: `zip -P secret zipcrypto.zip hello.txt` |
| `spanned.z01`…`.z05`, `.zip` | `zip -r -s 64k`: `video.bin` + 1200 small files; data *and* central directory cross parts |
| `spanned-pw.z01`, `.zip` | `zip -s 64k -P secret`: ZipCrypto in a spanned set |
| `spanned64.z01`, `.z02`, `.zip` | `zip -fz -s 64k`: forced ZIP64 records |

Spanned content is deterministic so tests can check every byte:
`video.bin`/`secret.bin` are the first 150 000 / 100 000 bytes of
`x = (x * 1103515245 + 12345) mod 2^31; byte = (x >> 16) & 0xFF` from
`x = 12345` (`lcg_bytes` in `tests/zip_backend.rs`), and `files/NNNN.txt`
is `"file N\n"` repeated `N % 7 + 1` times.

## `tar/`

Made with GNU tar 1.35 (`--sort=name --owner=0 --group=0 --mtime=2026-01-01`)
and the standard `gzip -9 -n`, `bzip2 -9`, `xz -9`, and libzstd level 19 (via
Python `zstandard`). All `show*` files hold the same `Show/` folder:

| Entry | Covers |
|-------|--------|
| `Show/Season 1/episode.mkv` | 60 000 bytes from `lcg_bytes` (see `zip/` below) |
| `Show/a-very-long-folder-name-…/and-an-even-longer-file-name-…txt` | path over 100 characters (GNU long name / PAX) |
| `Show/täst ünïcode.txt` | non-ASCII name |
| `Show/Extras/` | empty folder |
| `Show/link-out` → `../../../etc/passwd` | symlink pointing outside: must be skipped |
| `Show/hardlink.srt` | hard link: skipped |

| File | Format |
|------|--------|
| `show.tar` | GNU tar format |
| `show.tar.gz`, `show.tar.bz2`, `show.tar.zst` | the same, compressed |
| `show-pax.tar.xz` | PAX (POSIX.1-2001) format, xz |
| `evil.tar` | Python `tarfile`: `../escaped.txt`, `/abs.txt`, symlink `up` → `..`, then `up/pwned.txt` |
| `notes.txt.gz` | a gzip that isn't a tar: must be refused |

## `7z/`

Copied from the [py7zr](https://github.com/miurahr/py7zr) test suite
(`tests/data/`, LGPL-2.1). Used only as test input; nothing is linked
into the binary.

| File                         | What it covers                                   |
|------------------------------|--------------------------------------------------|
| `archive.7z.001`, `.002`     | real 7-Zip multi-volume split, solid, LZMA2+BCJ  |
| `encrypted_1.7z`             | AES-256, password `secret`                       |
| `filename_encryption.7z`     | encrypted headers (`-mhe`), password `hello`     |
| `crc_corrupted.7z`           | bad CRC on `src/scripts/py7zr`                   |
| `solid.7z`, `copy.7z`        | solid LZMA, stored (no compression)              |
| `umlaut-non_solid.7z`        | non-ASCII filename `täst.txt`                    |
| `lzma2bcj.7z`                | LZMA2 + BCJ filter, nested folders               |

## `rar/`

Copied from the [rarfile](https://github.com/markokr/rarfile) test suite
(`test/files/`, ISC licence, © Marko Kreen). Made with WinRAR / `rar`.

| File                                   | What it covers                          |
|----------------------------------------|-----------------------------------------|
| `rar3-old.rar`, `.r00`, `.r01`         | RAR3 old-style volumes                  |
| `rar3-vols.part1-3.rar`                | RAR3 new-style volumes                  |
| `rar5-vols.part1-3.rar`                | RAR5 new-style volumes                  |
| `rar5-psw.rar`                         | RAR5 encrypted data, password `password`|
| `rar5-hpsw.rar`                        | RAR5 encrypted headers, password `password` |
| `rar3-comment-psw.rar`                 | RAR3 encrypted data, password `password`|
| `rar5-evil-symlink-traversal.rar`      | malicious symlink pointing to `..`      |
| `rar5-subdirs.rar`                     | nested folders                          |
| `rar3-solid.rar`, `rar5-solid.rar`     | solid archives                          |
