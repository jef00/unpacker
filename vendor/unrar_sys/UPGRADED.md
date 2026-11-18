# unrar_sys 0.5.8 with UnRAR 7.23

This is unrar_sys 0.5.8 from crates.io. The Rust code is unchanged. Only the
bundled RARLAB UnRAR C++ source was upgraded, from 7.01 (2024) to **7.23**
(June 2026, unrarsrc-7.2.7, the same engine as WinRAR 7.23).

## Where the source came from

`vendor/unrar/` is commit d861246 ("Updated to 7.2.7") of
github.com/pmachapman/unrar, a mirror of the rarlab.com source tarballs.
rarlab.com wasn't reachable from the build machine. To check the mirror,
its 7.0.9 commit was compared with the official tarball that unrar_sys 0.5.8
ships. They are identical apart from CRLF line endings and the patches below.

## Changes on top of the RARLAB source

unrar_sys keeps four patches (`vendor/patches.txt`, commits in muja/unrar.rs):

| Commit | What | In 7.23 |
|---|---|---|
| 5accdb7 | `RAROpenArchiveEx` returns the handle on a broken header instead of NULL, so the Rust side can report the error | ported by hand (`dll.cpp`) |
| c3b414e | avoid `__builtin_cpu_supports` (link errors) | applied cleanly |
| f2adc58 | drop the WMI/comsupp code for windows-gnu | not needed: RARLAB disabled that code (`#if 0`) |
| 7db15f7 | invalid UTF-8 byte in a comment | not needed: gone upstream |

## Build changes (`build.rs`)

- `largepage.cpp` is new in 7.2x and is compiled on every platform.
- `motw.cpp` (Mark of the Web) is new and is compiled on Windows only, matching
  RARLAB's Windows DLL project. The Unix makefile leaves it out.

## C API

`RAR_DLL_VERSION` went from 9 to 10. The only struct change is a new
`MarkOfTheWeb` pointer in `RAROpenArchiveDataEx`. It takes space that used to
be `Reserved`, which the Rust bindings already zero. NULL means "don't copy
Mark of the Web", which is the old behaviour, so `src/lib.rs` is unchanged.

## Behaviour change: one archive at a time

In 7.2x, `RARReadHeaderEx` and `RARProcessFile` also return UnRAR's global
error state (`ErrHandler`), which only `RAROpenArchiveEx` resets. Two archives
open in different threads would see each other's errors. The program only
unpacks one archive at a time anyway, but `backend::unrar_lock()` now makes
that a rule: the RAR backend holds it from open to close. Tests running in
parallel need it too. Without it, about 1 in 20 runs failed on a 2-CPU machine.

## Upgrading again

Replace `vendor/unrar/` with a newer unrarsrc and redo the `dll.cpp` and
`rijndael.cpp`/`system.cpp` changes above. Compare the source list in
`build.rs` with `OBJECTS` in the makefile and with `UnRARDll.vcxproj`.
Compare `dll.hpp` with the structs in `src/lib.rs`. Then run the full test
suite on all three OSes (CI does this).
