//! Unpacker library: discovery → detection → extraction.
//!
//! The binary in `main.rs` is a thin shell around this; a GUI later can
//! reuse everything here unchanged.

pub mod backend;
pub mod detect;
pub mod discovery;
pub mod error;
pub mod multi_reader;
pub mod origin;
pub mod safety;

pub use discovery::{Scheme, VolumeSet};
pub use error::{Error, Result};
