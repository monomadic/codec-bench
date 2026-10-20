# codec-bench

A headless harness that runs the same files through three decoder stacks and writes down what
happened: how fast, how much CPU, whether the pixels agree, and how each one fails.

| backend | what it is | role |
|---|---|---|
| `ffmpeg` | `ffprobe` + `ffmpeg` as external processes, raw planar output on a pipe | the oracle for pixels; a fair sequential-decode baseline; **not** a fair seek baseline (one process per seek) |
| `libav` (feature) | in-process libav via rsmpeg, resident demuxer + decoder; a port of switchblade's `SeekablePlayer` without the playback machinery | the fair seek baseline |
| `libav-vt` (feature, macOS) | the same with the VideoToolbox hwaccel, frames downloaded to CPU planes | the hardware baseline FilmCraft's `filmcraft-platform` has to match |
| `filmcraft` | FilmCraft's pure-Rust demuxers and decoders, git-pinned | what EffectCraft ships; the "could we drop ffmpeg" candidate |
| `filmcraft-hw` (feature) | the same with `filmcraft-platform` registered: VideoToolbox / Media Foundation / NVDEC / VA-API in front of the software decoders, automatic fallback | the pure-Rust answer to `libav-vt`; its `pixel_format` says which decoder actually ran |
| `own` | your decoder crates | starts by failing every file; the report is the ladder |

The default build links nothing: it needs only the `ffmpeg`/`ffprobe` binaries on `PATH` for the
oracle backend and for `scripts/make-fixtures.sh`. `--features libav` links the ffmpeg 8.x
libraries through rsmpeg (`brew install ffmpeg@8 pkgconf`; `.cargo/config.toml` points
pkg-config at the keg).

## Run

```sh
cargo build --release
./scripts/make-fixtures.sh                # ffmpeg-generated clips → corpus/files/ (gitignored)
./target/release/codec-bench suite --hash --max-frames 300 --seeks 12
./target/release/codec-bench suite --hash --psnr ...   # also dump luma planes so the report can show PSNR
cargo build --release --features libav,filmcraft-hw   # adds libav, libav-vt and filmcraft-hw
./target/release/codec-bench report runs/<timestamp>
```

Other entry points:

```sh
codec-bench backends                                   # what this binary was built with
codec-bench run --backend filmcraft --scenario decode --hash corpus/files/fixtures/h264_1080p_gop250.mp4
codec-bench compare --a ffmpeg --b filmcraft corpus/files/fixtures/vp9_1080p.webm   # per-frame PSNR, in one process
codec-bench suite --set malformed --scenarios robust   # just the crash hunt
```

`suite` runs every (backend × file × scenario) as its own subprocess. A crash or hang is a row in
the report, not the end of the run. CPU time and peak RSS come from the kernel's child accounting
(`getrusage(RUSAGE_CHILDREN)`), so the ffmpeg processes of the CLI backend are counted too.

## Scenarios

- **decode**: every frame (or the first N) in order; wall ms per frame, CPU core-ms per frame, per-frame MD5 with `--hash`.
- **seek**: N random exact seeks from a seed (every backend gets the same targets); p50/p95 latency and whether the seeked frames match the reference's sequential decode at the same index.

With `--psnr` each run also dumps luma planes (`<out>/frames/<backend>/<file>/<scenario>/<index>.y`): every seeked frame, and in decode every `--dump-every`-th frame (default 50) plus the seek targets. The report reads them back and adds a **min PSNR** column: the lowest luma PSNR of a backend's dumped frames against the reference's sequential-decode frame of the same index, ∞ when all identical. Dumps run to ~2 MB per 1080p frame and 8 MB at 4K, per backend; nothing is dumped for the robust scenario. Dumping and hashing happen between decode calls, so timings exclude them, but gigabytes of writes evict the fixtures from the page cache, and seek latencies of large files (ProRes especially) read higher on a `--psnr` run than on a plain one. Take timings from a run without `--psnr`, and PSNR from one with it; `suite --out <same dir>` merges, replacing only the rows it re-runs.
- **robust**: open and decode a little; the only output is how it failed. Malformed files run only this.

The runner never judges. The report compares. If you want pass/fail, write it over `results.jsonl`.

## Canonical frame

Planar Y'CbCr at the stream's own subsampling and bit depth (`Planes::Yuv8` / `Yuv16`), tightly
packed, Y Cb Cr [A]. The harness never colour-converts, so a difference is the decoder's, not the
converter's. Frame MD5s are the harness's own convention (planes concatenated, 16-bit samples
little-endian); conformance suites' reference MD5s use their own layouts and need per-suite
adapters (see `docs/DESIGN.md`).

## What the first runs showed (macOS, Apple silicon, whole files, 12 seeks)

Numbers are from `runs/all` on this machine (FilmCraft at `cee1feb`, 2026-10-13) and will differ
on yours; they are here to show the kind of thing the harness finds, not as results.

Exact random seeks on long-GOP (250) fixtures, p50 ms:

| file | ffmpeg CLI | libav | libav-vt | filmcraft | filmcraft-hw |
|---|---|---|---|---|---|
| H.264 1080p | 145 | 90 | 301 | 150 | 71 |
| H.264 2160p | 394 | 240 | 597 | 542 | 224 |
| HEVC 1080p 8-bit | 111 | 73 | 151 | 112 | 56 |
| HEVC 1080p 10-bit | 179 | 123 | 53 | 149 | 70 |
| VP9 1080p | 164 | 99 | 258 | 233 | 210 (software) |
| AV1 1080p | 146 | 111 | 182 | 812 | 838 (software) |
| ProRes 422 | 114 | 24 | n/a | 3.3 | 3.5 |

- **ffmpeg returns the wrong picture for exact seeks into open-GOP HEVC.** Seeking to frame 248 of a stream whose next CRA is frame 250 gives the CRA picture, both from the CLI's `-ss` and from in-process `avformat_seek_file` + decode-forward, software or VideoToolbox. Frames 246–249 are RASL leading pictures that follow the CRA in decode order; libav discards them after a random access. FilmCraft decodes them and returns the frame that ffmpeg's own *sequential* decode produces for that index. This is why the seek table's `true` column judges against sequential decode, not against the reference's seek.
- **FilmCraft's hardware path (VideoToolbox) is bit-exact with the software decoders and with ffmpeg**, and is faster than libav's VideoToolbox on exact seeks (71 vs 301 ms on H.264 1080p) because FilmCraft decodes the GOP from the keyframe while libav also downloads every intermediate frame. VP9 and AV1 fall through to software, and the tag says so.
- **FilmCraft PR #2180 (VP9 through VideoToolbox, open as of 2026-10-21) measured at its head `1285fff9`:** bit-exact with ffmpeg; 1080p 1.8 ms/frame (libav-vt 3.5), 2160p 5.9 ms/frame (libav-vt 8.3); exact seeks p50 89 / 340 ms against libav-vt's 314 / 811. H.264 and HEVC stay bit-exact at that revision. To measure a PR, point the `filmcraft-*` git deps at its head commit; the `[patch]` block only works when the local checkout's crate versions are semver-compatible with the pin (0.1.x at `2c5ba7a`, 0.8.0 on current main).
- **FilmCraft's HEVC seek failure at the old pin (`2c5ba7a`) is fixed at `cee1feb`.** AV1 seeks went from 1345 to 812 ms between the pins; sequential AV1 is still ~10× slower than dav1d.

- **Why FilmCraft's VideoToolbox path out-seeks libav's on the same silicon.** Not threading (one thread vs auto moves seeks under 10 %) and not readback (skipping the download leaves decode cost unchanged, `CODEC_BENCH_LIBAV_NOREADBACK=1`). ffmpeg's hwaccel submits one frame and calls `VTDecompressionSessionWaitForAsynchronousFrames` before the next (libavcodec/videotoolbox.c, release/8.0), so the hardware never pipelines; FilmCraft keeps a couple of access units in flight. Decode-only throughput shows it directly: 1080p H.264 3.7 ms/frame through libav vs 2.0 through FilmCraft, 4K 9.1 vs 5.4.
- **Which codecs reach the hardware.** libav-vt: H.264, HEVC, VP9, AV1 (M3 or later) and ProRes (`p210le`, now handled). filmcraft-hw at `cee1feb`: H.264 and HEVC; VP9 with PR 2180; AV1 and ProRes in software. VideoToolbox's ProRes decoder differs from ffmpeg's software one by one LSB (74 dB) and from FilmCraft's by less (78 dB); none of the three agree exactly, as expected for a codec without a normative decoder.
- **In-process libav is bit-exact with the CLI everywhere, and so is VideoToolbox** on these fixtures (NV12/P010 de-interleaved; samples identical). So a hardware path can be checked for correctness, not just speed.
- **ProRes shows `0/12` true seeks for FilmCraft** with a min PSNR around 74 dB: one-LSB rounding, not wrong frames. ffmpeg's wrong HEVC seek shows as `11/12` with a PSNR in the twenties: a different picture. The PSNR column is what tells those apart.
- **VideoToolbox seeks are slower in wall time than software on long-GOP 8-bit H.264 and VP9, at a tenth of the CPU.** An exact seek is a serial GOP decode (~7 ms/frame under VT, as switchblade's design record says); software fans the GOP across cores. VT wins outright on 10-bit HEVC. This is the number that settles scrub = keyframe seeks, settle = one exact seek.
- **Sequential decode CPU per frame**, 1080p H.264: CLI 11.8, libav 9.0, VT 5.0, FilmCraft 16.3 core-ms. `libav-vt` wall per frame includes the download to CPU memory (~3 ms at 1080p).

- **Bit-exact with ffmpeg**: H.264 (all three fixtures, including VFR), HEVC 8- and 10-bit, AV1, VP9 (after asking a quarter-frame into each frame, see the note in `backend-filmcraft`).
- **ProRes differs by one LSB** (luma PSNR ≈ 74.5 dB): expected, ProRes has no normative decoder; the report says "differs @0" because it is strictly about bit-exactness.
- **FilmCraft is faster than ffmpeg's software path on H.264 and 10-bit HEVC in wall time, but spends more CPU** (it threads wider): 1080p long-GOP H.264 at 1.6 vs 2.6 ms/frame wall, 20 vs 16 core-ms.
- **FilmCraft's AV1 decoder is ~10× slower** than ffmpeg's (dav1d): 22 vs 2.3 ms/frame.
- **ffmpeg conceals the bit-flipped file and keeps going; FilmCraft stops at the first invalid `ref_idx`.** Neither crashed. Whether to conceal or stop is a policy question the harness just records.
- The ffmpeg CLI backend's seek numbers are process spawns (its floor is ~100 ms even on intra-only ProRes, where libav takes 22 ms); that is why switchblade moved to in-process libav.
- On the bit-flipped file `libav-vt` stops with an opaque VideoToolbox error while software libav conceals; the two ffmpeg paths differ in robustness, not only speed.

## Layout

```
crates/harness-core      Decoder trait, Frame, StreamInfo, RunResult, Summary, Rng
crates/backend-ffmpeg    CLI backend (+ libav / libav-vt behind the `libav` feature)
crates/backend-filmcraft FilmCraft MediaSource adapter
crates/backend-own       your decoders
crates/codec-bench       the binary: run / suite / report / compare
corpus/manifest.toml     every file the suite knows about, by set
scripts/                 make-fixtures.sh, fetch-conformance.sh
docs/DESIGN.md           decisions and the rules for adding a backend or a corpus set
```

## License

MIT or Apache-2.0, at your option. ffmpeg is never linked by the default build; the `libav`
feature links the system ffmpeg libraries through rsmpeg and inherits their licence terms.
