//! Build script: one Windows link fix.
//!
//! UnRAR (compiled by `unrar_sys`) calls advapi32 functions: registry
//! (`RegOpenKeyExW`), process tokens (`OpenProcessToken`) and the old crypto
//! API (`CryptGenRandom`). `unrar_sys` 0.5.8 never links advapi32: it relied
//! on Rust's std doing so, which recent Rust versions no longer do. Without
//! this, linking on Windows fails with LNK2019 "unresolved external symbol
//! __imp_RegCloseKey" and friends.
//!
//! Drop this once `unrar_sys` links advapi32 itself.

fn main() {
    // CARGO_CFG_TARGET_OS (not `cfg!(windows)`) so cross-compiling works too.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rustc-link-lib=advapi32");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
