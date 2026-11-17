# Test fixtures

Real archives made by the real tools, so the tests check against
what users actually download, not just against what our own test code writes.

## `zip/`

| File            | Made with                                      |
|-----------------|------------------------------------------------|
| `zipcrypto.zip` | Info-ZIP: `zip -P secret zipcrypto.zip hello.txt` |

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
