# codec-bench design notes

## Why a separate repo

EffectCraft decodes single frames on demand for a compositor; switchblade plays many streams at
once and lives on seek latency. Neither codebase is a neutral place to compare decoders, and
neither wants ffmpeg as a build dependency. This harness is where ffmpeg is allowed to exist, as
an oracle, so the apps don't have to carry it.

## Rules

1. **The runner measures; it never judges.** No thresholds in code. `results.jsonl` is the
   product; `report` is one reading of it.
2. **One subprocess per run.** Crash containment, clean CPU accounting, no shared decoder state,
   no symbol clashes between backends.
3. **No colour conversion in the harness.** Compare in the stream's own planar format.
4. **Same targets for everyone.** Seek indices come from a seeded SplitMix64; the seed is in
   `meta.json`.
5. **Corpus files are never committed.** `corpus/manifest.toml` is; it must list every file with
   its set, codec and provenance so a run is reproducible from the scripts.
6. **Clean-room for `backend-own`** if you want the result to be shippable alongside EffectCraft:
   specs and conformance vectors only, ffmpeg's output only as a number in the report.

## Adding a backend

Implement `harness_core::Decoder` (`info`, `next_frame`, `seek`) and expose a
`BackendDesc { name, open }`. Register it in `crates/codec-bench/src/backends.rs` behind a
cargo feature. `seek(i)` must position so that the next `next_frame` is frame `i` exactly; if the
decoder can only land on keyframes, decode forward inside `seek` or `next_frame`, because the
seek scenario times `seek` + one `next_frame` together.

Frame index semantics: index `i` is the i-th picture in presentation order. For the ffmpeg CLI
that is the i-th frame out of `-fps_mode passthrough`; for FilmCraft it is the frame displayed at
`(i + ¼) / fps`. On constant-frame-rate files these agree. On variable-frame-rate files they
don't, which is why `h264_720p_vfr.mp4` is in the corpus: expect "differs" there and decide what
you want index to mean before trusting any VFR number.

## Pinning

FilmCraft is a git dependency pinned to one revision in the workspace `Cargo.toml`: `cee1feb`
(upstream main, 2026-10-13), 1643 commits past the `2c5ba7a` EffectCraft pins. The bump cost
nothing here (this harness touches only `open_bytes`, `MediaInfo.video`, `video_frame` and the
frame enums) and fixed the HEVC seek failure. EffectCraft's hardware-decode PR records what the
same bump broke for a larger surface (timestamp rounding, VP9 frame threading, renamed fields).
Bump on purpose, re-run the fixtures set with `--hash`, and keep the previous `runs/` directory
to diff. A local checkout at `../filmcraft` can be used through the `[patch]` block.

## Conformance hashes

Each suite checks correctness its own way:

- JVT (H.264): decoded YUV files to compare whole, or MD5s of them.
- JCT-VC (HEVC): per-picture MD5/CRC/checksum in decoded-picture-hash SEI messages.
- libvpx (VP9) and AOM (AV1): per-frame MD5 of the decoder's frame buffer in `.md5` companions.

codec-bench's per-frame MD5 is over tightly packed planes, so it matches none of these directly.
Wiring a suite in means a small adapter per suite (read its reference, hash our frame the way it
does). Until then, the `ffmpeg` backend's output is the reference, and `compare` gives PSNR for
near-misses.

## Hardware decode

The comparison switchblade needs is VideoToolbox through libav versus VideoToolbox through
FilmCraft's `filmcraft-platform`: `libav-vt` (in the `libav` feature) and `filmcraft-hw` (its
own feature; `filmcraft-platform` holds the FFI). The hardware switch in FilmCraft is
process-global and read when a stream's decoder is created, so the backend sets it before every
request; `compare` runs two backends in one process. Report software and hardware backends as
separate columns; never average them. Both hardware backends download every frame to CPU planes
so it can be hashed; a zero-copy texture path would be faster in an app and is out of scope.

## Seek truth

A seek's correct answer is the frame the same reference produces by decoding sequentially to
that index, not what the reference's own seek returns: ffmpeg's seek is wrong on RASL leading
pictures of open-GOP HEVC (frames 246–249 before a CRA at 250 come back as the CRA). The seek
table therefore needs a `decode` run with `--hash` over the whole file; without one it falls
back to the reference's seek and marks the cell `~`.

## Next steps, in order

1. ~~`libav` backend (port `SeekablePlayer`)~~ done; `libav-vt` too.
2. Conformance adapters for libvpx/AOM `.md5` files (the easy ones), then JCT-VC SEI hashes.
3. ~~`filmcraft-hw` feature at a newer FilmCraft pin~~ done.
4. A PSNR column in the report's seek and decode tables, so one-LSB differences (ProRes) read as
   near-exact rather than wrong; needs frames or a second hash of the frame quantised by one bit.
5. A `library` set from real clips (phone VFR, HDR, iCloud placeholders) with a `scripts/add-library.sh` that writes manifest rows.
6. First `backend-own` target: ProRes 422 (SMPTE RDD 36), judged against ffmpeg with PSNR ≥ 70 dB and against FilmCraft bit-exactly if you choose the same rounding.
