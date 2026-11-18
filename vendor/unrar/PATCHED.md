# Patched copy of `unrar` 0.5.8

Two changes: a fix for a buffer over-read (patch 1) and live extraction
progress (patch 2).

Upstream: https://github.com/muja/unrar.rs (MIT OR Apache-2.0, licences included).

## Patch 1: the bug

`src/open_archive.rs`, `Internal::callback`, message `UCM_CHANGEVOLUMEW`:

```rust
WideCString::from_ptr_truncate(p1 as *const _, 2048)
```

This builds a 2048-element slice from `p1`. For `RAR_VOL_NOTIFY`, UnRAR
(`vendor/unrar/volume.cpp`, `DllVolNotify`) passes `NextName.data()` of a
`std::wstring`, a buffer only as long as the file name. So every volume
change reads past the end of a heap buffer: undefined behaviour. Debug
builds abort with "unsafe precondition(s) violated"; release builds
silently over-read (and could crash if the buffer ends at a page boundary).

## The fix (one line)

```rust
WideCString::from_ptr_str(p1 as *const _)
```

UnRAR always passes a nul-terminated string, so scanning to the terminator
is both correct and in bounds.

## Patch 2: live progress (`extract_to_counting`)

Not a bug fix but an addition, so the program can show real progress for
RAR. UnRAR reports every chunk it writes through the `UCM_PROCESSDATA`
callback, also when extracting to a file; the crate's `Extract` mode
ignored it. `extract_to_counting(path, written)` is `extract_to` plus an
`ExtractCounting` mode that adds each chunk's length to a shared
`Arc<AtomicU64>`.

Why not watch the output file's size instead: on Windows, UnRAR's
`File::Prealloc` (file.cpp) sets a file over 1 MB to its full size before
writing any data, so the size jumps to 100% at once.

Changes, all marked `PATCHED (unpacker)` in `src/open_archive.rs`:
`extract_to_counting`, `ExtractCounting`, and `process_file_x_with` /
`process_file_raw_with`, which let the caller provide the callback's
initial output. Existing methods behave exactly as before.

## Status

Present in 0.5.8 and on upstream `master` as of 2026-11. Worth reporting
upstream; once a fixed release exists, delete this folder and the
`[patch.crates-io]` section in the root `Cargo.toml`.

Files trimmed from the published crate: `data/`, `examples/`, `tests/`.
