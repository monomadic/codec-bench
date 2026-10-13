//! The contract every backend implements, the canonical frame they all decode into, and the
//! result records the runner and report exchange.
//!
//! The harness never converts colour: a frame is compared in the stream's own planar Y'CbCr at
//! its own bit depth, so decoder differences aren't hidden behind RGB rounding.

use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub mod metrics;

/// Chroma subsampling of planar Y'CbCr.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Chroma {
    C420,
    C422,
    C444,
}

impl Chroma {
    /// (horizontal shift, vertical shift) of the chroma planes.
    pub fn shifts(self) -> (u32, u32) {
        match self {
            Chroma::C420 => (1, 1),
            Chroma::C422 => (1, 0),
            Chroma::C444 => (0, 0),
        }
    }

    /// Chroma plane size for a luma plane of `w` × `h`.
    pub fn plane_size(self, w: u32, h: u32) -> (u32, u32) {
        let (sx, sy) = self.shifts();
        ((w + (1 << sx) - 1) >> sx, (h + (1 << sy) - 1) >> sy)
    }

    /// The ffmpeg pixel-format name for this subsampling at a bit depth.
    pub fn ffmpeg_pix_fmt(self, bits: u32) -> String {
        let base = match self {
            Chroma::C420 => "yuv420p",
            Chroma::C422 => "yuv422p",
            Chroma::C444 => "yuv444p",
        };
        if bits == 8 { base.to_string() } else { format!("{base}{bits}le") }
    }

    /// Parse an ffmpeg planar pixel-format name: `yuv420p`, `yuv422p10le`, `yuvj444p`, …
    /// Returns (chroma, bits). `None` for packed, RGB, gray or alpha formats.
    pub fn from_ffmpeg_pix_fmt(name: &str) -> Option<(Chroma, u32)> {
        let rest = name.strip_prefix("yuvj").or_else(|| name.strip_prefix("yuv"))?;
        let (chroma, rest) = if let Some(r) = rest.strip_prefix("420p") {
            (Chroma::C420, r)
        } else if let Some(r) = rest.strip_prefix("422p") {
            (Chroma::C422, r)
        } else if let Some(r) = rest.strip_prefix("444p") {
            (Chroma::C444, r)
        } else {
            return None;
        };
        if rest.is_empty() {
            return Some((chroma, 8));
        }
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        let tail = &rest[digits.len()..];
        if tail != "le" {
            return None; // big-endian or something exotic: not canonical
        }
        let bits: u32 = digits.parse().ok()?;
        (9..=16).contains(&bits).then_some((chroma, bits))
    }
}

/// What a backend learned about the file on open.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StreamInfo {
    pub codec: String,
    pub container: String,
    /// The backend's own name for the decoded format (`yuv420p`, `Yuv16/10`, …), informational.
    pub pixel_format: String,
    pub width: u32,
    pub height: u32,
    /// Frames per second as a ratio (num, den).
    pub frame_rate: (u32, u32),
    pub duration_s: f64,
    /// The container's frame count when it states one.
    pub frame_count: Option<u64>,
}

impl StreamInfo {
    pub fn fps(&self) -> f64 {
        if self.frame_rate.1 == 0 { 0.0 } else { self.frame_rate.0 as f64 / self.frame_rate.1 as f64 }
    }

    /// The stated frame count, else the one the duration and rate imply.
    pub fn frames(&self) -> u64 {
        self.frame_count.unwrap_or_else(|| (self.duration_s * self.fps()).round().max(0.0) as u64)
    }

    /// Media time of frame `index` in seconds.
    pub fn time_of(&self, index: u64) -> f64 {
        if self.frame_rate.0 == 0 { 0.0 } else { index as f64 * self.frame_rate.1 as f64 / self.frame_rate.0 as f64 }
    }
}

/// Decoded samples. Planes are tightly packed (no stride), in Y, Cb, Cr order, with an optional
/// alpha plane last.
#[derive(Clone, Debug)]
pub enum Planes {
    Yuv8(Vec<Vec<u8>>),
    /// `bits` significant bits per sample, stored little-endian in u16.
    Yuv16 { planes: Vec<Vec<u16>>, bits: u32 },
    /// Interleaved 8-bit RGBA: what some intra codecs (and all stills) decode to.
    Rgba8(Vec<u8>),
}

impl Planes {
    pub fn bits(&self) -> u32 {
        match self {
            Planes::Yuv8(_) | Planes::Rgba8(_) => 8,
            Planes::Yuv16 { bits, .. } => *bits,
        }
    }
}

/// One decoded picture in canonical form.
#[derive(Clone, Debug)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub chroma: Chroma,
    pub planes: Planes,
    /// Presentation time in seconds, as the backend reports it (content-relative).
    pub pts_s: f64,
}

impl Frame {
    /// MD5 of the planes in order (Y, Cb, Cr, alpha), 16-bit samples little-endian. This is the
    /// harness's own convention, used to compare backends with each other; it is not the MD5
    /// convention of any conformance suite.
    pub fn md5(&self) -> String {
        let mut h = Md5::new();
        match &self.planes {
            Planes::Yuv8(ps) => ps.iter().for_each(|p| h.update(p)),
            Planes::Yuv16 { planes, .. } => planes.iter().for_each(|p| p.iter().for_each(|s| h.update(s.to_le_bytes()))),
            Planes::Rgba8(d) => h.update(d),
        }
        format!("{:x}", h.finalize())
    }

    /// Luma plane as f64 samples, for PSNR. `None` for RGBA frames.
    fn luma(&self) -> Option<Vec<f64>> {
        match &self.planes {
            Planes::Yuv8(ps) => ps.first().map(|p| p.iter().map(|&s| s as f64).collect()),
            Planes::Yuv16 { planes, .. } => planes.first().map(|p| p.iter().map(|&s| s as f64).collect()),
            Planes::Rgba8(_) => None,
        }
    }

    /// Peak signal-to-noise ratio of the luma planes, in dB. `None` when the frames differ in
    /// size or depth or aren't Y'CbCr; `f64::INFINITY` when identical.
    pub fn luma_psnr(&self, other: &Frame) -> Option<f64> {
        if (self.width, self.height) != (other.width, other.height) || self.planes.bits() != other.planes.bits() {
            return None;
        }
        let (a, b) = (self.luma()?, other.luma()?);
        if a.len() != b.len() || a.is_empty() {
            return None;
        }
        let mse = a.iter().zip(&b).map(|(x, y)| (x - y) * (x - y)).sum::<f64>() / a.len() as f64;
        let peak = ((1u32 << self.planes.bits()) - 1) as f64;
        Some(if mse == 0.0 { f64::INFINITY } else { 10.0 * (peak * peak / mse).log10() })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    /// The backend knows it can't do this (codec, container, pixel format): not a bug.
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("open: {0}")]
    Open(String),
    #[error("decode: {0}")]
    Decode(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, DecodeError>;

/// A decoder positioned in one file. Frames come out in presentation order.
pub trait Decoder: Send {
    fn backend(&self) -> &'static str;
    fn info(&self) -> &StreamInfo;
    /// The next frame in presentation order, `None` at the end of the stream.
    fn next_frame(&mut self) -> Result<Option<Frame>>;
    /// Position the decoder so that the next `next_frame` returns frame `index` exactly (not
    /// the preceding keyframe). Backends that can only seek to keyframes decode forward.
    fn seek(&mut self, index: u64) -> Result<()>;
}

/// How the harness constructs a backend's decoder.
pub type OpenFn = fn(&Path) -> Result<Box<dyn Decoder>>;

#[derive(Clone, Copy)]
pub struct BackendDesc {
    pub name: &'static str,
    pub open: OpenFn,
}

/// How a run ended. The runner adds `Crash` and `Timeout`; a backend process only reports the
/// first three.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    Ok,
    Unsupported { reason: String },
    Error { message: String },
    Crash { status: String },
    Timeout { after_s: f64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scenario {
    /// Decode every frame (or the first N) in order.
    Decode,
    /// Random exact seeks.
    Seek,
    /// Open and decode a little; the only question is how it fails.
    Robust,
}

impl Scenario {
    pub const ALL: [Scenario; 3] = [Scenario::Decode, Scenario::Seek, Scenario::Robust];
    pub fn name(self) -> &'static str {
        match self {
            Scenario::Decode => "decode",
            Scenario::Seek => "seek",
            Scenario::Robust => "robust",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|x| x.name() == s)
    }
}

/// One seek: which frame, how long until it was decoded, and what came out.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SeekSample {
    pub index: u64,
    pub wall_ms: f64,
    pub md5: Option<String>,
    pub pts_s: Option<f64>,
}

/// The record one backend process writes for one file and scenario. The runner fills in the
/// process-level fields (`cpu_ms`, `peak_rss_bytes`, crash and timeout outcomes).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunResult {
    pub backend: String,
    pub file: String,
    pub scenario: Scenario,
    pub outcome: Outcome,
    pub info: Option<StreamInfo>,
    /// Frames decoded (decode and robust scenarios).
    pub frames: u64,
    /// Wall time of the measured section (open excluded).
    pub wall_ms: f64,
    pub open_ms: f64,
    /// Per-frame wall time summary (decode scenario).
    pub frame_ms: Option<metrics::Summary>,
    /// Per-seek wall time summary (seek scenario).
    pub seek_ms: Option<metrics::Summary>,
    pub seeks: Vec<SeekSample>,
    /// Per-frame MD5 in decode order, when hashing was requested.
    pub frame_md5: Vec<String>,
    /// Process CPU time (user + system) of the backend process and its children.
    pub cpu_ms: Option<f64>,
    pub peak_rss_bytes: Option<u64>,
    pub host: Option<String>,
    /// Where this run wrote luma dumps (`<index>.y`), when asked to.
    #[serde(default)]
    pub dump_dir: Option<String>,
}

impl RunResult {
    pub fn new(backend: &str, file: &Path, scenario: Scenario) -> Self {
        Self {
            backend: backend.to_string(),
            file: file.display().to_string(),
            scenario,
            outcome: Outcome::Ok,
            info: None,
            frames: 0,
            wall_ms: 0.0,
            open_ms: 0.0,
            frame_ms: None,
            seek_ms: None,
            seeks: Vec::new(),
            frame_md5: Vec::new(),
            cpu_ms: None,
            peak_rss_bytes: None,
            host: None,
            dump_dir: None,
        }
    }

    pub fn fail(mut self, e: &DecodeError) -> Self {
        self.outcome = match e {
            DecodeError::Unsupported(r) => Outcome::Unsupported { reason: r.clone() },
            other => Outcome::Error { message: other.to_string() },
        };
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pix_fmt_parsing() {
        assert_eq!(Chroma::from_ffmpeg_pix_fmt("yuv420p"), Some((Chroma::C420, 8)));
        assert_eq!(Chroma::from_ffmpeg_pix_fmt("yuvj420p"), Some((Chroma::C420, 8)));
        assert_eq!(Chroma::from_ffmpeg_pix_fmt("yuv422p10le"), Some((Chroma::C422, 10)));
        assert_eq!(Chroma::from_ffmpeg_pix_fmt("yuv444p12le"), Some((Chroma::C444, 12)));
        assert_eq!(Chroma::from_ffmpeg_pix_fmt("yuv420p10be"), None);
        assert_eq!(Chroma::from_ffmpeg_pix_fmt("rgb24"), None);
        assert_eq!(Chroma::from_ffmpeg_pix_fmt("yuva420p"), None);
        assert_eq!(Chroma::C420.ffmpeg_pix_fmt(10), "yuv420p10le");
    }

    #[test]
    fn psnr_of_identical_is_infinite() {
        let f = Frame { width: 2, height: 2, chroma: Chroma::C444, planes: Planes::Yuv8(vec![vec![1, 2, 3, 4]; 3]), pts_s: 0.0 };
        assert_eq!(f.luma_psnr(&f), Some(f64::INFINITY));
        let mut g = f.clone();
        if let Planes::Yuv8(p) = &mut g.planes {
            p[0][0] = 2;
        }
        let db = f.luma_psnr(&g).unwrap();
        assert!(db > 50.0 && db < 60.0, "{db}");
        assert_ne!(f.md5(), g.md5());
    }
}

/// Luma-plane dumps: what the report needs to compute PSNR between backends after the fact,
/// without either process decoding the other's frames. One file per frame, `<index>.y`:
/// magic `CBY1`, width, height, bits (u32 LE each), then the samples (u8, or u16 LE).
pub mod dump {
    use super::{Frame, Planes};
    use std::io::{Read, Write};
    use std::path::Path;

    const MAGIC: &[u8; 4] = b"CBY1";

    pub struct Luma {
        pub width: u32,
        pub height: u32,
        pub bits: u32,
        pub samples: Vec<u16>,
    }

    impl Luma {
        pub fn of(frame: &Frame) -> Option<Self> {
            let samples = match &frame.planes {
                Planes::Yuv8(ps) => ps.first()?.iter().map(|&s| s as u16).collect(),
                Planes::Yuv16 { planes, .. } => planes.first()?.clone(),
                Planes::Rgba8(_) => return None,
            };
            Some(Self { width: frame.width, height: frame.height, bits: frame.planes.bits(), samples })
        }

        pub fn write(&self, path: &Path) -> std::io::Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
            f.write_all(MAGIC)?;
            for v in [self.width, self.height, self.bits] {
                f.write_all(&v.to_le_bytes())?;
            }
            if self.bits <= 8 {
                f.write_all(&self.samples.iter().map(|&s| s as u8).collect::<Vec<u8>>())?;
            } else {
                let mut buf = Vec::with_capacity(self.samples.len() * 2);
                for s in &self.samples {
                    buf.extend_from_slice(&s.to_le_bytes());
                }
                f.write_all(&buf)?;
            }
            f.flush()
        }

        pub fn read(path: &Path) -> std::io::Result<Self> {
            let mut f = std::io::BufReader::new(std::fs::File::open(path)?);
            let mut head = [0u8; 16];
            f.read_exact(&mut head)?;
            if &head[..4] != MAGIC {
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "not a luma dump"));
            }
            let u = |i: usize| u32::from_le_bytes([head[i], head[i + 1], head[i + 2], head[i + 3]]);
            let (width, height, bits) = (u(4), u(8), u(12));
            let n = width as usize * height as usize;
            let mut body = Vec::new();
            f.read_to_end(&mut body)?;
            let samples: Vec<u16> = if bits <= 8 {
                body.iter().map(|&b| b as u16).collect()
            } else {
                body.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect()
            };
            if samples.len() != n {
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "luma dump size mismatch"));
            }
            Ok(Self { width, height, bits, samples })
        }

        /// PSNR in dB against `other`; `None` when sizes or depths differ; infinite when equal.
        pub fn psnr(&self, other: &Luma) -> Option<f64> {
            if (self.width, self.height, self.bits) != (other.width, other.height, other.bits) || self.samples.is_empty() {
                return None;
            }
            let mse = self.samples.iter().zip(&other.samples).map(|(&a, &b)| (a as f64 - b as f64).powi(2)).sum::<f64>() / self.samples.len() as f64;
            let peak = ((1u32 << self.bits) - 1) as f64;
            Some(if mse == 0.0 { f64::INFINITY } else { 10.0 * (peak * peak / mse).log10() })
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::{Chroma, Frame, Planes};

        #[test]
        fn round_trip_and_psnr() {
            let dir = std::env::temp_dir().join(format!("codec-bench-dump-{}", std::process::id()));
            let f = Frame { width: 3, height: 2, chroma: Chroma::C444, planes: Planes::Yuv16 { planes: vec![vec![1, 2, 3, 4, 5, 1023]; 3], bits: 10 }, pts_s: 0.0 };
            let l = Luma::of(&f).unwrap();
            let p = dir.join("7.y");
            l.write(&p).unwrap();
            let back = Luma::read(&p).unwrap();
            assert_eq!(back.samples, l.samples);
            assert_eq!(back.psnr(&l), Some(f64::INFINITY));
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}
