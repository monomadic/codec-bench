//! In-process libav through rsmpeg: a resident demuxer and decoder, so `seek` is
//! `avformat_seek_file` to the keyframe before the target plus a decode-forward to the exact
//! picture, with no process spawn. Two backends come out of this module: `libav` (software
//! decode) and, on macOS, `libav-vt` (VideoToolbox hwaccel, frames downloaded to CPU planes).
//!
//! This is switchblade's `SeekablePlayer` (crates/sb-media/src/seekable.rs) with the playback
//! machinery removed: no demux thread, no paced queue, no filter graph, no pacing anchor. What is
//! kept, and why, in that file's words:
//! - the AVIO interrupt callback is installed *before* `avformat_open_input`, so a reader stuck in
//!   libav I/O can be told to stop;
//! - a hardware lane picks libav's *native* decoder by codec name, because `av_find_best_stream`
//!   returns the first registered decoder (libdav1d ahead of native `av1`), which never reaches
//!   VideoToolbox;
//! - time is content-relative: the stream's `start_time` (edit lists, phone footage) is added
//!   when seeking and subtracted from every pts;
//! - a seek lands backward on the keyframe and drops decoded frames until the target
//!   (`skip_until`), the "exact" seek;
//! - hardware frames are downloaded with `av_hwframe_transfer_data` (NV12 / P010) and
//!   de-interleaved here to planar, which keeps the samples bit-identical to the software path.

use harness_core::{BackendDesc, Chroma, DecodeError, Decoder, Frame, Planes, Result, StreamInfo};
use rsmpeg::ffi;
use std::ffi::{c_void, CStr, CString};
use std::path::Path;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};

pub const DESC: BackendDesc = BackendDesc { name: "libav", open: open_sw };
#[cfg(target_os = "macos")]
pub const DESC_VT: BackendDesc = BackendDesc { name: "libav-vt", open: open_vt };

pub fn open_sw(path: &Path) -> Result<Box<dyn Decoder>> {
    Ok(Box::new(Libav::open(path, false)?))
}

#[cfg(target_os = "macos")]
pub fn open_vt(path: &Path) -> Result<Box<dyn Decoder>> {
    Ok(Box::new(Libav::open(path, true)?))
}

// RAII for the libav objects, so every early return frees.
struct FmtCtx(*mut ffi::AVFormatContext);
impl Drop for FmtCtx {
    fn drop(&mut self) {
        unsafe { ffi::avformat_close_input(&mut self.0) }
    }
}
struct DecCtx(*mut ffi::AVCodecContext);
impl Drop for DecCtx {
    fn drop(&mut self) {
        unsafe { ffi::avcodec_free_context(&mut self.0) }
    }
}
struct HwDev(*mut ffi::AVBufferRef);
impl Drop for HwDev {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { ffi::av_buffer_unref(&mut self.0) }
        }
    }
}
struct FramePtr(*mut ffi::AVFrame);
impl Drop for FramePtr {
    fn drop(&mut self) {
        unsafe { ffi::av_frame_free(&mut self.0) }
    }
}
struct PktPtr(*mut ffi::AVPacket);
impl Drop for PktPtr {
    fn drop(&mut self) {
        unsafe { ffi::av_packet_free(&mut self.0) }
    }
}

/// AVIO interrupt callback: nonzero aborts the blocking libav call with AVERROR_EXIT.
unsafe extern "C" fn check_stop(opaque: *mut c_void) -> std::os::raw::c_int {
    unsafe { (*(opaque as *const AtomicBool)).load(Ordering::Relaxed) as i32 }
}

#[cfg(target_os = "macos")]
unsafe extern "C" fn get_hw_format(_ctx: *mut ffi::AVCodecContext, fmts: *const ffi::AVPixelFormat) -> ffi::AVPixelFormat {
    unsafe {
        let mut p = fmts;
        while *p != ffi::AV_PIX_FMT_NONE {
            if *p == ffi::AV_PIX_FMT_VIDEOTOOLBOX {
                return *p;
            }
            p = p.add(1);
        }
        *fmts
    }
}

fn quiet_libav_once() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe { ffi::av_log_set_level(ffi::AV_LOG_ERROR as i32) });
}

fn err_str(code: i32) -> String {
    let mut buf = [0u8; 128];
    unsafe {
        if ffi::av_strerror(code, buf.as_mut_ptr() as *mut _, buf.len()) < 0 {
            return format!("error {code}");
        }
    }
    CStr::from_bytes_until_nul(&buf).map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|_| format!("error {code}"))
}

fn cstr(p: *const std::os::raw::c_char) -> String {
    if p.is_null() { String::new() } else { unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned() }
}

pub struct Libav {
    info: StreamInfo,
    // Field order is drop order: decoder before the device it references, contexts before the
    // interrupt flag they point at.
    frame: FramePtr,
    transfer: FramePtr,
    pkt: PktPtr,
    dec: DecCtx,
    _hw: HwDev,
    fmt: FmtCtx,
    stop_io: Box<AtomicBool>,
    sidx: i32,
    /// Seconds per stream pts unit.
    tb: f64,
    /// Absolute stream start time in seconds; content-relative time = pts × tb − start_off.
    start_off: f64,
    frame_s: f64,
    /// Exact-seek refinement: drop decoded frames before this content-relative time.
    skip_until: Option<f64>,
    eof_sent: bool,
    hardware: bool,
    name: &'static str,
}

// Single-threaded use of raw libav pointers; nothing is shared.
unsafe impl Send for Libav {}

impl Libav {
    pub fn open(path: &Path, use_vt: bool) -> Result<Self> {
        quiet_libav_once();
        let cpath = c_path(path).ok_or_else(|| DecodeError::Open("path is not a C string".into()))?;
        let stop_io = Box::new(AtomicBool::new(false));
        unsafe {
            let mut fmt_raw = ffi::avformat_alloc_context();
            if fmt_raw.is_null() {
                return Err(DecodeError::Open("avformat_alloc_context".into()));
            }
            (*fmt_raw).interrupt_callback = ffi::AVIOInterruptCB { callback: Some(check_stop), opaque: &*stop_io as *const AtomicBool as *mut c_void };
            let r = ffi::avformat_open_input(&mut fmt_raw, cpath.as_ptr(), ptr::null(), ptr::null_mut());
            if r < 0 {
                // avformat_open_input frees a caller-supplied context on failure.
                return Err(DecodeError::Open(format!("avformat_open_input: {}", err_str(r))));
            }
            let fmt = FmtCtx(fmt_raw);
            let r = ffi::avformat_find_stream_info(fmt.0, ptr::null_mut());
            if r < 0 {
                return Err(DecodeError::Open(format!("avformat_find_stream_info: {}", err_str(r))));
            }
            let mut codec: *const ffi::AVCodec = ptr::null();
            let sidx = ffi::av_find_best_stream(fmt.0, ffi::AVMEDIA_TYPE_VIDEO, -1, -1, &mut codec, 0);
            if sidx < 0 {
                return Err(DecodeError::Unsupported("no video stream".into()));
            }
            let stream = *(*fmt.0).streams.add(sidx as usize);
            let par = (*stream).codecpar;
            let codec_name = cstr(ffi::avcodec_get_name((*par).codec_id));
            if use_vt {
                let native = ffi::avcodec_find_decoder_by_name(ffi::avcodec_get_name((*par).codec_id));
                if !native.is_null() {
                    codec = native;
                }
            }
            if codec.is_null() {
                return Err(DecodeError::Unsupported(format!("no decoder for {codec_name}")));
            }
            let stream_tb = (*stream).time_base;
            let tb = stream_tb.num as f64 / stream_tb.den as f64;
            let start_off = if (*stream).start_time == ffi::AV_NOPTS_VALUE { 0.0 } else { (*stream).start_time as f64 * tb };

            let dec = DecCtx(ffi::avcodec_alloc_context3(codec));
            if dec.0.is_null() {
                return Err(DecodeError::Open("avcodec_alloc_context3".into()));
            }
            let r = ffi::avcodec_parameters_to_context(dec.0, par);
            if r < 0 {
                return Err(DecodeError::Open(format!("avcodec_parameters_to_context: {}", err_str(r))));
            }
            // Auto (frame threads) like the CLI and switchblade's player, unless
            // CODEC_BENCH_LIBAV_THREADS says otherwise. abner runs one thread when VideoToolbox is
            // up: frame threads add a packet of post-flush delay each and do nothing for a
            // hardware decode. The variable exists to measure that claim.
            (*dec.0).thread_count = std::env::var("CODEC_BENCH_LIBAV_THREADS").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
            let mut hw = HwDev(ptr::null_mut());
            let mut hardware = false;
            #[cfg(target_os = "macos")]
            if use_vt {
                if ffi::av_hwdevice_ctx_create(&mut hw.0, ffi::AV_HWDEVICE_TYPE_VIDEOTOOLBOX, ptr::null(), ptr::null_mut(), 0) == 0 {
                    (*dec.0).hw_device_ctx = ffi::av_buffer_ref(hw.0);
                    (*dec.0).get_format = Some(get_hw_format);
                    hardware = true;
                } else {
                    return Err(DecodeError::Unsupported("VideoToolbox device unavailable".into()));
                }
            }
            #[cfg(not(target_os = "macos"))]
            if use_vt {
                return Err(DecodeError::Unsupported("VideoToolbox is macOS only".into()));
            }
            let r = ffi::avcodec_open2(dec.0, codec, ptr::null_mut());
            if r < 0 {
                return Err(DecodeError::Open(format!("avcodec_open2: {}", err_str(r))));
            }

            let fr = (*stream).avg_frame_rate;
            let fr = if fr.num > 0 && fr.den > 0 { fr } else { (*stream).r_frame_rate };
            let frame_rate = if fr.num > 0 && fr.den > 0 { (fr.num as u32, fr.den as u32) } else { (0, 1) };
            let frame_s = if frame_rate.0 > 0 { frame_rate.1 as f64 / frame_rate.0 as f64 } else { 1.0 / 30.0 };
            let duration_s = if (*stream).duration != ffi::AV_NOPTS_VALUE && (*stream).duration > 0 {
                (*stream).duration as f64 * tb
            } else if (*fmt.0).duration > 0 {
                (*fmt.0).duration as f64 / ffi::AV_TIME_BASE as f64
            } else {
                0.0
            };
            let pix = cstr(ffi::av_get_pix_fmt_name((*par).format));
            let info = StreamInfo {
                codec: codec_name,
                container: cstr((*(*fmt.0).iformat).name),
                pixel_format: if hardware { format!("{pix} (videotoolbox)") } else { pix },
                width: (*par).width.max(0) as u32,
                height: (*par).height.max(0) as u32,
                frame_rate,
                duration_s,
                frame_count: ((*stream).nb_frames > 0).then_some((*stream).nb_frames as u64),
            };
            Ok(Self {
                info,
                frame: FramePtr(ffi::av_frame_alloc()),
                transfer: FramePtr(ffi::av_frame_alloc()),
                pkt: PktPtr(ffi::av_packet_alloc()),
                dec,
                _hw: hw,
                fmt,
                stop_io,
                sidx,
                tb,
                start_off,
                frame_s,
                skip_until: None,
                eof_sent: false,
                hardware,
                name: if use_vt { "libav-vt" } else { "libav" },
            })
        }
    }

    pub fn hardware(&self) -> bool {
        self.hardware
    }

    /// Feed one packet of our stream (or EOF) to the decoder. `Ok(false)` once EOF has been sent.
    unsafe fn feed(&mut self) -> Result<bool> {
        unsafe {
            loop {
                let r = ffi::av_read_frame(self.fmt.0, self.pkt.0);
                if r == ffi::AVERROR_EOF {
                    if !self.eof_sent {
                        self.eof_sent = true;
                        let _ = ffi::avcodec_send_packet(self.dec.0, ptr::null());
                    }
                    return Ok(false);
                }
                if r < 0 {
                    return Err(DecodeError::Decode(format!("av_read_frame: {}", err_str(r))));
                }
                if (*self.pkt.0).stream_index != self.sidx {
                    ffi::av_packet_unref(self.pkt.0);
                    continue;
                }
                let sr = ffi::avcodec_send_packet(self.dec.0, self.pkt.0);
                ffi::av_packet_unref(self.pkt.0);
                if sr < 0 && sr != ffi::AVERROR(ffi::EAGAIN) {
                    // Corrupt packet: skip it, keep the stream alive (as the player does).
                    continue;
                }
                return Ok(true);
            }
        }
    }

    /// The decoded frame (hardware frames downloaded), converted to the canonical form.
    unsafe fn take(&mut self) -> Result<Frame> {
        unsafe {
            let mut f = self.frame.0;
            let pts = (*f).best_effort_timestamp;
            let pts_s = if pts == ffi::AV_NOPTS_VALUE { f64::NAN } else { pts as f64 * self.tb - self.start_off };
            #[cfg(target_os = "macos")]
            if (*f).format == ffi::AV_PIX_FMT_VIDEOTOOLBOX {
                ffi::av_frame_unref(self.transfer.0);
                let r = ffi::av_hwframe_transfer_data(self.transfer.0, f, 0);
                if r < 0 {
                    return Err(DecodeError::Decode(format!("av_hwframe_transfer_data: {}", err_str(r))));
                }
                let _ = ffi::av_frame_copy_props(self.transfer.0, f);
                f = self.transfer.0;
            }
            let out = convert(f, pts_s);
            ffi::av_frame_unref(self.frame.0);
            ffi::av_frame_unref(self.transfer.0);
            out
        }
    }
}

impl Drop for Libav {
    fn drop(&mut self) {
        self.stop_io.store(true, Ordering::Relaxed);
    }
}

impl Decoder for Libav {
    fn backend(&self) -> &'static str {
        self.name
    }
    fn info(&self) -> &StreamInfo {
        &self.info
    }
    fn next_frame(&mut self) -> Result<Option<Frame>> {
        unsafe {
            loop {
                let rr = ffi::avcodec_receive_frame(self.dec.0, self.frame.0);
                if rr == ffi::AVERROR(ffi::EAGAIN) {
                    if !self.feed()? {
                        // EOF sent and nothing more came out before EAGAIN would not happen;
                        // libav answers AVERROR_EOF once drained.
                        continue;
                    }
                    continue;
                }
                if rr == ffi::AVERROR_EOF {
                    return Ok(None);
                }
                if rr < 0 {
                    return Err(DecodeError::Decode(format!("avcodec_receive_frame: {}", err_str(rr))));
                }
                if let Some(t) = self.skip_until {
                    let pts = (*self.frame.0).best_effort_timestamp;
                    // Pre-target GOP frame. A quarter frame of slack tolerates timestamps rounded
                    // to a coarse container timescale either way.
                    if pts != ffi::AV_NOPTS_VALUE && pts as f64 * self.tb - self.start_off < t - 0.25 * self.frame_s {
                        ffi::av_frame_unref(self.frame.0);
                        continue;
                    }
                    self.skip_until = None;
                }
                // CODEC_BENCH_LIBAV_NOREADBACK=1: measure decode alone; the picture is left on
                // the GPU and a blank frame of the right size is returned. Diagnostics only.
                if std::env::var_os("CODEC_BENCH_LIBAV_NOREADBACK").is_some() {
                    let pts = (*self.frame.0).best_effort_timestamp;
                    let pts_s = if pts == ffi::AV_NOPTS_VALUE { f64::NAN } else { pts as f64 * self.tb - self.start_off };
                    ffi::av_frame_unref(self.frame.0);
                    return Ok(Some(Frame { width: self.info.width, height: self.info.height, chroma: Chroma::C420, planes: Planes::Yuv8(vec![Vec::new(); 3]), pts_s }));
                }
                return self.take().map(Some);
            }
        }
    }
    fn seek(&mut self, index: u64) -> Result<()> {
        let target = self.info.time_of(index);
        let ts = ((target + self.start_off) / self.tb).round() as i64;
        unsafe {
            // Land on the keyframe at or before the target; forward fallback is a player
            // concern (looping near EOF), not a harness one.
            let r = ffi::avformat_seek_file(self.fmt.0, self.sidx, i64::MIN, ts, ts, ffi::AVSEEK_FLAG_BACKWARD as i32);
            if r < 0 {
                return Err(DecodeError::Decode(format!("avformat_seek_file: {}", err_str(r))));
            }
            ffi::avcodec_flush_buffers(self.dec.0);
        }
        self.eof_sent = false;
        self.skip_until = Some(target);
        Ok(())
    }
}

#[cfg(unix)]
fn c_path(p: &Path) -> Option<CString> {
    use std::os::unix::ffi::OsStrExt;
    CString::new(p.as_os_str().as_bytes()).ok()
}
#[cfg(not(unix))]
fn c_path(p: &Path) -> Option<CString> {
    CString::new(p.to_string_lossy().as_bytes()).ok()
}

/// Copy a frame's planes stride-free into the canonical form. Planar Y'CbCr at 8 or 9–16 bits,
/// plus the semi-planar NV12 / P010 that hardware download produces (de-interleaved; samples
/// unchanged, P010 shifted down from its top-aligned 16-bit storage).
unsafe fn convert(f: *const ffi::AVFrame, pts_s: f64) -> Result<Frame> {
    unsafe {
        let (w, h) = ((*f).width as u32, (*f).height as u32);
        let fmt = (*f).format;
        let plane8 = |i: usize, pw: u32, ph: u32| -> Vec<u8> {
            let stride = (*f).linesize[i] as usize;
            let base = (*f).data[i];
            let mut out = Vec::with_capacity((pw * ph) as usize);
            for y in 0..ph as usize {
                out.extend_from_slice(std::slice::from_raw_parts(base.add(y * stride), pw as usize));
            }
            out
        };
        let plane16 = |i: usize, pw: u32, ph: u32, shift: u32| -> Vec<u16> {
            let stride = (*f).linesize[i] as usize;
            let base = (*f).data[i];
            let mut out = Vec::with_capacity((pw * ph) as usize);
            for y in 0..ph as usize {
                let row = std::slice::from_raw_parts(base.add(y * stride), 2 * pw as usize);
                out.extend(row.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]]) >> shift));
            }
            out
        };
        let planar8 = |chroma: Chroma| {
            let (cw, ch) = chroma.plane_size(w, h);
            Frame { width: w, height: h, chroma, planes: Planes::Yuv8(vec![plane8(0, w, h), plane8(1, cw, ch), plane8(2, cw, ch)]), pts_s }
        };
        let planar16 = |chroma: Chroma, bits: u32| {
            let (cw, ch) = chroma.plane_size(w, h);
            Frame { width: w, height: h, chroma, planes: Planes::Yuv16 { planes: vec![plane16(0, w, h, 0), plane16(1, cw, ch, 0), plane16(2, cw, ch, 0)], bits }, pts_s }
        };
        let frame = match fmt {
            x if x == ffi::AV_PIX_FMT_YUV420P || x == ffi::AV_PIX_FMT_YUVJ420P => planar8(Chroma::C420),
            x if x == ffi::AV_PIX_FMT_YUV422P || x == ffi::AV_PIX_FMT_YUVJ422P => planar8(Chroma::C422),
            x if x == ffi::AV_PIX_FMT_YUV444P || x == ffi::AV_PIX_FMT_YUVJ444P => planar8(Chroma::C444),
            x if x == ffi::AV_PIX_FMT_YUV420P10LE => planar16(Chroma::C420, 10),
            x if x == ffi::AV_PIX_FMT_YUV422P10LE => planar16(Chroma::C422, 10),
            x if x == ffi::AV_PIX_FMT_YUV444P10LE => planar16(Chroma::C444, 10),
            x if x == ffi::AV_PIX_FMT_YUV420P12LE => planar16(Chroma::C420, 12),
            x if x == ffi::AV_PIX_FMT_YUV422P12LE => planar16(Chroma::C422, 12),
            x if x == ffi::AV_PIX_FMT_YUV444P12LE => planar16(Chroma::C444, 12),
            x if x == ffi::AV_PIX_FMT_NV12 || x == ffi::AV_PIX_FMT_NV16 => {
                let chroma = if x == ffi::AV_PIX_FMT_NV12 { Chroma::C420 } else { Chroma::C422 };
                let (cw, ch) = chroma.plane_size(w, h);
                let uv = plane8(1, cw * 2, ch);
                let (u, v): (Vec<u8>, Vec<u8>) = uv.chunks_exact(2).map(|c| (c[0], c[1])).unzip();
                Frame { width: w, height: h, chroma, planes: Planes::Yuv8(vec![plane8(0, w, h), u, v]), pts_s }
            }
            x if x == ffi::AV_PIX_FMT_P010LE || x == ffi::AV_PIX_FMT_P210LE => {
                // 10-bit biplanar, samples in the top bits of 16 (what VideoToolbox returns for
                // 10-bit HEVC and ProRes 422).
                let chroma = if x == ffi::AV_PIX_FMT_P010LE { Chroma::C420 } else { Chroma::C422 };
                let (cw, ch) = chroma.plane_size(w, h);
                let uv = plane16(1, cw * 2, ch, 6);
                let (u, v): (Vec<u16>, Vec<u16>) = uv.chunks_exact(2).map(|c| (c[0], c[1])).unzip();
                Frame { width: w, height: h, chroma, planes: Planes::Yuv16 { planes: vec![plane16(0, w, h, 6), u, v], bits: 10 }, pts_s }
            }
            other => return Err(DecodeError::Unsupported(format!("pixel format {} is not planar Y'CbCr", cstr(ffi::av_get_pix_fmt_name(other))))),
        };
        Ok(frame)
    }
}
