//! FilmCraft's `MediaSource`: the file is opened from memory with every registered container
//! and codec opener, and frames are requested by media time. FilmCraft keeps a GOP cache per
//! source, so sequential requests decode forward without re-seeking and a jump decodes forward
//! from the preceding sync sample. Pixels arrive in the stream's own planar format
//! (`Yuv8` / `Yuv16`) and are copied stride-free; nothing is colour-converted.
//!
//! This mirrors how EffectCraft's media pool uses FilmCraft (`crates/media/src/pool.rs` there),
//! except that EffectCraft reads containers in place through a positional `ByteReader`; reading
//! the whole file first is simpler and keeps I/O out of the decode timings.

use harness_core::{BackendDesc, Chroma, DecodeError, Decoder, Frame, Planes, Result, StreamInfo};
use filmcraft_media::{FrameRequest, MediaError, SharedSource};
use filmcraft_time::{Tick, TICKS_PER_SECOND};
use std::path::Path;
use std::sync::Arc;

pub const DESC: BackendDesc = BackendDesc { name: "filmcraft", open };
/// FilmCraft with the OS hardware decoder registered (`filmcraft-platform`): VideoToolbox on
/// macOS, Media Foundation on Windows, NVDEC / VA-API on Linux. Streams the hardware declines
/// (profile, size, chroma, other codecs) fall through to the software decoder; the report's
/// `pixel_format` says which one actually ran.
#[cfg(feature = "hw")]
pub const DESC_HW: BackendDesc = BackendDesc { name: "filmcraft-hw", open: open_hw };

pub fn open(path: &Path) -> Result<Box<dyn Decoder>> {
    Ok(Box::new(FilmCraft::open(path, false)?))
}

#[cfg(feature = "hw")]
pub fn open_hw(path: &Path) -> Result<Box<dyn Decoder>> {
    use std::sync::OnceLock;
    static REGISTERED: OnceLock<filmcraft_platform::Availability> = OnceLock::new();
    match REGISTERED.get_or_init(filmcraft_platform::register) {
        filmcraft_platform::Availability::Available(_) => Ok(Box::new(FilmCraft::open(path, true)?)),
        filmcraft_platform::Availability::Unavailable(why) => Err(DecodeError::Unsupported(format!("no hardware decoder: {why}"))),
    }
}

pub struct FilmCraft {
    source: SharedSource,
    info: StreamInfo,
    next: u64,
    frames: u64,
    /// Ask for the hardware decoder. The switch is process-global and read when a stream's
    /// decoder is first created, so it is set again before every request: `compare` runs two
    /// backends in one process.
    hardware: bool,
    hw_frames_at_open: u64,
}

impl FilmCraft {
    pub fn open(path: &Path, hardware: bool) -> Result<Self> {
        filmcraft_codecs::hw::set_hardware_decoding(hardware);
        let bytes: Arc<[u8]> = std::fs::read(path)?.into();
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let source = filmcraft_codecs::open_bytes(&name, bytes).map_err(open_error)?;
        let mi = source.info();
        let v = mi.video.as_ref().ok_or_else(|| DecodeError::Unsupported("no video stream".into()))?;
        let info = StreamInfo {
            codec: v.codec.clone(),
            container: mi.container.clone(),
            pixel_format: v.pixel_format.clone(),
            width: v.width,
            height: v.height,
            frame_rate: (v.frame_rate.num.max(0) as u32, v.frame_rate.den.max(1) as u32),
            duration_s: mi.duration.0 as f64 / TICKS_PER_SECOND as f64,
            frame_count: None,
        };
        let frames = info.frames();
        Ok(Self { source, info, next: 0, frames, hardware, hw_frames_at_open: filmcraft_codecs::hw::hw_stats().frames })
    }

    /// After the first picture: record in `pixel_format` whether hardware actually decoded it.
    fn note_backend(&mut self) {
        if self.next != 1 {
            return;
        }
        let stats = filmcraft_codecs::hw::hw_stats();
        let hw = stats.frames > self.hw_frames_at_open;
        let tag = match (self.hardware, hw, filmcraft_codecs::hw::hw_backend()) {
            (true, true, Some(b)) => format!(" ({b})"),
            (true, false, _) => " (software; hardware declined)".to_string(),
            _ => String::new(),
        };
        self.info.pixel_format.push_str(&tag);
    }

    /// Media time to ask for frame `index`: a quarter of a frame past its start. FilmCraft
    /// answers "the frame displayed at t"; containers with a coarse timescale (Matroska's 1 ms)
    /// store frame 2 of a 30 fps stream at 67 ms while it starts at 66.667 ms, so asking for the
    /// exact start lands on frame 1. A quarter-frame offset tolerates timestamps rounded either
    /// way (EffectCraft's media pool does the same; FilmCraft rounds requests itself after this pin).
    fn tick_of(&self, index: u64) -> Tick {
        let (num, den) = self.info.frame_rate;
        if num == 0 {
            return Tick(0);
        }
        let t = ((4 * index as i128 + 1) * den as i128 * TICKS_PER_SECOND as i128 + 2 * num as i128) / (4 * num as i128);
        Tick(t.clamp(0, i64::MAX as i128) as i64)
    }
}

fn open_error(e: MediaError) -> DecodeError {
    match e {
        MediaError::Unsupported(s) => DecodeError::Unsupported(s),
        MediaError::NoStream(s) => DecodeError::Unsupported(format!("no {s} stream")),
        MediaError::Io(s) | MediaError::Offline(s) => DecodeError::Open(s),
        other => DecodeError::Open(other.to_string()),
    }
}

fn decode_error(e: MediaError) -> DecodeError {
    match e {
        MediaError::Unsupported(s) => DecodeError::Unsupported(s),
        other => DecodeError::Decode(other.to_string()),
    }
}

fn convert(f: &filmcraft_frame::VideoFrame) -> Result<Frame> {
    use filmcraft_frame::PixelData;
    let chroma_of = |c: filmcraft_frame::Chroma| match c {
        filmcraft_frame::Chroma::C420 => Chroma::C420,
        filmcraft_frame::Chroma::C422 => Chroma::C422,
        filmcraft_frame::Chroma::C444 => Chroma::C444,
    };
    let (chroma, planes) = match &f.data {
        PixelData::Yuv8 { planes, chroma, alpha } => {
            let mut ps: Vec<Vec<u8>> = planes.iter().map(|p| p.as_ref().clone()).collect();
            if let Some(a) = alpha {
                ps.push(a.as_ref().clone());
            }
            (chroma_of(*chroma), Planes::Yuv8(ps))
        }
        PixelData::Yuv16 { planes, chroma, bits, alpha } => {
            let mut ps: Vec<Vec<u16>> = planes.iter().map(|p| p.as_ref().clone()).collect();
            if let Some(a) = alpha {
                ps.push(a.as_ref().clone());
            }
            (chroma_of(*chroma), Planes::Yuv16 { planes: ps, bits: *bits })
        }
        PixelData::Rgba8(d) => (Chroma::C444, Planes::Rgba8(d.as_ref().clone())),
        PixelData::RgbaF32(_) => return Err(DecodeError::Unsupported("decoder produced linear float RGBA; not comparable".into())),
    };
    Ok(Frame { width: f.width, height: f.height, chroma, planes, pts_s: f.pts.0 as f64 / TICKS_PER_SECOND as f64 })
}

impl Decoder for FilmCraft {
    fn backend(&self) -> &'static str {
        DESC.name
    }
    fn info(&self) -> &StreamInfo {
        &self.info
    }
    fn next_frame(&mut self) -> Result<Option<Frame>> {
        if self.next >= self.frames {
            return Ok(None);
        }
        let t = self.tick_of(self.next);
        filmcraft_codecs::hw::set_hardware_decoding(self.hardware);
        let vf = self.source.video_frame(FrameRequest::full(t)).map_err(decode_error)?;
        self.next += 1;
        self.note_backend();
        convert(&vf).map(Some)
    }
    fn seek(&mut self, index: u64) -> Result<()> {
        // FilmCraft seeks per request; positioning is just the index. The jump cost is paid by
        // the next `video_frame`, which is what the seek scenario times.
        self.next = index;
        Ok(())
    }
}
