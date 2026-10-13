//! ffmpeg as an external process. `ffprobe` describes the stream; `ffmpeg` decodes it to raw
//! planar Y'CbCr on a pipe in the stream's native pixel format, so nothing is converted.
//!
//! This backend is the oracle for correctness and a fair baseline for sequential decode cost
//! (process spawn is a one-off per run). It is **not** a fair baseline for seek latency: every
//! `seek` respawns ffmpeg with `-ss`, which is exactly the cost switchblade moved away from.
//! Use the `libav` feature for that comparison once it exists.
//!
//! Frames come out with `-fps_mode passthrough`, so frame `index` means the i-th decoded picture,
//! not a constant-frame-rate slot; on variable-frame-rate files that differs from `time_of(i)`.

use harness_core::{BackendDesc, Chroma, DecodeError, Decoder, Frame, Planes, Result, StreamInfo};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};

#[cfg(feature = "libav")]
pub mod libav;

pub const DESC: BackendDesc = BackendDesc { name: "ffmpeg", open };

pub fn open(path: &Path) -> Result<Box<dyn Decoder>> {
    Ok(Box::new(FfmpegCli::open(path)?))
}

pub struct FfmpegCli {
    path: PathBuf,
    info: StreamInfo,
    chroma: Chroma,
    bits: u32,
    pix_fmt: String,
    frame_bytes: usize,
    child: Option<(Child, ChildStdout)>,
    /// Index of the frame `next_frame` returns next.
    next: u64,
    buf: Vec<u8>,
}

impl FfmpegCli {
    pub fn open(path: &Path) -> Result<Self> {
        let probe = probe(path)?;
        let (chroma, bits) = Chroma::from_ffmpeg_pix_fmt(&probe.pix_fmt)
            .ok_or_else(|| DecodeError::Unsupported(format!("pixel format {} is not planar Y'CbCr", probe.pix_fmt)))?;
        let (cw, ch) = chroma.plane_size(probe.info.width, probe.info.height);
        let bps = if bits > 8 { 2 } else { 1 };
        let frame_bytes = ((probe.info.width * probe.info.height) as usize + 2 * (cw * ch) as usize) * bps;
        Ok(Self {
            path: path.to_path_buf(),
            pix_fmt: chroma.ffmpeg_pix_fmt(bits),
            info: probe.info,
            chroma,
            bits,
            frame_bytes,
            child: None,
            next: 0,
            buf: Vec::new(),
        })
    }

    fn spawn(&mut self, from_index: u64) -> Result<()> {
        self.stop();
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-nostdin", "-v", "error", "-threads", "0"]);
        if from_index > 0 {
            // Input seeking with accurate_seek (ffmpeg's default): lands on the keyframe, decodes
            // forward, drops frames before the target. Exact, and the whole cost is on us.
            cmd.args(["-ss", &format!("{:.6}", self.info.time_of(from_index))]);
        }
        cmd.arg("-i").arg(&self.path);
        cmd.args(["-map", "0:v:0", "-an", "-sn", "-dn", "-fps_mode", "passthrough", "-f", "rawvideo", "-pix_fmt", &self.pix_fmt, "-"]);
        cmd.stdout(Stdio::piped()).stderr(Stdio::null()).stdin(Stdio::null());
        let mut child = cmd.spawn().map_err(|e| DecodeError::Open(format!("spawn ffmpeg: {e}")))?;
        let out = child.stdout.take().ok_or_else(|| DecodeError::Open("ffmpeg stdout".into()))?;
        self.child = Some((child, out));
        self.next = from_index;
        Ok(())
    }

    fn stop(&mut self) {
        if let Some((mut child, out)) = self.child.take() {
            drop(out);
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Fill `buf` with one frame. `Ok(false)` at a clean end of stream.
    fn read_frame_bytes(&mut self) -> Result<bool> {
        let n = self.frame_bytes;
        self.buf.resize(n, 0);
        let Some((_, out)) = self.child.as_mut() else { return Ok(false) };
        let mut got = 0;
        while got < n {
            match out.read(&mut self.buf[got..]) {
                Ok(0) => break,
                Ok(k) => got += k,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        match got {
            0 => Ok(false),
            g if g == n => Ok(true),
            g => Err(DecodeError::Decode(format!("short frame from ffmpeg: {g} of {n} bytes"))),
        }
    }

    fn split_planes(&self) -> Planes {
        let (w, h) = (self.info.width as usize, self.info.height as usize);
        let (cw, ch) = self.chroma.plane_size(self.info.width, self.info.height);
        let sizes = [w * h, (cw * ch) as usize, (cw * ch) as usize];
        if self.bits == 8 {
            let mut off = 0;
            let planes = sizes
                .iter()
                .map(|&s| {
                    let p = self.buf[off..off + s].to_vec();
                    off += s;
                    p
                })
                .collect();
            Planes::Yuv8(planes)
        } else {
            let mut off = 0;
            let planes = sizes
                .iter()
                .map(|&s| {
                    let p = self.buf[off..off + 2 * s].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                    off += 2 * s;
                    p
                })
                .collect();
            Planes::Yuv16 { planes, bits: self.bits }
        }
    }
}

impl Drop for FfmpegCli {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Decoder for FfmpegCli {
    fn backend(&self) -> &'static str {
        DESC.name
    }
    fn info(&self) -> &StreamInfo {
        &self.info
    }
    fn next_frame(&mut self) -> Result<Option<Frame>> {
        if self.child.is_none() {
            let at = self.next;
            self.spawn(at)?;
        }
        if !self.read_frame_bytes()? {
            return Ok(None);
        }
        let index = self.next;
        self.next += 1;
        Ok(Some(Frame { width: self.info.width, height: self.info.height, chroma: self.chroma, planes: self.split_planes(), pts_s: self.info.time_of(index) }))
    }
    fn seek(&mut self, index: u64) -> Result<()> {
        self.spawn(index)
    }
}

struct Probe {
    info: StreamInfo,
    pix_fmt: String,
}

fn probe(path: &Path) -> Result<Probe> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=codec_name,pix_fmt,width,height,r_frame_rate,avg_frame_rate,nb_frames,duration:format=format_name,duration", "-of", "json"])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| DecodeError::Open(format!("spawn ffprobe: {e}")))?;
    if !out.status.success() {
        return Err(DecodeError::Open(format!("ffprobe failed: {}", String::from_utf8_lossy(&out.stderr).trim())));
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|e| DecodeError::Open(format!("ffprobe json: {e}")))?;
    let stream = v["streams"].get(0).ok_or_else(|| DecodeError::Unsupported("no video stream".into()))?;
    let s = |k: &str| stream[k].as_str().unwrap_or("").to_string();
    let ratio = |k: &str| -> (u32, u32) {
        let t = s(k);
        let mut it = t.split('/').filter_map(|x| x.trim().parse::<u32>().ok());
        match (it.next(), it.next()) {
            (Some(n), Some(d)) if d > 0 => (n, d),
            (Some(n), None) => (n, 1),
            _ => (0, 1),
        }
    };
    let mut rate = ratio("avg_frame_rate");
    if rate.0 == 0 {
        rate = ratio("r_frame_rate");
    }
    let duration = s("duration").parse::<f64>().ok().or_else(|| v["format"]["duration"].as_str().and_then(|d| d.parse().ok())).unwrap_or(0.0);
    Ok(Probe {
        pix_fmt: s("pix_fmt"),
        info: StreamInfo {
            codec: s("codec_name"),
            container: v["format"]["format_name"].as_str().unwrap_or("").to_string(),
            pixel_format: s("pix_fmt"),
            width: stream["width"].as_u64().unwrap_or(0) as u32,
            height: stream["height"].as_u64().unwrap_or(0) as u32,
            frame_rate: rate,
            duration_s: duration,
            frame_count: s("nb_frames").parse().ok(),
        },
    })
}

/// `ffmpeg -version`'s first line, for run metadata.
pub fn version() -> Option<String> {
    let out = Command::new("ffmpeg").arg("-version").stdin(Stdio::null()).output().ok()?;
    String::from_utf8_lossy(&out.stdout).lines().next().map(str::to_string)
}
