//! In-process libav (rsmpeg) backend: a resident demuxer + decoder session, so `seek` is a
//! sample-table lookup plus a partial GOP decode rather than a process spawn.
//!
//! Not implemented. When it is, it should:
//! - link through `rsmpeg` with `link_system_ffmpeg` (ffmpeg 8.x headers; pkg-config);
//! - decode to the native planar format with no `sws_scale`, and copy planes stride-free into
//!   `harness_core::Planes`;
//! - implement `seek` as `avformat_seek_file` backward to the keyframe followed by decode-forward
//!   to the exact pts (content-relative: subtract the stream's start_time);
//! - optionally take the `videotoolbox` hwaccel so hardware decode can be compared with
//!   FilmCraft's `filmcraft-platform` path.
//! switchblade's `SeekablePlayer` (crates/sb-media/src/seekable.rs) already does all of this and
//! is the thing to port.

use harness_core::{BackendDesc, DecodeError, Decoder, Result};
use std::path::Path;

pub const DESC: BackendDesc = BackendDesc { name: "libav", open };

pub fn open(_path: &Path) -> Result<Box<dyn Decoder>> {
    Err(DecodeError::Unsupported("libav backend is a stub; see backend-ffmpeg/src/libav.rs".into()))
}
