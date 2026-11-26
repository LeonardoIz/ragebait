//! Platform-level data formats, shared by every Xbox Series title.
//!
//! Each module is pure (bytes in, bytes out, no I/O): [`texture`] for the
//! GPU's tiled texture layouts and copies to and from linear rows,
//! [`bcpack`] for BCPack textures, and [`xma`] for XMA2 audio.

pub mod bcpack;
pub mod texture;
pub mod xma;
