//! The slot for your own demuxer and decoder crates.
//!
//! Keep the harness's rules in mind if you want the result to be clean-room: implement from the
//! specification and the conformance vectors; let the `ffmpeg` backend's output be the oracle in
//! the report, but don't read reference decoder source. Record the spec edition in each crate.
//!
//! A sensible first target is an intra-only codec with a public spec (ProRes, SMPTE RDD 36), so
//! the whole pipeline (container → bitstream → planes → report) is proven before a long-GOP
//! codec. For H.264 the vector ladder is roughly: I-only CAVLC → P → B → CABAC → weighted
//! prediction → interlace / MBAFF → high-bit-depth and 4:2:2 profiles.

use harness_core::{BackendDesc, DecodeError, Decoder, Result};
use std::path::Path;

pub const DESC: BackendDesc = BackendDesc { name: "own", open };

pub fn open(path: &Path) -> Result<Box<dyn Decoder>> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    Err(DecodeError::Unsupported(format!("no own decoder yet for .{ext} (backend-own is a stub)")))
}
