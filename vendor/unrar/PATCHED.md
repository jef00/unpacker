# Patched copy of `unrar` 0.5.8

Upstream: https://github.com/muja/unrar.rs (MIT OR Apache-2.0, licences included).

## The bug

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

## Status

Present in 0.5.8 and on upstream `master` as of 2026-11. Worth reporting
upstream; once a fixed release exists, delete this folder and the
`[patch.crates-io]` section in the root `Cargo.toml`.

Files trimmed from the published crate: `data/`, `examples/`, `tests/`.
